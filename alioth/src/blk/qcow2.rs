// Copyright 2025 Google LLC
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#[cfg(test)]
#[path = "qcow2_test.rs"]
mod tests;

use alioth_macros::Layout;
use bitfield::bitfield;
use bitflags::bitflags;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::c_enum;
use crate::utils::endian::{Bu32, Bu64};

#[repr(C)]
#[derive(Debug, Clone, Default, Layout, KnownLayout, Immutable, FromBytes, IntoBytes)]
/// QCOW2 Header
///
/// [Specification](https://qemu-project.gitlab.io/qemu/interop/qcow2.html#header)
pub struct QCow2Hdr {
    pub magic: [u8; 4],
    pub version: Bu32,
    pub backing_file_offset: Bu64,
    pub backing_file_size: Bu32,
    pub cluster_bits: Bu32,
    pub size: Bu64,
    pub crypt_method: Bu32,
    pub l1_size: Bu32,
    pub l1_table_offset: Bu64,
    pub refcount_table_offset: Bu64,
    pub refcount_table_clusters: Bu32,
    pub nb_snapshots: Bu32,
    pub snapshots_offset: Bu64,
    pub incompatible_features: Bu64,
    pub compatible_features: Bu64,
    pub autoclear_features: Bu64,
    pub refcount_order: Bu32,
    pub header_length: Bu32,
    pub compression_type: QCow2Compression,
    pub padding: [u8; 7],
}

/// QCOW2 Magic Number "QFI\xfb"
pub const QCOW2_MAGIC: [u8; 4] = *b"QFI\xfb";

bitflags! {
    #[derive(Default, Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct QCow2IncompatibleFeatures: u64 {
        const DIRTY = 1 << 0;
        const CORRUPT = 1 << 1;
        const EXTERNAL_DATA = 1 << 2;
        const COMPRESSION = 1 << 3;
        const EXTERNAL_L2 = 1 << 4;
    }
}

bitflags! {
    #[derive(Default, Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct QCow2CompatibleFeatures: u64 {
        const LAZY_REFCOUNTS = 1 << 0;
    }
}

c_enum! {
    #[derive(Default, Immutable, KnownLayout, FromBytes, IntoBytes)]
    pub struct QCow2Compression(u8);
    {
        DEFLATE = 0;
        ZSTD = 1;
    }
}

bitfield! {
    /// QCOW2 L1 Table Entry
    #[derive(Copy, Clone, Default, PartialEq, Eq, Hash, KnownLayout, Immutable, FromBytes, IntoBytes)]
    #[repr(transparent)]
    pub struct QCow2L1(u64);
    impl Debug;
    pub rc1, _: 63;
    pub offset, _: 55, 9;
}

impl QCow2L1 {
    pub fn l2_offset(&self) -> u64 {
        self.offset() << 9
    }
}

bitfield! {
    #[derive(Copy, Clone, Default, PartialEq, Eq, Hash, KnownLayout, Immutable, FromBytes, IntoBytes)]
    #[repr(transparent)]
    pub struct QCow2L2(u64);
    impl Debug;
    pub desc, _: 61, 0;
    pub compressed, _: 62;
    pub rc1, _: 63;
}

bitfield! {
    #[derive(Copy, Clone, Default, PartialEq, Eq, Hash, KnownLayout, Immutable, FromBytes, IntoBytes)]
    #[repr(transparent)]
    pub struct QCow2StdDesc(u64);
    impl Debug;
    pub offset, _: 55, 9;
    pub zero, _: 0;
}

impl QCow2StdDesc {
    pub fn cluster_offset(&self) -> u64 {
        self.offset() << 9
    }
}
