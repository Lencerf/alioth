use std::ffi::CStr;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use snafu::ResultExt;
use zerocopy::{FromBytes, FromZeros, Immutable, IntoBytes};

use crate::arch::layout::{DEVICE_TREE_START, KERNEL_IMAGE_START};
// use crate::arch::reg::{Pstate, Reg};
use crate::loader::{InitState, Result, error, search_initramfs_address};
use crate::mem::MemRegionEntry;
use crate::mem::mapped::RamBus;

pub fn load<P: AsRef<Path>>(
    memory: &RamBus,
    mem_regions: &[(u64, MemRegionEntry)],
    kernel: P,
    _cmdline: Option<&CStr>,
    initramfs: Option<P>,
) -> Result<InitState> {
    unimplemented!()
}
