use std::sync::Arc;

use alioth::hv::Hypervisor;
use alioth::vm::Machine;
use axum::Router;
use axum::body::Bytes;
use axum::extract::Path;
use axum::routing::post;

pub struct Server<H>
where
    H: Hypervisor,
{
    pub vm: Arc<Machine<H>>,
}

async fn vm_pause(path: Path<String>, bytes: Bytes) {
    let Path(name) = path;
    log::info!("Pausing VM: {}", name);
    log::info!("received data: {:?}", bytes);
    log::info!("VM paused successfully");
}

async fn vm_resume(path: Path<String>, bytes: Bytes) {
    let Path(name) = path;
    log::info!("Resuming VM: {}", name);
    log::info!("received data: {:?}", bytes);
    log::info!("VM resumed successfully");
}

impl<H> Server<H>
where
    H: Hypervisor,
{
    pub fn new(vm: Machine<H>) -> Self {
        Self { vm: Arc::new(vm) }
    }

    pub fn run(self) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let app = Router::new()
                .without_v07_checks()
                .route("/vms/{name}:pause", post(vm_pause))
                .route("/vms/{name}:resume", post(vm_resume));

            let listener = tokio::net::TcpListener::bind("127.0.0.1:3000")
                .await
                .unwrap();
            let server = axum::serve(listener, app).into_future();
            let vm = self.vm.clone();
            let results = tokio::task::spawn_blocking(move || vm.wait());
            tokio::select! {
                r = server => log::info!("server stop: {:?}", r),
                r = results => log::info!("vm done: {:#x?}", r),
            };
        });
    }
}
