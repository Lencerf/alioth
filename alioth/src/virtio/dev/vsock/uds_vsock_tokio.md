# vsock 设备 Tokio 异步后端设计文档

## 背景

Alioth 是一个用 Rust 编写的 Type-2 hypervisor。其 virtio 设备当前由
[mio](https://docs.rs/mio)（epoll/kqueue 封装）驱动，每个设备运行在独立的 OS 线程上，
通过单线程 event loop 处理 I/O。

对于 virtio-net 这类设备，工作模式简单——在 virtqueue 和 tap 设备之间搬运数据包——
单线程 mio event loop 完全足够。但对于 **virtio-vsock**，情况复杂得多：

- 需要管理一个 **UDS listener**（接受新连接）
- 每个连接需要同时处理 **TX**（guest → host）和 **RX**（host → guest）双向数据搬运
- 每个连接有独立的状态机：`Requested → Established → Shutdown`
- 需要手动维护 `Token → fd` 映射表，在 event loop 中做 token dispatch
- 需要在 `WouldBlock` 时手动暂停/恢复 I/O

引入 async Rust + Tokio 可以让我们用线性代码表达每个连接的生命周期，
用 `tokio::select!` 处理双向并发 I/O，用 `tokio::spawn` 管理并发连接。

## 核心挑战：Tokio runtime 内外代码的协作

VCPU 线程必须是真实的 OS 线程（不能被 Tokio 管理），但它们必须能通知设备线程
有新的工作（如 guest 踢了 virtqueue）。现有的通信机制是：

```
VCPU 线程                              Device 线程
  │                                      │
  ├── event_tx.send(WakeEvent) ────────▶  flume channel（数据通道）
  │                                      │
  └── notifier.notify() ───────────────▶  eventfd/kqueue → mio::Poll 被唤醒（信号通道）
```

这里有两条通道：flume 传数据，Notifier 传信号。两者缺一不可——

**对于 mio/io_uring**：它们的 poll 机制是纯 fd-based 的。flume 内部虽然用 eventfd
做信号，但不暴露其 fd。所以必须额外有一个 Notifier 作为"唤醒 fd"，让 poll loop 知道
有新消息到达。

**对于 Tokio**：它的 reactor 有 waker 机制。flume 的 `recv_async()` 在内部注册 waker，
`send()` 时直接调用 waker 唤醒 task。不需要任何额外的 fd。Notifier 是多余的。

这引出了核心设计问题：**如何让同一个 `Backend` trait 同时支持"需要 Notifier"的
mio/io_uring 和"不需要 Notifier"的 Tokio？**

## 方案：`Option<Notifier>`

将 `Backend::register_notifier` 的返回类型从 `Arc<Notifier>` 改为 `Option<Arc<Notifier>>`：

```rust
pub trait Backend<D: Virtio>: Send + 'static {
    /// 返回一个可选的 Notifier。
    /// - mio/io_uring: Some(notifier) — VCPU 通过 eventfd/kqueue 唤醒设备线程
    /// - tokio:         None           — flume recv_async() 自动唤醒，不需要额外 fd
    fn register_notifier(&mut self, token: u64) -> Result<Option<Arc<Notifier>>>;
}
```

调用侧（VCPU 线程中的 `wake_up_dev`）：

```rust
self.event_tx.send(event);
if let Some(n) = &self.notifier {
    n.notify();  // mio/io_uring 需要，tokio 跳过
}
```

Tokio 设备线程不再依赖 Notifier，直接通过 flume 唤醒：

```rust
// TokioBackend::event_loop
rt.block_on(async {
    loop {
        match event_rx.recv_async().await {
            Ok(WakeEvent::Notify { q_index }) => dev.handle_queue(q_index, &mut active).await?,
            Ok(WakeEvent::Shutdown) => return Ok(()),
            // ...
        }
    }
})
```

这个方案的好处：
- **不引入 dummy 实现**：Tokio 不假装需要 Notifier
- **不引入间接层**：不需要 mio::Poll → kqueue dup → AsyncFd 的绕路
- **语义清晰**：`Option` 精确表达了"某些后端需要，某些不需要"

## 最终设计

### 架构概览

```
VCPU Thread                              Device Thread
  │                                         │
  │  event_tx.send(WakeEvent) ────────────▶  │  flume channel
  │                                         │
  │  if notifier.is_some():                 │
  │      notifier.notify() ───────────────▶  │  eventfd/kqueue (mio only)
  │                                         │
                                            ▼
                               ┌─────────────────────────┐
                               │  match worker_api {      │
                               │    Mio    → mio loop     │
                               │    Tokio  → tokio loop   │
                               │  }                       │
                               └─────────────────────────┘
```

### 连接生命周期（Tokio 路径）

每个 vsock 连接对应一个 spawned async task，整个生命周期用线性代码表达：

```rust
tokio::spawn(async move {
    let (reader, writer) = stream.into_split();

    // host socket → guest RX queue
    let read_task = async {
        loop {
            let n = reader.read(&mut buf).await?;
            rx_data_tx.send(data);   // 通知主循环写入 RX virtqueue
            rx_notify.notify_one();
        }
    };

    // guest TX queue → host socket
    let write_task = async {
        while let Ok(data) = tx_data_rx.recv_async().await {
            writer.write_all(&data).await?;
        }
    };

    // 任一方向结束则整体结束
    tokio::select! { _ = read_task => {}, _ = write_task => {} }

    // 自动清理连接状态
    shared.connections.lock().remove(&(host_port, guest_port));
});
```

### 关键类型

```rust
/// 共享状态，供主事件循环和 spawned task 共同访问
struct VsockShared {
    connections: Mutex<HashMap<(u32, u32), ConnHandle>>,
    host_ports:   Mutex<HashMap<u32, u32>>,
    next_port:    Mutex<u32>,
}

/// 每个连接的 handle。只存 sender 端——receiver 直接 move 进 spawned task
#[derive(Clone)]
struct ConnHandle {
    tx_data_tx: flume::Sender<Vec<u8>>,              // 主循环 → 写入 task
    rx_data_tx: flume::Sender<VsockHeaderAndData>,    // 读取 task → 主循环
    rx_notify:  Arc<tokio::sync::Notify>,             // 有新 RX 数据通知主循环
}
```

### 与 mio 版本的对比

| 关注点 | mio | tokio |
|--------|-----|-------|
| 连接生命周期 | 分散在 5 个方法中，靠 `HashMap<Token, ...>` 维系 | 1 个 async task，线性表达 |
| 双向 I/O | 交替处理 | `tokio::select!` 并发 |
| 唤醒机制 | `Notifier` (eventfd/kqueue) | flume waker（零额外 fd） |
| 配置 | `api = "mio"`（默认） | `api = "tokio"` |

### 配置示例

```bash
# mio（默认）
--vsock uds,cid=3,path=vsock.sock

# tokio
--vsock uds,cid=3,path=vsock.sock,api=tokio
```

### 文件清单

| 文件 | 角色 |
|------|------|
| `virtio/worker/tokio.rs` | `VirtioTokio` trait + `TokioBackend` |
| `virtio/worker/mio.rs` | `VirtioMio` trait（未改动核心逻辑） |
| `virtio/worker/worker.rs` | `WorkerApi` 新增 `Tokio` 变体 |
| `virtio/dev/dev.rs` | `Backend::register_notifier` → `Option<Notifier>` |
| `virtio/pci.rs` | 条件 notify |
| `virtio/dev/vsock/uds_vsock.rs` | 合并后的 vsock，同时实现 `VirtioMio` + `VirtioTokio` |
