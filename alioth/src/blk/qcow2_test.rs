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

use std::fs::File;
use std::io::Read;
use std::os::unix::fs::FileExt;

use miniz_oxide::inflate::TINFLStatus;
use miniz_oxide::inflate::core::inflate_flags::TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF;
use miniz_oxide::inflate::core::{DecompressorOxide, decompress};
use zerocopy::{FromZeros, IntoBytes};

use crate::blk::qcow2::{QCow2CmprDesc, QCow2Hdr, QCow2L1, QCow2L2};
use crate::utils::endian::Bu64;

#[test]
fn test_qcow2() {
    let mut hdr = QCow2Hdr::new_zeroed();
    let mut f = File::open("/Users/lencerf/data/ubuntu-25.04-server-cloudimg-amd64.img").unwrap();
    f.read_exact(hdr.as_mut_bytes()).unwrap();
    println!("{hdr:#x?}");
    let mut l1_table = vec![Bu64::new_zeroed(); hdr.l1_size.to_ne() as usize];
    f.read_exact_at(l1_table.as_mut_bytes(), hdr.l1_table_offset.to_ne())
        .unwrap();
    println!("{l1_table:#x?}");
    let cluster_size = 1 << hdr.cluster_bits.to_ne();
    println!("cluster_size = {cluster_size}");
    let l1_0 = QCow2L1(l1_table[0].to_ne());
    let mut l2_table = vec![Bu64::new_zeroed(); cluster_size as usize / size_of::<Bu64>()];
    f.read_exact_at(l2_table.as_mut_bytes(), l1_0.l2_offset())
        .unwrap();
    // let mut l2_index = 0;
    // for (i, l2_entry) in l2_table.iter().enumerate() {
    //     let l2_entry = QCow2L2(l2_entry.to_ne());
    //     if l2_entry.compressed() || l2_entry.desc() == 0 {
    //         continue;
    //     }
    //     l2_index = i;
    //     break;
    // }
    let l2_index = 0;
    let l2_entry_0 = QCow2L2(l2_table[l2_index].to_ne());
    println!("l2_index = {l2_index}: entry = {l2_entry_0:#x?}");
    println!("virtual offset = {}", cluster_size * l2_index);
    assert!(l2_entry_0.compressed());

    let l2_desc = QCow2CmprDesc(l2_entry_0.desc());
    let (offset, size) = l2_desc.offset_size(hdr.cluster_bits.to_ne());
    let mut buf = vec![0u8; size as usize];
    f.read_exact_at(&mut buf, offset).unwrap();
    println!("offset = {offset:x}, size={size:x}");
    println!("encryped: {:02x?}", &buf[..32]);
    let mut decompressed_buf = vec![0u8; cluster_size];
    let r = &mut DecompressorOxide::new();
    let (status, n_read, n_write) = decompress(
        r,
        buf.as_slice(),
        &mut decompressed_buf,
        0,
        TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF,
    );
    // let mut decoder = Decoder::new(buf.as_slice());
    // let mut decoded = vec![];
    // std::io::copy(&mut decoder, &mut decompressed_buf).unwrap();
    // println!("{:02x?}", &decompressed_buf[..32]);
    // let d = decompress_to_vec(&buf).unwrap();
    println!("nread {n_read} nwrite {n_write}");
    std::fs::write("/Users/lencerf/data/cluster0", &decompressed_buf).unwrap();
    println!("{:02x?}", &decompressed_buf[..32]);
    assert_eq!(status, TINFLStatus::Done);

    // let l2_entry_desc = QCow2StdDesc(l2_entry_0.desc());
    // println!("cluster offset = {}", l2_entry_desc.cluster_offset());
    // let mut buf = vec![0u8; cluster_size];
    // f.read_exact_at(&mut buf, l2_entry_desc.cluster_offset())
    //     .unwrap();
    // println!("{:02x?}", &buf[..32])
}
