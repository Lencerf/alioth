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

use std::mem::size_of;

use rstest::rstest;
use zerocopy::IntoBytes;

use crate::arch::layout::{ACPI_START, KERNEL_IMAGE_START, MEM_64_START, RAM_32_START, UEFI_START};
use crate::arch::reg::MpidrEl1;
use crate::board::aarch64::{
    StaticUefiTables, create_acpi, create_dsdt, create_uefi, encode_mpidr,
};
use crate::board::{BoardSpec, CpuSpec, CpuTopology};
use crate::firmware::acpi::bindings::AcpiTableRsdp;
use crate::firmware::acpi::{GicVersion, MsiController};
use crate::firmware::uefi::{EfiMemoryAttribute, EfiMemoryType};
use crate::mem::{MemRegionEntry, MemRegionType, MemSpec};
use crate::utils::wrapping_sum;

#[rstest]
#[case(CpuTopology{smt: false, cores: 1, sockets: 1, ..Default::default()}, 1, 1)]
#[case(CpuTopology{smt: true, cores: 8, sockets: 1, thread_contiguous: false}, 8, 1)]
#[case(CpuTopology{smt: true, cores: 8, sockets: 4, thread_contiguous: false}, 45, (1 << 16) | (5 << 8) | 1)]
fn test_encode_mpidr(#[case] topology: CpuTopology, #[case] index: u16, #[case] mpidr: u64) {
    assert_eq!(encode_mpidr(&topology, index), MpidrEl1(mpidr));
}

#[test]
fn test_create_acpi_and_uefi() {
    let spec = BoardSpec {
        mem: MemSpec {
            size: 4 << 30,
            ..Default::default()
        },
        cpu: CpuSpec {
            count: 4,
            topology: CpuTopology {
                smt: true,
                cores: 2,
                sockets: 1,
                thread_contiguous: false,
            },
        },
        coco: None,
        acpi: true,
    };

    let dsdt = create_dsdt(&spec);
    assert_eq!(wrapping_sum(&dsdt), 0);
    assert_eq!(dsdt.len(), 363 + 4 * 42);

    for (gic, msi) in [
        (GicVersion::V3, Some(MsiController::Its)),
        (GicVersion::V3, Some(MsiController::GicV2m)),
        (GicVersion::V2, Some(MsiController::GicV2m)),
    ] {
        let mut acpi = create_acpi(&spec, gic, msi);
        acpi.relocate(ACPI_START + size_of::<AcpiTableRsdp>() as u64);
        acpi.update_checksums();
        assert_eq!(wrapping_sum(&acpi.rsdp().as_bytes()[..20]), 0);
        assert_eq!(wrapping_sum(acpi.rsdp().as_bytes()), 0);
        for &(offset, len) in &acpi.table_checksums {
            assert_eq!(wrapping_sum(&acpi.tables()[offset..(offset + len)]), 0);
        }
    }

    let acpi_size = KERNEL_IMAGE_START - RAM_32_START;
    let mem_regions = [
        (
            0x3000_0000,
            MemRegionEntry {
                size: 256 << 20,
                type_: MemRegionType::Reserved,
            },
        ),
        (
            RAM_32_START,
            MemRegionEntry {
                size: acpi_size,
                type_: MemRegionType::Acpi,
            },
        ),
        (
            KERNEL_IMAGE_START,
            MemRegionEntry {
                size: (2 << 30) - acpi_size,
                type_: MemRegionType::Ram,
            },
        ),
        (
            MEM_64_START,
            MemRegionEntry {
                size: 2 << 30,
                type_: MemRegionType::Ram,
            },
        ),
    ];
    let (uefi_tables, mmap) = create_uefi(&mem_regions);
    assert_eq!(size_of::<StaticUefiTables>(), 192);
    assert_eq!(uefi_tables.systab.nr_tables, 2);
    assert_eq!(uefi_tables.config_tables[0].table, ACPI_START);
    assert!(uefi_tables.systab.tables > UEFI_START);
    assert_eq!(mmap.len(), 3);
    assert_eq!(mmap[0].ty, EfiMemoryType::ACPI_RECLAIM_MEMORY);
    assert_eq!(mmap[0].phys_addr, RAM_32_START);
    assert_eq!(mmap[0].attribute, EfiMemoryAttribute::WB);
    assert_eq!(mmap[1].ty, EfiMemoryType::CONVENTIONAL_MEMORY);
    assert_eq!(mmap[1].phys_addr, KERNEL_IMAGE_START);
    assert_eq!(mmap[2].ty, EfiMemoryType::CONVENTIONAL_MEMORY);
    assert_eq!(mmap[2].phys_addr, MEM_64_START);
}
