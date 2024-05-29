mod vhost_vsock;

use serde::Deserialize;
use zerocopy::{AsBytes, FromBytes, FromZeroes};

use crate::impl_mmio_for_zerocopy;

use self::vhost_vsock::VhostVsockParam;

#[derive(Debug, Clone, Copy, Default, FromBytes, FromZeroes, AsBytes)]
#[repr(C, align(8))]
pub struct VsockConfig {
    pub guest_cid: u32,
    pub guest_cid_hi: u32,
}

impl_mmio_for_zerocopy!(VsockConfig);

#[derive(Debug, Deserialize)]
pub enum VsockParam {
    #[serde(alias = "vhost")]
    Vhost(VhostVsockParam),
}
