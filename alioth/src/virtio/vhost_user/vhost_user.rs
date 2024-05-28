use std::io::{self, IoSlice, IoSliceMut, Read, Write};
use std::mem::size_of;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::ptr::null_mut;

use bitfield::bitfield;
use bitflags::bitflags;
use libc::CMSG_SPACE;
use zerocopy::{transmute, AsBytes, FromBytes, FromZeroes};

use crate::ffi;
use crate::virtio::{Error, Result};

bitflags! {
    #[derive(Default, Debug, Clone, Copy, PartialEq, Eq, Hash)]
    #[repr(transparent)]
    pub struct VhostFeature: u64 {
        const MQ = 1 << 0;
        const LOG_SHMFD = 1 << 1;
        const RARP = 1 << 2;
        const REPLY_ACK = 1 << 3;
        const MTU = 1 << 4;
        const BACKEND_REQ = 1 << 5;
        const CROSS_ENDIAN = 1 << 6;
        const CRYPTO_SESSION = 1 << 7;
        const PAGEFAULT = 1 << 8;
        const CONFIG = 1 << 9;
        const BACKEND_SEND_FD = 1 << 10;
        const HOST_NOTIFIER = 1 << 11;
        const INFLIGHT_SHMFD = 1 << 12;
        const RESET_DEVICE = 1 << 13;
        const INBAND_NOTIFICATIONS = 1 << 14;
        const CONFIGURE_MEM_SLOTS = 1 << 15;
        const STATUS = 1 << 16;
        const XEN_MMAP = 1 << 17;
        const SHARED_OBJECT = 1 << 18;
        const DEVICE_STATE = 1 << 19;
    }
}

pub const VHOST_USER_GET_FEATURES: u32 = 1;
pub const VHOST_USER_SET_FEATURES: u32 = 2;
pub const VHOST_USER_SET_OWNER: u32 = 3;
pub const VHOST_USER_RESET_OWNER: u32 = 4;
pub const VHOST_USER_SET_MEM_TABLE: u32 = 5;
pub const VHOST_USER_SET_LOG_BASE: u32 = 6;
pub const VHOST_USER_SET_LOG_FD: u32 = 7;
pub const VHOST_USER_SET_VRING_NUM: u32 = 8;
pub const VHOST_USER_SET_VRING_ADDR: u32 = 9;
pub const VHOST_USER_SET_VRING_BASE: u32 = 10;
pub const VHOST_USER_GET_VRING_BASE: u32 = 11;
pub const VHOST_USER_SET_VRING_KICK: u32 = 12;
pub const VHOST_USER_SET_VRING_CALL: u32 = 13;
pub const VHOST_USER_SET_VRING_ERR: u32 = 14;
pub const VHOST_USER_GET_PROTOCOL_FEATURES: u32 = 15;
pub const VHOST_USER_SET_PROTOCOL_FEATURES: u32 = 16;
pub const VHOST_USER_GET_QUEUE_NUM: u32 = 17;
pub const VHOST_USER_SET_VRING_ENABLE: u32 = 18;
pub const VHOST_USER_SEND_RARP: u32 = 19;
pub const VHOST_USER_NET_SET_MTU: u32 = 20;
pub const VHOST_USER_SET_BACKEND_REQ_FD: u32 = 21;
pub const VHOST_USER_IOTLB_MSG: u32 = 22;
pub const VHOST_USER_SET_VRING_ENDIAN: u32 = 23;
pub const VHOST_USER_GET_CONFIG: u32 = 24;
pub const VHOST_USER_SET_CONFIG: u32 = 25;
pub const VHOST_USER_CREATE_CRYPTO_SESSION: u32 = 26;
pub const VHOST_USER_CLOSE_CRYPTO_SESSION: u32 = 27;
pub const VHOST_USER_POSTCOPY_ADVISE: u32 = 28;
pub const VHOST_USER_POSTCOPY_LISTEN: u32 = 29;
pub const VHOST_USER_POSTCOPY_END: u32 = 30;
pub const VHOST_USER_GET_INFLIGHT_FD: u32 = 31;
pub const VHOST_USER_SET_INFLIGHT_FD: u32 = 32;
pub const VHOST_USER_GPU_SET_SOCKET: u32 = 33;
pub const VHOST_USER_RESET_DEVICE: u32 = 34;
pub const VHOST_USER_GET_MAX_MEM_SLOTS: u32 = 36;
pub const VHOST_USER_ADD_MEM_REG: u32 = 37;
pub const VHOST_USER_REM_MEM_REG: u32 = 38;
pub const VHOST_USER_SET_STATUS: u32 = 39;
pub const VHOST_USER_GET_STATUS: u32 = 40;
pub const VHOST_USER_GET_SHARED_OBJECT: u32 = 41;
pub const VHOST_USER_SET_DEVICE_STATE_FD: u32 = 42;
pub const VHOST_USER_CHECK_DEVICE_STATE: u32 = 43;

bitfield! {
    #[derive(Copy, Clone, Default, AsBytes, FromBytes, FromZeroes)]
    #[repr(transparent)]
    pub struct MessageFlag(u32);
    impl Debug;
    need_reply, set_need_reply: 3;
    reply, set_reply: 2;
    version, set_version: 1, 0;
}

impl MessageFlag {
    pub const VERSION_1: u32 = 0x1;
    pub const REPLY: u32 = 1 << 2;
    pub const NEED_REPLY: u32 = 1 << 3;
    pub const fn sender() -> Self {
        MessageFlag(MessageFlag::VERSION_1 | MessageFlag::NEED_REPLY)
    }
}

// #[derive(AsBytes, FromBytes, FromZeroes)]
#[repr(C)]
pub union Payload {
    pub none: [u32; 4],
    pub u64: [u32; 2],
}

#[derive(Debug, AsBytes, FromBytes, FromZeroes)]
#[repr(C)]
pub struct VirtqState {
    pub index: u32,
    pub num: u32,
}

#[derive(Debug, AsBytes, FromBytes, FromZeroes)]
#[repr(C)]
pub struct VirtqAddr {
    pub index: u32,
    pub flags: u32,
    pub desc_user_addr: u64,
    pub used_user_addr: u64,
    pub avail_user_addr: u64,
    pub log_guest_addr: u64,
}

#[derive(Debug, AsBytes, FromBytes, FromZeroes)]
#[repr(C)]
pub struct MemoryRegion {
    pub guest_phys_addr: u64,
    pub memory_size: u64,
    pub userspace_addr: u64,
    pub mmap_offset: u64,
}

#[derive(Debug, AsBytes, FromBytes, FromZeroes)]
#[repr(C)]
pub struct MemoryOneRegion {
    pub _padding: u64,
    pub region: MemoryRegion,
}

#[derive(Debug, AsBytes, FromBytes, FromZeroes)]
#[repr(C)]
pub struct DeviceConfig {
    pub offset: u32,
    pub size: u32,
    pub flags: u32,
    pub region: [u8; 256],
}

#[derive(AsBytes, FromBytes, FromZeroes)]
#[repr(C)]
pub struct Message {
    pub request: u32,
    pub flag: MessageFlag,
    pub size: u32,
}

#[derive(Debug)]
pub struct VhostUserDev {
    conn: UnixStream,
}

macro_rules! impl_recv_reply {
    ($rq:expr, ()) => {{
        let mut resp = Message::new_zeroed();
        (&self.conn).read(resp.as_bytes_mut())?;
        if resp.request != $rq {
            return Err(Error::InvalidVhostRespMsg);
        } else if resp.size != 0 {
            println!("get resp size = {}", resp.size);
            return Err(Error::InvalidVhostRespPayloadSize);
        } else {
            Ok(())
        }
    }};
    ($rq:expr, $ty_rsp:ident) => {{
        let mut resp = Message::new_zeroed();
        let mut payload = $ty_rsp::new_zeroed();
        (&self.conn).read_vectored(&mut [
            IoSliceMut::new(resp.as_bytes_mut()),
            IoSliceMut::new(payload.as_bytes_mut()),
        ])?;
        if resp.request != $rq {
            Err(Error::InvalidVhostRespMsg);
        } else if resp.size != size_of::<$ty_rsp>() as u32 {
            Err(Error::InvalidVhostRespPayloadSize);
        } else {
            Ok(payload)
        }
    }};
}

unsafe fn add_fd_to_uds_msg(msg: &libc::msghdr, fd: RawFd) {
    let cmsg = libc::cmsghdr {
        cmsg_level: libc::SOL_SOCKET,
        cmsg_type: libc::SCM_RIGHTS,
        cmsg_len: unsafe { libc::CMSG_LEN(size_of::<RawFd>() as _) } as _,
    };
    let cmsg_ptr = unsafe { libc::CMSG_FIRSTHDR(msg) };
    unsafe { std::ptr::write_unaligned(cmsg_ptr, cmsg) };
    unsafe { std::ptr::write_unaligned(libc::CMSG_DATA(cmsg_ptr) as *mut _, fd) };
}

macro_rules! impl_send_msg {
    ($rq:expr, $ty_req:ident, $fd:expr) => {
        let vhost_msg = Message {
            request: $rq,
            flag: MessageFlag::sender(),
            size: size_of::<$ry_req>() as u32,
        };
        let bufs = [
            IoSlice::new(vhost_msg.as_bytes()),
            IoSlice::new(payload.as_bytes()),
        ];
        let mut cmsg_buf = [0u8; unsafe { libc::CMSG_SPACE(size_of::<RawFd>() as _) } as _];
        let uds_msg = libc::msghdr {
            msg_name: null_mut(),
            msg_namelen: 0,
            msg_iov: bufs.as_ptr() as _,
            msg_iovlen: bufs.len(),
            msg_control: cmsg_buf.as_mut_ptr() as _,
            msg_controllen: cmsg_buf.len(),
            msg_flags: 0,
        };
        add_fd_to_uds_msg(&uds_msg, $fd);
        ffi!(unsafe { libc::sendmsg(self.conn.as_raw_fd(), &uds_msg, 0) })?;
    };
}

macro_rules! impl_request_with_fd {
    ($method:ident, $rq:expr, $ty_req:ident, $ty_resp:ident) => {
        pub fn $method(&self, payload: &$ty_req, fd: RawFd) -> Result<$ty_resp> {
            let vhost_msg = Message {
                request: $rq,
                flag: MessageFlag::sender(),
                size: size_of::<$ty_req>() as u32,
            };
            let bufs = [
                IoSlice::new(vhost_msg.as_bytes()),
                IoSlice::new(payload.as_bytes()),
            ];
            let mut cmsg_buf = [0u8; unsafe { libc::CMSG_SPACE(size_of::<RawFd>() as _) } as _];
            let uds_msg = libc::msghdr {
                msg_name: null_mut(),
                msg_namelen: 0,
                msg_iov: bufs.as_ptr() as _,
                msg_iovlen: bufs.len(),
                msg_control: cmsg_buf.as_mut_ptr() as _,
                msg_controllen: cmsg_buf.len(),
                msg_flags: 0,
            };
            let cmsg_ptr = unsafe { libc::CMSG_FIRSTHDR(&uds_msg) };
            let cmsg = libc::cmsghdr {
                cmsg_level: libc::SOL_SOCKET,
                cmsg_type: libc::SCM_RIGHTS,
                cmsg_len: unsafe { libc::CMSG_LEN(size_of::<RawFd>() as _) } as _,
            };
            unsafe { std::ptr::write_unaligned(cmsg_ptr, cmsg) };
            unsafe { std::ptr::write_unaligned(libc::CMSG_DATA(cmsg_ptr) as *mut _, fd) };
            ffi!(unsafe { libc::sendmsg(self.conn.as_raw_fd(), &uds_msg, 0) })?;

            let mut resp = Message::new_zeroed();
            let mut payload = $ty_resp::new_zeroed();
            (&self.conn).read_vectored(&mut [
                IoSliceMut::new(resp.as_bytes_mut()),
                IoSliceMut::new(payload.as_bytes_mut()),
            ])?;
            if resp.request != $rq {
                return Err(Error::InvalidVhostRespMsg);
            }
            if resp.size != size_of::<$ty_resp>() as u32 {
                return Err(Error::InvalidVhostRespPayloadSize);
            }
            Ok(payload)
        }
    };

    ($method:ident, $rq:expr, $ty_req:ident, ()) => {
        pub fn $method(&self, payload: &$ty_req, fd: RawFd) -> Result<()> {
            let vhost_msg = Message {
                request: $rq,
                flag: MessageFlag::sender(),
                size: size_of::<$ty_req>() as u32,
            };
            let bufs = [
                IoSlice::new(vhost_msg.as_bytes()),
                IoSlice::new(payload.as_bytes()),
            ];
            let mut cmsg_buf = [0u8; unsafe { libc::CMSG_SPACE(size_of::<RawFd>() as _) } as _];
            let uds_msg = libc::msghdr {
                msg_name: null_mut(),
                msg_namelen: 0,
                msg_iov: bufs.as_ptr() as _,
                msg_iovlen: bufs.len(),
                msg_control: cmsg_buf.as_mut_ptr() as _,
                msg_controllen: cmsg_buf.len(),
                msg_flags: 0,
            };
            let cmsg_ptr = unsafe { libc::CMSG_FIRSTHDR(&uds_msg) };
            let cmsg = libc::cmsghdr {
                cmsg_level: libc::SOL_SOCKET,
                cmsg_type: libc::SCM_RIGHTS,
                cmsg_len: unsafe { libc::CMSG_LEN(size_of::<RawFd>() as _) } as _,
            };
            unsafe { std::ptr::write_unaligned(cmsg_ptr, cmsg) };
            unsafe { std::ptr::write_unaligned(libc::CMSG_DATA(cmsg_ptr) as *mut _, fd) };
            ffi!(unsafe { libc::sendmsg(self.conn.as_raw_fd(), &uds_msg, 0) })?;

            let ret: u64 = self.receive($rq)?;
            if ret != 0 {
                Err(Error::VhostBackendErrCode(ret))
            } else {
                Ok(())
            }
        }
    };
}

macro_rules! impl_request {
    ($method:ident, $rq:expr, (), ()) => {
        pub fn $method(&self) -> Result<()> {
            let msg = Message {
                request: $rq,
                flag: MessageFlag::sender(),
                size: 0,
            };
            (&self.conn).write(msg.as_bytes())?;
            let ret: u64 = self.receive($rq)?;
            if ret != 0 {
                Err(Error::VhostBackendErrCode(ret))
            } else {
                Ok(())
            }
        }
    };
    ($method:ident, $rq:expr, $ty_req:ident, ()) => {
        pub fn $method(&self, payload: &$ty_req) -> Result<()> {
            let msg = Message {
                request: $rq,
                flag: MessageFlag::sender(),
                size: size_of::<$ty_req>() as u32,
            };
            (&self.conn).write_vectored(&[
                IoSlice::new(msg.as_bytes()),
                IoSlice::new(payload.as_bytes()),
            ])?;
            let ret: u64 = self.receive($rq)?;
            if ret != 0 {
                Err(Error::VhostBackendErrCode(ret))
            } else {
                Ok(())
            }
        }
    };
    ($method:ident, $rq:expr, (), $ty_resp:ident) => {
        pub fn $method(&self) -> Result<$ty_resp> {
            let msg = Message {
                request: $rq,
                flag: MessageFlag::sender(),
                size: 0,
            };
            (&self.conn).write(msg.as_bytes())?;
            self.receive($rq)
        }
    };
    ($method:ident, $rq:expr, $ty_req:ident, $ty_resp:ident) => {
        pub fn $method(&self, payload: &$ty_req) -> Result<$ty_resp> {
            let msg = Message {
                request: $rq,
                flag: MessageFlag::sender(),
                size: size_of::<$ty_req>() as u32,
            };
            (&self.conn).write_vectored(&[
                IoSlice::new(msg.as_bytes()),
                IoSlice::new(payload.as_bytes()),
            ])?;
            self.receive($rq)
        }
    };
}

impl VhostUserDev {
    pub fn new<P: AsRef<Path>>(sock: P) -> Result<Self> {
        Ok(VhostUserDev {
            conn: UnixStream::connect(sock)?,
        })
    }

    fn receive<T: FromBytes + AsBytes>(&self, req: u32) -> Result<T> {
        let mut resp = Message::new_zeroed();
        let mut payload = T::new_zeroed();
        (&self.conn).read_vectored(&mut [
            IoSliceMut::new(resp.as_bytes_mut()),
            IoSliceMut::new(payload.as_bytes_mut()),
        ])?;
        if resp.request != req {
            Err(Error::InvalidVhostRespMsg(req, resp.request))
        } else if resp.size != size_of::<T>() as u32 {
            Err(Error::InvalidVhostRespPayloadSize(
                size_of::<T>(),
                resp.size,
            ))
        } else {
            Ok(payload)
        }
    }

    impl_request!(get_features, VHOST_USER_GET_FEATURES, (), u64);
    impl_request!(set_features, VHOST_USER_SET_FEATURES, u64, ());
    impl_request!(
        get_protocol_features,
        VHOST_USER_GET_PROTOCOL_FEATURES,
        (),
        u64
    );
    impl_request!(
        set_protocol_features,
        VHOST_USER_SET_PROTOCOL_FEATURES,
        u64,
        u64
    );

    impl_request!(set_owner, VHOST_USER_SET_OWNER, (), ());

    impl_request!(set_vring_num, VHOST_USER_SET_VRING_NUM, VirtqState, ());

    impl_request!(set_vring_addr, VHOST_USER_SET_VRING_ADDR, VirtqAddr, ());

    impl_request!(set_vring_base, VHOST_USER_SET_VRING_BASE, VirtqState, ());

    impl_request!(
        get_config,
        VHOST_USER_GET_CONFIG,
        DeviceConfig,
        DeviceConfig
    );

    impl_request!(
        get_virtq_base,
        VHOST_USER_GET_VRING_BASE,
        VirtqState,
        VirtqState
    );

    impl_request!(get_queue_num, VHOST_USER_GET_QUEUE_NUM, (), u64);

    impl_request_with_fd!(set_vring_kick, VHOST_USER_SET_VRING_KICK, u64, ());
    impl_request_with_fd!(set_vring_call, VHOST_USER_SET_VRING_CALL, u64, ());
    impl_request!(
        set_vring_enable,
        VHOST_USER_SET_VRING_ENABLE,
        VirtqState,
        ()
    );
    impl_request!(set_status, VHOST_USER_SET_STATUS, u64, ());
    impl_request!(get_status, VHOST_USER_GET_STATUS, (), u64);
    impl_request_with_fd!(add_mem_region, VHOST_USER_ADD_MEM_REG, MemoryOneRegion, ());

    // pub fn get_vring_kick(&self, payload: &VirtqState, fd: RawFd) -> Result<VirtqState> {
    //     let vhost_msg = Message {
    //         request: VHOST_USER_GET_VRING_BASE,
    //         flag: MessageFlag::sender(),
    //         size: size_of::<VirtqState>() as u32,
    //     };
    //     let bufs = [
    //         IoSlice::new(vhost_msg.as_bytes()),
    //         IoSlice::new(payload.as_bytes()),
    //     ];
    //     let mut cmsg_buf = [0u8; unsafe { libc::CMSG_SPACE(size_of::<RawFd>() as _) } as _];
    //     let uds_msg = libc::msghdr {
    //         msg_name: null_mut(),
    //         msg_namelen: 0,
    //         msg_iov: bufs.as_ptr() as _,
    //         msg_iovlen: bufs.len(),
    //         msg_control: cmsg_buf.as_mut_ptr() as _,
    //         msg_controllen: cmsg_buf.len(),
    //         msg_flags: 0,
    //     };
    //     let cmsg_ptr = unsafe { libc::CMSG_FIRSTHDR(&uds_msg) };
    //     let cmsg = libc::cmsghdr {
    //         cmsg_level: libc::SOL_SOCKET,
    //         cmsg_type: libc::SCM_RIGHTS,
    //         cmsg_len: unsafe { libc::CMSG_LEN(size_of::<RawFd>() as _) } as _,
    //     };
    //     unsafe { std::ptr::write_unaligned(cmsg_ptr, cmsg) };
    //     unsafe { std::ptr::write_unaligned(libc::CMSG_DATA(cmsg_ptr) as *mut _, fd) };
    //     ffi!(unsafe { libc::sendmsg(self.conn.as_raw_fd(), &uds_msg, 0) })?;

    //     let mut resp = Message::new_zeroed();
    //     let mut payload = VirtqState::new_zeroed();
    //     (&self.conn).read_vectored(&mut [
    //         IoSliceMut::new(resp.as_bytes_mut()),
    //         IoSliceMut::new(payload.as_bytes_mut()),
    //     ])?;
    //     if resp.request != vhost_msg.request {
    //         return Err(Error::InvalidVhostRespMsg);
    //     }
    //     if resp.size != size_of::<VirtqState>() as u32 {
    //         return Err(Error::InvalidVhostRespPayloadSize);
    //     }
    //     Ok(payload)
    // }

    // pub fn get_virtq_base(&self, payload: &VirtqState) -> Result<VirtqState> {
    //     let msg = Message {
    //         request: VHOST_USER_GET_VRING_BASE,
    //         flag: MessageFlag::sender(),
    //         size: size_of::<VirtqState>() as u32,
    //     };
    //     (&self.conn).write_vectored(&[
    //         IoSlice::new(msg.as_bytes()),
    //         IoSlice::new(payload.as_bytes()),
    //     ])?;
    //     let mut resp = Message::new_zeroed();
    //     let mut payload = VirtqState::new_zeroed();
    //     (&self.conn).read_vectored(&mut [
    //         IoSliceMut::new(resp.as_bytes_mut()),
    //         IoSliceMut::new(payload.as_bytes_mut()),
    //     ])?;
    //     if resp.request != msg.request {
    //         return Err(Error::InvalidVhostRespMsg);
    //     }
    //     if resp.size != size_of::<VirtqState>() as u32 {
    //         return Err(Error::InvalidVhostRespPayloadSize);
    //     }
    //     Ok(payload)
    // }

    // pub fn get_features(&self) -> Result<u64> {
    //     let msg = Message {
    //         request: VHOST_USER_GET_FEATURES,
    //         flag: MessageFlag(MessageFlag::VERSION_1),
    //         size: 0,
    //     };
    //     (&self.conn).read()(&self.conn).write(&msg.as_bytes()[0..12 + msg.size as usize])?;
    //     let mut resp = Message::new_zeroed();
    //     let resp_size = (&self.conn).read(resp.as_bytes_mut())?;
    //     // Ok(transmute!(unsafe { resp.payload.u64 }))
    //     unimplemented!()
    // }
}

#[cfg(test)]
mod test {
    use std::io::{Read, Write};
    use std::mem::{size_of, size_of_val};
    use std::os::fd::RawFd;
    use std::os::unix::net::UnixStream;

    use zerocopy::{transmute, AsBytes, FromZeroes};

    use crate::virtio::vhost_user::{
        DeviceConfig, MessageFlag, VhostFeature, VHOST_USER_GET_PROTOCOL_FEATURES,
    };
    use crate::virtio::VirtioFeature;

    use super::{Message, Payload, VhostUserDev, VHOST_USER_GET_FEATURES};

    #[test]
    fn test_feature() {
        println!("{:x?}", VhostFeature::from_bits_retain(0x8629));
    }

    #[test]
    fn test_request() {
        println!("space={}", unsafe {
            libc::CMSG_SPACE(size_of::<RawFd>() as _)
        });
        println!(
            "virtio feat = {:?}",
            VirtioFeature::from_bits_retain(0x170000000)
        );
        let dev = VhostUserDev::new("/tmp/virtiofsd").unwrap();
        println!("feat={:x?}", dev.get_features());
        let feat = VhostFeature::from_bits_retain(dev.get_protocol_features().unwrap());
        println!("prot=feat{:x?}", feat);
        dev.set_protocol_features(&feat.bits()).unwrap();
        println!("will set oner");
        dev.set_owner().unwrap();
        println!("set owner done");
        let mut cfg = DeviceConfig::new_zeroed();
        cfg.size = size_of_val(&cfg.region) as _;
        println!("cfg = {:x?}", dev.get_config(&cfg));
        println!("queue={:x?}", dev.get_queue_num());
    }
}
