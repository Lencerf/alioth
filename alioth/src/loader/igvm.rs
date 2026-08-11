// Copyright 2026 Google LLC
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

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use ::igvm::{IgvmDirectiveHeader, IgvmFile, IsolationType};
use ::igvm::snp_defs::{SevSelector, SevVmsa};
use igvm_defs::{
    IGVM_VHS_MEMORY_MAP_ENTRY, MemoryMapEntryType, IgvmPageDataType,
};
use snafu::ResultExt;

use crate::arch::msr::Msr;
use crate::arch::reg::{DtReg, DtRegVal, Reg, SReg, SegAccess, SegReg, SegRegVal};
use crate::arch::sev::SnpPageType;
use crate::hv::{CocoSpec, Vm};
use crate::loader::{Error, InitState, Result, error};
use crate::mem::{MemRegion, MemRegionEntry, MemRegionType, Memory};
use crate::mem::mapped::ArcMemPages;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AddrRange {
    start: u64,
    end: u64,
}

fn convert_selector(sel: &SevSelector) -> SegRegVal {
    SegRegVal {
        selector: sel.selector,
        base: sel.base,
        limit: sel.limit,
        access: SegAccess(sel.attrib as u32),
    }
}

fn parse_vmsa_registers(vmsa: &SevVmsa) -> InitState {
    InitState {
        regs: vec![
            (Reg::Rax, vmsa.rax),
            (Reg::Rbx, vmsa.rbx),
            (Reg::Rcx, vmsa.rcx),
            (Reg::Rdx, vmsa.rdx),
            (Reg::Rsi, vmsa.rsi),
            (Reg::Rdi, vmsa.rdi),
            (Reg::Rsp, vmsa.rsp),
            (Reg::Rbp, vmsa.rbp),
            (Reg::R8, vmsa.r8),
            (Reg::R9, vmsa.r9),
            (Reg::R10, vmsa.r10),
            (Reg::R11, vmsa.r11),
            (Reg::R12, vmsa.r12),
            (Reg::R13, vmsa.r13),
            (Reg::R14, vmsa.r14),
            (Reg::R15, vmsa.r15),
            (Reg::Rip, vmsa.rip),
            (Reg::Rflags, vmsa.rflags),
        ],
        sregs: vec![
            (SReg::Cr0, vmsa.cr0),
            (SReg::Cr3, vmsa.cr3),
            (SReg::Cr4, vmsa.cr4),
        ],
        msrs: vec![(Msr::EFER, vmsa.efer)],
        seg_regs: vec![
            (SegReg::Cs, convert_selector(&vmsa.cs)),
            (SegReg::Ds, convert_selector(&vmsa.ds)),
            (SegReg::Es, convert_selector(&vmsa.es)),
            (SegReg::Fs, convert_selector(&vmsa.fs)),
            (SegReg::Gs, convert_selector(&vmsa.gs)),
            (SegReg::Ss, convert_selector(&vmsa.ss)),
            (SegReg::Tr, convert_selector(&vmsa.tr)),
            (SegReg::Ldtr, convert_selector(&vmsa.ldtr)),
        ],
        dt_regs: vec![
            (
                DtReg::Gdtr,
                DtRegVal {
                    base: vmsa.gdtr.base,
                    limit: vmsa.gdtr.limit as u16,
                },
            ),
            (
                DtReg::Idtr,
                DtRegVal {
                    base: vmsa.idtr.base,
                    limit: vmsa.idtr.limit as u16,
                },
            ),
        ],
        initramfs: None,
    }
}

fn generate_igvm_memory_map(regions: &[(u64, MemRegionEntry)]) -> Vec<u8> {
    let mut map = Vec::new();
    for (gpa, entry) in regions {
        let entry_type = match entry.type_ {
            MemRegionType::Ram => MemoryMapEntryType::MEMORY,
            _ => MemoryMapEntryType::PLATFORM_RESERVED,
        };
        let entry_struct = IGVM_VHS_MEMORY_MAP_ENTRY {
            starting_gpa_page_number: gpa >> 12,
            number_of_pages: entry.size >> 12,
            entry_type,
            flags: 0,
            reserved: 0,
        };
        let bytes = unsafe {
            std::slice::from_raw_parts(
                &entry_struct as *const _ as *const u8,
                std::mem::size_of::<IGVM_VHS_MEMORY_MAP_ENTRY>(),
            )
        };
        map.extend_from_slice(bytes);
    }
    let null_entry = IGVM_VHS_MEMORY_MAP_ENTRY {
        starting_gpa_page_number: 0,
        number_of_pages: 0,
        entry_type: MemoryMapEntryType::MEMORY,
        flags: 0,
        reserved: 0,
    };
    let bytes = unsafe {
        std::slice::from_raw_parts(
            &null_entry as *const _ as *const u8,
            std::mem::size_of::<IGVM_VHS_MEMORY_MAP_ENTRY>(),
        )
    };
    map.extend_from_slice(bytes);
    map
}

fn write_parameter(
    parameter_areas: &mut HashMap<u32, Vec<u8>>,
    param: &igvm_defs::IGVM_VHS_PARAMETER,
    data: &[u8],
) -> Result<()> {
    let area = parameter_areas
        .get_mut(&param.parameter_area_index)
        .ok_or_else(|| {
            error::InvalidIgvm {
                err: format!("Missing parameter area {}", param.parameter_area_index),
            }
            .build()
        })?;
    let offset = param.byte_offset as usize;
    if offset + data.len() > area.len() {
        return error::InvalidIgvm {
            err: format!(
                "Parameter write out of bounds: offset={}, len={}, area_size={}",
                offset,
                data.len(),
                area.len()
            ),
        }
        .fail();
    }
    area[offset..offset + data.len()].copy_from_slice(data);
    Ok(())
}

fn overlaps_with_regions(mem_regions: &[(u64, MemRegionEntry)], start: u64, end: u64) -> bool {
    for (r_start, entry) in mem_regions {
        let r_end = r_start + entry.size;
        if start < r_end && end > *r_start {
            return true;
        }
    }
    false
}

fn default_reset_state() -> InitState {
    use crate::arch::reg::{Cr0, Rflags};
    let boot_cs = SegRegVal {
        selector: 0xf000,
        base: 0xffff0000,
        limit: 0xffff,
        access: SegAccess(0x9a),
    };
    let boot_ds = SegRegVal {
        selector: 0x0,
        base: 0x0,
        limit: 0xffff,
        access: SegAccess(0x93),
    };
    let boot_ss = SegRegVal {
        selector: 0x0,
        base: 0x0,
        limit: 0xffff,
        access: SegAccess(0x92),
    };
    let boot_tr = SegRegVal {
        selector: 0x0,
        base: 0x0,
        limit: 0xffff,
        access: SegAccess(0x83),
    };
    let boot_ldtr = SegRegVal {
        selector: 0x0,
        base: 0x0,
        limit: 0xffff,
        access: SegAccess(0x82),
    };
    InitState {
        regs: vec![
            (Reg::Rax, 0),
            (Reg::Rbx, 0),
            (Reg::Rcx, 0),
            (Reg::Rdx, 0x600),
            (Reg::Rsi, 0),
            (Reg::Rdi, 0),
            (Reg::Rsp, 0),
            (Reg::Rbp, 0),
            (Reg::R8, 0),
            (Reg::R9, 0),
            (Reg::R10, 0),
            (Reg::R11, 0),
            (Reg::R12, 0),
            (Reg::R13, 0),
            (Reg::R14, 0),
            (Reg::R15, 0),
            (Reg::Rip, 0xfff0),
            (Reg::Rflags, Rflags::RESERVED_1.bits() as u64),
        ],
        sregs: vec![
            (SReg::Cr0, (Cr0::ET | Cr0::NW | Cr0::CD).bits()),
            (SReg::Cr2, 0),
            (SReg::Cr3, 0),
            (SReg::Cr4, 0),
            (SReg::Cr8, 0),
        ],
        msrs: vec![(Msr::EFER, 0)],
        seg_regs: vec![
            (SegReg::Cs, boot_cs),
            (SegReg::Ds, boot_ds),
            (SegReg::Es, boot_ds),
            (SegReg::Fs, boot_ds),
            (SegReg::Gs, boot_ds),
            (SegReg::Ss, boot_ss),
            (SegReg::Tr, boot_tr),
            (SegReg::Ldtr, boot_ldtr),
        ],
        dt_regs: vec![
            (
                DtReg::Idtr,
                DtRegVal {
                    base: 0,
                    limit: 0xffff,
                },
            ),
            (
                DtReg::Gdtr,
                DtRegVal {
                    base: 0,
                    limit: 0xffff,
                },
            ),
        ],
        initramfs: None,
    }
}

pub fn load(
    memory: &Memory,
    mem_regions: &[(u64, MemRegionEntry)],
    path: &Path,
    vm: &impl Vm,
    coco: Option<&CocoSpec>,
    cpu_count: u16,
) -> Result<InitState, Error> {
    let bytes = std::fs::read(path).context(error::AccessFile { path })?;

    let is_snp = matches!(coco, Some(CocoSpec::AmdSnp { .. }));
    let is_standard_sev = matches!(coco, Some(CocoSpec::AmdSev { .. }));

    let igvm_file = match IgvmFile::new_from_binary(
        &bytes,
        if is_snp {
            Some(IsolationType::Snp)
        } else {
            None
        },
    ) {
        Ok(f) => f,
        Err(e) => {
            return error::InvalidIgvm {
                err: format!("Failed to parse IGVM: {:?}", e),
            }
            .fail();
        }
    };

    let mut parameter_areas: HashMap<u32, Vec<u8>> = HashMap::new();
    let mut bsp_regs = None;
    let mut current_regions = mem_regions.to_vec();
    let mut allocated_roms: Vec<(u64, ArcMemPages)> = Vec::new();

    // Pass 1: Set up parameter areas and required memory regions
    for directive in igvm_file.directives() {
        log::trace!("IGVM Directive: {:?}", directive);
        match directive {
            IgvmDirectiveHeader::ParameterArea {
                parameter_area_index,
                number_of_bytes,
                ..
            } => {
                parameter_areas.insert(*parameter_area_index, vec![0u8; *number_of_bytes as usize]);
            }
            _ => {}
        }
    }

    // Collect and merge all GPA ranges we need to write to or reserve
    let mut needed_ranges: Vec<AddrRange> = Vec::new();
    for directive in igvm_file.directives() {
        match directive {
            IgvmDirectiveHeader::PageData { gpa, .. } => {
                needed_ranges.push(AddrRange { start: *gpa, end: *gpa + 4096 });
            }
            IgvmDirectiveHeader::ParameterInsert(insert) => {
                let size = parameter_areas.get(&insert.parameter_area_index).map_or(0, |a| a.len() as u64);
                if size > 0 {
                    needed_ranges.push(AddrRange { start: insert.gpa, end: insert.gpa + size });
                }
            }
            IgvmDirectiveHeader::RequiredMemory { gpa, number_of_bytes, .. } => {
                needed_ranges.push(AddrRange { start: *gpa, end: *gpa + *number_of_bytes as u64 });
            }
            _ => {}
        }
    }

    needed_ranges.sort_by_key(|r| r.start);
    let mut merged_ranges: Vec<AddrRange> = Vec::new();
    for r in needed_ranges {
        if let Some(last) = merged_ranges.last_mut() {
            if r.start <= last.end {
                if r.end > last.end {
                    last.end = r.end;
                }
                continue;
            }
        }
        merged_ranges.push(r);
    }

    // Map missing ranges as Reserved regions
    for r in merged_ranges {
        if !overlaps_with_regions(&current_regions, r.start, r.end) {
            let size = r.end - r.start;
            log::info!("Mapping IGVM required memory region: {:#x} - {:#x} (size={})", r.start, r.end, size);
            let rom = ArcMemPages::from_anonymous(size as usize, None, None)
                .context(error::AddMemSlot)?;
            allocated_roms.push((r.start, rom.clone()));

            if is_standard_sev {
                vm.register_encrypted_range(rom.as_slice()).context(error::Hv)?;
            }

            let region = Arc::new(MemRegion::with_dev_mem(
                rom,
                MemRegionType::Reserved,
            ));
            memory.add_region(r.start, region).context(error::AddMemSlot)?;
            current_regions.push((
                r.start,
                MemRegionEntry {
                    size,
                    type_: MemRegionType::Reserved,
                },
            ));
        }
    }

    // Pass 2: Fill VMM-specific parameters into parameter areas
    for directive in igvm_file.directives() {
        match directive {
            IgvmDirectiveHeader::VpCount(param) => {
                let count = cpu_count as u32;
                write_parameter(&mut parameter_areas, param, &count.to_le_bytes())?;
            }
            IgvmDirectiveHeader::MemoryMap(param) => {
                let map = generate_igvm_memory_map(&current_regions);
                write_parameter(&mut parameter_areas, param, &map)?;
            }
            IgvmDirectiveHeader::EnvironmentInfo(param) => {
                let env_info = igvm_defs::IgvmEnvironmentInfo::new()
                    .with_memory_is_shared(true);
                let bytes = unsafe {
                    std::slice::from_raw_parts(
                        &env_info as *const _ as *const u8,
                        std::mem::size_of::<igvm_defs::IgvmEnvironmentInfo>(),
                    )
                };
                write_parameter(&mut parameter_areas, param, bytes)?;
            }
            _ => {}
        }
    }

    let ram_bus = memory.ram_bus();

    // Pass 3: Load pages to guest memory and run launch updates
    for directive in igvm_file.directives() {
        match directive {
            IgvmDirectiveHeader::PageData {
                gpa,
                data_type,
                data,
                ..
            } => {
                let mut page_data = data.clone();
                let page_type = match *data_type {
                    IgvmPageDataType::NORMAL => {
                        ram_bus.write(*gpa, &page_data)?;
                        SnpPageType::NORMAL
                    }
                    IgvmPageDataType::CPUID_DATA => {
                        ram_bus.write(*gpa, &page_data)?;
                        SnpPageType::CPUID
                    }
                    IgvmPageDataType::SECRETS => {
                        ram_bus.write(*gpa, &page_data)?;
                        SnpPageType::SECRETS
                    }
                    _ => continue,
                };

                if is_snp {
                    memory.mark_private_memory(vm, *gpa, page_data.len() as u64, true)?;
                    vm.snp_launch_update(&mut page_data, *gpa, page_type)
                        .context(error::Hv)?;
                }
            }
            IgvmDirectiveHeader::ParameterInsert(insert) => {
                let data = parameter_areas
                    .get(&insert.parameter_area_index)
                    .ok_or_else(|| {
                        error::InvalidIgvm {
                            err: format!("Missing parameter area {}", insert.parameter_area_index),
                        }
                        .build()
                    })?;
                ram_bus.write(insert.gpa, data)?;

                if is_snp {
                    let mut data_mut = data.clone();
                    memory.mark_private_memory(vm, insert.gpa, data_mut.len() as u64, true)?;
                    vm.snp_launch_update(&mut data_mut, insert.gpa, SnpPageType::NORMAL)
                        .context(error::Hv)?;
                }
            }
            IgvmDirectiveHeader::SnpVpContext {
                vp_index,
                gpa: _,
                vmsa,
                ..
            } => {
                if *vp_index == 0 {
                    bsp_regs = Some(parse_vmsa_registers(vmsa));
                }
            }
            _ => {}
        }
    }

    if is_standard_sev {
        for (gpa, rom) in &mut allocated_roms {
            log::info!("Encrypting IGVM ROM region: {:#x} (size={})", gpa, rom.size());
            let data = rom.as_slice_mut();
            vm.sev_launch_update_data(data).context(error::Hv)?;
        }
    }

    let bsp_regs = bsp_regs.unwrap_or_else(default_reset_state);
    Ok(bsp_regs)
}
