pub const VHOST_VIRTIO: u8 = 0xAF;

#[repr(C)]
#[derive(Debug, Copy, Clone, Default)]
pub struct VhostMemoryRegion {
    pub guest_phys_addr: u64,
    pub memory_size: u64,
    pub userspace_addr: u64,
    /// No flags are defined.
    pub flags_padding: u64,
}
#[repr(C)]
#[derive(Debug)]
pub struct VhostMemory<const N: usize> {
    pub nregions: u32,
    pub padding: u32,
    pub regions: [VhostMemoryRegion; N],
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct VhostVringState {
    // queue index
    pub index: u32,
    pub num: u32,
}
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct VhostVringFile {
    pub index: u32,
    pub fd: i32,
}
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct VhostVringAddr {
    pub index: u32,
    pub flags: u32,
    pub desc_user_addr: u64,
    pub used_user_addr: u64,
    pub avail_user_addr: u64,
    pub log_guest_addr: u64,
}
