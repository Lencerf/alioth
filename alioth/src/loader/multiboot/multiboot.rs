// Copyright 2024 Google LLC
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

use std::ffi::CStr;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::mem::{offset_of, size_of, size_of_val};
use std::path::Path;

use snafu::ResultExt;
use zerocopy::{FromBytes, FromZeros, Immutable, IntoBytes};

use crate::arch::layout::{
    APIC_START, BOOT_GDT_START, KERNEL_CMDLINE_LIMIT, KERNEL_CMDLINE_START, MULTIBOOT_INFO_START,
};
use crate::arch::msr::{ApicBase, Msr};
use crate::arch::reg::{Cr0, DtReg, DtRegVal, Reg, Rflags, SReg, SegAccess, SegReg, SegRegVal};
use crate::loader::{InitState, Result, error, search_initramfs_address};
use crate::mem::mapped::RamBus;
use crate::mem::{MemRegionEntry, MemRegionType};
use crate::sys::elf::{
    ELF_HEADER_MAGIC, ELF_IDENT_CLASS_64, Elf32Header, Elf32ProgramHeader, Elf64Header,
    Elf64ProgramHeader, PT_LOAD,
};

#[repr(C, packed)]
#[derive(Debug, Default, Copy, Clone, IntoBytes, Immutable, FromBytes)]
struct MultibootInfo {
    flags: u32,
    mem_lower: u32,
    mem_upper: u32,
    boot_device: u32,
    cmdline: u32,
    mods_count: u32,
    mods_addr: u32,
    syms: [u32; 4],
    mmap_length: u32,
    mmap_addr: u32,
    drives_length: u32,
    drives_addr: u32,
    config_table: u32,
    boot_loader_name: u32,
    apm_table: u32,
    vbe_control_info: u32,
    vbe_mode_info: u32,
    vbe_mode: u16,
    vbe_interface_seg: u16,
    vbe_interface_off: u16,
    vbe_interface_len: u16,
    framebuffer_addr: u64,
    framebuffer_pitch: u32,
    framebuffer_width: u32,
    framebuffer_height: u32,
    framebuffer_bpp: u8,
    framebuffer_type: u8,
    color_info: [u8; 6],
}

#[repr(C, packed)]
#[derive(Debug, Default, Copy, Clone, IntoBytes, Immutable, FromBytes)]
struct MultibootMmapEntry {
    size: u32,
    addr: u64,
    len: u64,
    type_: u32,
}

#[repr(C, packed)]
#[derive(Debug, Default, Copy, Clone, IntoBytes, Immutable, FromBytes)]
struct MultibootModList {
    mod_start: u32,
    mod_end: u32,
    cmdline: u32,
    pad: u32,
}

#[repr(C, packed)]
#[derive(Debug, IntoBytes, Default, Immutable)]
struct MultibootInfoPage {
    info: MultibootInfo,
    mmap: [MultibootMmapEntry; 32],
    boot_loader_name: [u8; 32],
    mods: [MultibootModList; 1],
}

pub fn load<P: AsRef<Path>>(
    memory: &RamBus,
    mem_regions: &[(u64, MemRegionEntry)],
    kernel: P,
    cmdline: Option<&CStr>,
    initramfs: Option<P>,
) -> Result<InitState> {
    let access_kernel = error::AccessFile {
        path: kernel.as_ref(),
    };
    let mut kernel = BufReader::new(File::open(&kernel).context(access_kernel)?);

    // Search for the Multiboot Header in the first 8192 bytes
    let mut header_buf = [0u8; 8192];
    kernel.seek(SeekFrom::Start(0)).context(access_kernel)?;
    let bytes_read = kernel.read(&mut header_buf).context(access_kernel)?;

    let mut found_header = None;
    for i in (0..bytes_read.saturating_sub(12)).step_by(4) {
        let magic = u32::from_le_bytes(header_buf[i..i + 4].try_into().unwrap());
        if magic == 0x1BADB002 {
            let flags = u32::from_le_bytes(header_buf[i + 4..i + 8].try_into().unwrap());
            let checksum = u32::from_le_bytes(header_buf[i + 8..i + 12].try_into().unwrap());
            if magic.wrapping_add(flags).wrapping_add(checksum) == 0 {
                found_header = Some((i as u64, flags));
                break;
            }
        }
    }

    let Some((header_file_offset, flags)) = found_header else {
        return error::MissingMultibootHeader.fail();
    };

    let entry_point;

    if flags & 0x00010000 != 0 {
        // Address fields (AOUT kludge) are valid
        let mut addr_fields = [0u32; 5];
        kernel
            .seek(SeekFrom::Start(header_file_offset + 12))
            .context(access_kernel)?;
        kernel
            .read_exact(addr_fields.as_mut_bytes())
            .context(access_kernel)?;

        let header_addr = addr_fields[0] as u64;
        let load_addr = addr_fields[1] as u64;
        let load_end_addr = addr_fields[2] as u64;
        let bss_end_addr = addr_fields[3] as u64;
        let entry_addr = addr_fields[4] as u64;

        if load_addr > header_addr {
            return error::InvalidMultibootHeader {
                reason: "load_addr cannot be greater than header_addr",
            }
            .fail();
        }

        let load_file_offset = header_file_offset - (header_addr - load_addr);
        let kernel_meta = kernel.get_ref().metadata().context(access_kernel)?;
        let file_size = kernel_meta.len();

        let load_size = if load_end_addr > 0 {
            if load_end_addr < load_addr {
                return error::InvalidMultibootHeader {
                    reason: "load_end_addr cannot be less than load_addr",
                }
                .fail();
            }
            load_end_addr - load_addr
        } else {
            file_size.saturating_sub(load_file_offset)
        };

        kernel
            .seek(SeekFrom::Start(load_file_offset))
            .context(access_kernel)?;
        memory.write_range(load_addr, load_size, &mut kernel)?;
        log::info!(
            "loaded via address fields at {:#x?}-{:#x?}",
            load_addr,
            load_addr + load_size
        );

        if bss_end_addr > load_addr + load_size {
            let bss_size = bss_end_addr - (load_addr + load_size);
            memory.write_range(
                load_addr + load_size,
                bss_size,
                std::io::repeat(0).take(bss_size),
            )?;
            log::info!(
                "zeroed bss segment at {:#x?}-{:#x?}",
                load_addr + load_size,
                bss_end_addr
            );
        }

        entry_point = entry_addr;
    } else {
        // Load using ELF program headers
        let mut ident = [0u8; 16];
        kernel.seek(SeekFrom::Start(0)).context(access_kernel)?;
        kernel.read_exact(&mut ident).context(access_kernel)?;
        if ident[0..4] != ELF_HEADER_MAGIC {
            return error::MissingMagic {
                magic: u32::from_ne_bytes(ELF_HEADER_MAGIC) as u64,
                found: u32::from_ne_bytes(ident[0..4].try_into().unwrap()) as u64,
            }
            .fail();
        }

        let is_64 = ident[4] == ELF_IDENT_CLASS_64;
        if is_64 {
            kernel.seek(SeekFrom::Start(0)).context(access_kernel)?;
            let mut elf_header = Elf64Header::new_zeroed();
            kernel
                .read_exact(elf_header.as_mut_bytes())
                .context(access_kernel)?;

            kernel
                .seek(SeekFrom::Start(elf_header.ph_off))
                .context(access_kernel)?;
            let mut program_header =
                Elf64ProgramHeader::new_vec_zeroed(elf_header.ph_num as usize).unwrap();
            kernel
                .read_exact(program_header.as_mut_bytes())
                .context(access_kernel)?;
            for phdr in program_header.iter() {
                if phdr.type_ == PT_LOAD && phdr.file_sz > 0 {
                    kernel
                        .seek(SeekFrom::Start(phdr.offset))
                        .context(access_kernel)?;
                    memory.write_range(phdr.paddr, phdr.file_sz, &mut kernel)?;
                    log::info!(
                        "loaded Elf64 PT_LOAD segment at {:#x?}-{:#x?}",
                        phdr.paddr,
                        phdr.paddr + phdr.file_sz
                    );
                    if phdr.mem_sz > phdr.file_sz {
                        let zero_size = phdr.mem_sz - phdr.file_sz;
                        memory.write_range(
                            phdr.paddr + phdr.file_sz,
                            zero_size,
                            std::io::repeat(0).take(zero_size),
                        )?;
                    }
                }
            }
            entry_point = elf_header.entry;
        } else {
            kernel.seek(SeekFrom::Start(0)).context(access_kernel)?;
            let mut elf_header = Elf32Header::new_zeroed();
            kernel
                .read_exact(elf_header.as_mut_bytes())
                .context(access_kernel)?;

            kernel
                .seek(SeekFrom::Start(elf_header.ph_off as u64))
                .context(access_kernel)?;
            let mut program_header =
                Elf32ProgramHeader::new_vec_zeroed(elf_header.ph_num as usize).unwrap();
            kernel
                .read_exact(program_header.as_mut_bytes())
                .context(access_kernel)?;
            for phdr in program_header.iter() {
                if phdr.type_ == PT_LOAD && phdr.file_sz > 0 {
                    kernel
                        .seek(SeekFrom::Start(phdr.offset as u64))
                        .context(access_kernel)?;
                    memory.write_range(phdr.paddr as u64, phdr.file_sz as u64, &mut kernel)?;
                    log::info!(
                        "loaded Elf32 PT_LOAD segment at {:#x?}-{:#x?}",
                        phdr.paddr,
                        phdr.paddr + phdr.file_sz
                    );
                    if phdr.mem_sz > phdr.file_sz {
                        let zero_size = (phdr.mem_sz - phdr.file_sz) as u64;
                        memory.write_range(
                            (phdr.paddr + phdr.file_sz) as u64,
                            zero_size,
                            std::io::repeat(0).take(zero_size),
                        )?;
                    }
                }
            }
            entry_point = elf_header.entry as u64;
        }
    }

    log::info!("Multiboot entry point = {entry_point:#x?}");

    let mut start_info_page = MultibootInfoPage {
        info: MultibootInfo {
            flags: 1 | (1 << 6) | (1 << 9), // mem_lower/upper, mmap, boot_loader_name
            ..Default::default()
        },
        ..Default::default()
    };

    // Calculate mem_lower/mem_upper
    let mem_lower = 640;
    let mut mem_upper = 0;
    for (addr, region) in mem_regions.iter() {
        if region.type_ == MemRegionType::Ram {
            let start = *addr;
            let end = start + region.size;
            if start <= 1024 * 1024 && end > 1024 * 1024 {
                mem_upper = ((end - 1024 * 1024) / 1024) as u32;
            }
        }
    }
    start_info_page.info.mem_lower = mem_lower;
    start_info_page.info.mem_upper = mem_upper;

    // Load cmd line
    if let Some(cmdline) = cmdline {
        let cmdline = cmdline.to_bytes_with_nul();
        if cmdline.len() as u64 > KERNEL_CMDLINE_LIMIT {
            return error::CmdLineTooLong {
                len: cmdline.len(),
                limit: KERNEL_CMDLINE_LIMIT,
            }
            .fail();
        }
        memory.write_range(KERNEL_CMDLINE_START, cmdline.len() as u64, cmdline)?;
        start_info_page.info.cmdline = KERNEL_CMDLINE_START as u32;
        start_info_page.info.flags |= 1 << 2; // cmdline valid
    }

    // Load initramfs (as Multiboot module)
    let initramfs_range;
    if let Some(initramfs) = initramfs {
        let access_initramfs = error::AccessFile {
            path: initramfs.as_ref(),
        };
        let initramfs = File::open(&initramfs).context(access_initramfs)?;
        let initramfs_size = initramfs.metadata().context(access_initramfs)?.len();
        let initramfs_gpa = search_initramfs_address(mem_regions, initramfs_size, (2 << 30) - 1)?;
        let initramfs_end = initramfs_gpa + initramfs_size;
        memory.write_range(initramfs_gpa, initramfs_size, initramfs)?;

        start_info_page.info.mods_count = 1;
        start_info_page.info.mods_addr =
            (MULTIBOOT_INFO_START + offset_of!(MultibootInfoPage, mods) as u64) as u32;

        start_info_page.mods[0] = MultibootModList {
            mod_start: initramfs_gpa as u32,
            mod_end: initramfs_end as u32,
            cmdline: 0,
            pad: 0,
        };
        start_info_page.info.flags |= 1 << 3; // mods valid

        log::info!(
            "initramfs loaded as multiboot module at {:#x} - {:#x}",
            initramfs_gpa,
            initramfs_end - 1
        );
        initramfs_range = Some(initramfs_gpa..initramfs_end);
    } else {
        initramfs_range = None;
    }

    // Populate mmap
    let mut index = 0;
    for (addr, region) in mem_regions.iter() {
        let type_ = match region.type_ {
            MemRegionType::Ram => 1,
            MemRegionType::Acpi => 3,
            MemRegionType::Reserved | MemRegionType::Pmem => 2,
            MemRegionType::Hidden => continue,
        };
        if index >= start_info_page.mmap.len() {
            break;
        }
        start_info_page.mmap[index] = MultibootMmapEntry {
            size: 20,
            addr: *addr,
            len: region.size,
            type_,
        };
        index += 1;
    }
    start_info_page.info.mmap_length = (index * size_of::<MultibootMmapEntry>()) as u32;
    start_info_page.info.mmap_addr =
        (MULTIBOOT_INFO_START + offset_of!(MultibootInfoPage, mmap) as u64) as u32;

    // Boot loader name
    let name_bytes = b"alioth\0";
    let name_len = std::cmp::min(name_bytes.len(), start_info_page.boot_loader_name.len());
    start_info_page.boot_loader_name[..name_len].copy_from_slice(&name_bytes[..name_len]);
    start_info_page.info.boot_loader_name =
        (MULTIBOOT_INFO_START + offset_of!(MultibootInfoPage, boot_loader_name) as u64) as u32;

    memory.write_t(MULTIBOOT_INFO_START, &start_info_page)?;

    // Set up GDT for protected mode
    let boot_cs = SegRegVal {
        selector: 0x10,
        base: 0,
        limit: 0xfff_ffff,
        access: SegAccess(0xc09b),
    };
    let boot_ds = SegRegVal {
        selector: 0x18,
        base: 0,
        limit: 0xfff_ffff,
        access: SegAccess(0xc093),
    };
    let boot_tr = SegRegVal {
        selector: 0x20,
        base: 0,
        limit: 0x67,
        access: SegAccess(0x8b),
    };
    let boot_ldtr = SegRegVal {
        selector: 0x28,
        base: 0,
        limit: 0,
        access: SegAccess(0x82),
    };
    let gdt = [
        0,
        0,
        boot_cs.to_desc(),
        boot_ds.to_desc(),
        boot_tr.to_desc(),
        boot_ldtr.to_desc(),
    ];
    let gdtr = DtRegVal {
        base: BOOT_GDT_START,
        limit: size_of_val(&gdt) as u16 - 1,
    };
    memory.write_t(BOOT_GDT_START, &gdt)?;

    let idtr = DtRegVal { base: 0, limit: 0 };

    let mut apic_base = ApicBase(APIC_START);
    apic_base.set_bsp(true);
    apic_base.set_xapic(true);
    apic_base.set_x2apic(true);

    Ok(InitState {
        regs: vec![
            (Reg::Rax, 0x2BADB002),
            (Reg::Rbx, MULTIBOOT_INFO_START),
            (Reg::Rflags, Rflags::RESERVED_1.bits() as u64),
            (Reg::Rip, entry_point),
        ],
        sregs: vec![(SReg::Cr0, Cr0::PE.bits()), (SReg::Cr4, 0)],
        seg_regs: vec![
            (SegReg::Cs, boot_cs),
            (SegReg::Ds, boot_ds),
            (SegReg::Es, boot_ds),
            (SegReg::Fs, boot_ds),
            (SegReg::Gs, boot_ds),
            (SegReg::Ss, boot_ds),
            (SegReg::Tr, boot_tr),
            (SegReg::Ldtr, boot_ldtr),
        ],
        dt_regs: vec![(DtReg::Gdtr, gdtr), (DtReg::Idtr, idtr)],
        msrs: vec![(Msr::APIC_BASE, apic_base.0), (Msr::EFER, 0)],
        initramfs: initramfs_range,
    })
}

#[cfg(test)]
#[path = "multiboot_test.rs"]
mod tests;
