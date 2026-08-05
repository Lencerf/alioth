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

mod sev;
mod tdx;

use std::arch::x86_64::{__cpuid, CpuidResult};
use std::collections::HashMap;
use std::mem::{offset_of, size_of, size_of_val};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64};

use parking_lot::Mutex;
use snafu::ResultExt;
use zerocopy::{FromZeros, IntoBytes};

use crate::arch::cpuid::{Cpuid1Ecx, CpuidIn};
use crate::arch::layout::{
    BIOS_DATA_END, EBDA_END, EBDA_START, IOAPIC_START, MEM_64_START, PORT_ACPI_RESET,
    PORT_ACPI_SLEEP_CONTROL, PORT_ACPI_TIMER, RAM_32_SIZE,
};
use crate::arch::x86_64::cpu_models::{CPU_MODELS, CpuModel};
use crate::arch::x86_64::features::{CpuidReg, FEATURE_WORDS, CpuFeature};
use crate::board::{Board, BoardSpec, CpuSpec, CpuTopology, PCIE_MMIO_64_SIZE, Result, error};
use crate::device::ioapic::IoApic;
use crate::firmware::acpi::bindings::{
    AcpiTableFadt, AcpiTableHeader, AcpiTableRsdp, AcpiTableXsdt3,
};
use crate::firmware::acpi::reg::{AcpiPmTimer, FadtReset, FadtSleepControl};
use crate::firmware::acpi::{
    AcpiTable, create_fadt, create_madt, create_mcfg, create_rsdp, create_xsdt,
};
use crate::hv::{CocoSpec, Hypervisor, Vm};
use crate::loader::{Executable, InitState, PayloadSpec};
use crate::mem::{MemRange, MemRegion, MemRegionEntry, MemRegionType};
use crate::utils::wrapping_sum;

pub struct ArchBoard<V>
where
    V: Vm,
{
    pub(crate) cpuids: HashMap<CpuidIn, CpuidResult>,
    pub(crate) sev_ap_eip: AtomicU32,
    pub(crate) tdx_hob: AtomicU64,
    pub(crate) io_apic: Arc<IoApic<V::MsiSender>>,
}

fn add_topology(cpuids: &mut HashMap<CpuidIn, CpuidResult>, func: u32, levels: &[(u8, u16)]) {
    let edx = 0; // patched later in init_vcpu()
    for (index, (level, count)) in levels.iter().chain(&[(0, 0)]).enumerate() {
        let eax = count.next_power_of_two().trailing_zeros();
        let ebx = *count as u32;
        let ecx = ((*level as u32) << 8) | (index as u32);
        cpuids.insert(
            CpuidIn {
                func,
                index: Some(index as u32),
            },
            CpuidResult { eax, ebx, ecx, edx },
        );
    }
}

impl<V: Vm> ArchBoard<V> {
    pub fn new<H>(hv: &H, vm: &V, spec: &BoardSpec) -> Result<Self>
    where
        H: Hypervisor<Vm = V>,
    {
        let host_supported = hv.get_supported_cpuids(spec.coco.as_ref())?;
        let mut cpuids = expand_cpu_model(&spec.cpu, &host_supported)?;

        let threads_per_core = 1 + spec.cpu.topology.smt as u16;
        let threads_per_socket = spec.cpu.topology.cores * threads_per_core;

        add_topology(
            &mut cpuids,
            0xb,
            &[(1, threads_per_core), (2, threads_per_socket)],
        );

        let leaf0 = CpuidIn {
            func: 0,
            index: None,
        };
        let Some(out) = cpuids.get_mut(&leaf0) else {
            return error::MissingCpuid { leaf: leaf0 }.fail();
        };
        let vendor = [out.ebx, out.edx, out.ecx];
        match vendor.as_bytes() {
            b"GenuineIntel" => add_topology(
                &mut cpuids,
                0x1f,
                &[(1, threads_per_core), (2, threads_per_socket)],
            ),
            b"AuthenticAMD" => add_topology(
                &mut cpuids,
                0x8000_0026,
                &[
                    (1, threads_per_core),
                    (2, threads_per_socket),
                    (3, threads_per_socket),
                    (4, threads_per_socket),
                ],
            ),
            _ => {}
        }

        let leaf1 = CpuidIn {
            func: 0x1,
            index: None,
        };
        let Some(out) = cpuids.get_mut(&leaf1) else {
            return error::MissingCpuid { leaf: leaf1 }.fail();
        };
        out.ecx |= (Cpuid1Ecx::TSC_DEADLINE | Cpuid1Ecx::HYPERVISOR).bits();

        if let Some(coco) = &spec.coco
            && matches!(coco, CocoSpec::AmdSev { .. } | CocoSpec::AmdSnp { .. })
        {
            sev::adjust_cpuid(coco, &mut cpuids)?;
        }

        Ok(Self {
            cpuids,
            sev_ap_eip: AtomicU32::new(0),
            tdx_hob: AtomicU64::new(0),
            io_apic: Arc::new(IoApic::new(vm.create_msi_sender()?)),
        })
    }
}

fn encode_x2apic_id(topology: &CpuTopology, index: u16) -> u32 {
    let (socket_id, core_id, thread_id) = topology.encode(index);

    let thread_width = topology.smt as u32;
    let cores_per_socket = topology.cores as u32;
    let core_width = cores_per_socket.next_power_of_two().trailing_zeros();

    (socket_id as u32) << (core_width + thread_width)
        | (core_id as u32) << thread_width
        | (thread_id as u32)
}

impl<V> Board<V>
where
    V: Vm,
{
    pub fn encode_cpu_identity(&self, index: u16) -> u64 {
        encode_x2apic_id(&self.spec.cpu.topology, index) as u64
    }

    pub(crate) fn setup_fw_cfg(&self, payload: &PayloadSpec) -> Result<()> {
        let Some(dev) = &*self.fw_cfg.lock() else {
            return Ok(());
        };
        let mut dev = dev.lock();
        if let Some(Executable::Linux(image)) = &payload.executable {
            dev.add_kernel_data(image).context(error::FwCfg)?;
        };
        if let Some(cmdline) = &payload.cmdline {
            dev.add_kernel_cmdline(cmdline).context(error::FwCfg)?;
        };
        if let Some(initramfs) = &payload.initramfs {
            dev.add_initramfs_data(initramfs).context(error::FwCfg)?;
        };
        Ok(())
    }

    pub fn create_ram(&self) -> Result<()> {
        let spec = &self.spec;
        let memory = &self.memory;

        let low_mem_size = std::cmp::min(spec.mem.size, RAM_32_SIZE);
        let pages_low = self.create_ram_pages(low_mem_size, c"ram-low")?;
        let region_low = MemRegion {
            ranges: vec![MemRange::Ram(pages_low.clone())],
            entries: if self.spec.coco.is_none() {
                vec![
                    MemRegionEntry {
                        size: BIOS_DATA_END,
                        type_: MemRegionType::Reserved,
                    },
                    MemRegionEntry {
                        size: EBDA_START - BIOS_DATA_END,
                        type_: MemRegionType::Ram,
                    },
                    MemRegionEntry {
                        size: EBDA_END - EBDA_START,
                        type_: MemRegionType::Acpi,
                    },
                    MemRegionEntry {
                        size: low_mem_size - EBDA_END,
                        type_: MemRegionType::Ram,
                    },
                ]
            } else {
                vec![MemRegionEntry {
                    size: low_mem_size,
                    type_: MemRegionType::Ram,
                }]
            },
            callbacks: Mutex::new(vec![]),
        };
        memory.add_region(0, Arc::new(region_low))?;

        if spec.mem.size > RAM_32_SIZE {
            let mem_hi_size = spec.mem.size - RAM_32_SIZE;
            let mem_hi = self.create_ram_pages(mem_hi_size, c"ram-high")?;
            let region_hi = MemRegion::with_ram(mem_hi.clone(), MemRegionType::Ram);
            memory.add_region(MEM_64_START, Arc::new(region_hi))?;
        }
        Ok(())
    }

    pub fn coco_init(&self) -> Result<()> {
        let Some(coco) = &self.spec.coco else {
            return Ok(());
        };
        match coco {
            CocoSpec::AmdSev { policy } => self.sev_init(*policy)?,
            CocoSpec::AmdSnp { policy } => self.snp_init(*policy)?,
            CocoSpec::IntelTdx { attr } => self.tdx_init(*attr)?,
        }
        Ok(())
    }

    fn patch_dsdt(&self, data: &mut [u8; 352]) {
        let pcie_mmio_64_start = self.spec.pcie_mmio_64_start();
        let pcei_mmio_64_max = pcie_mmio_64_start - 1 + PCIE_MMIO_64_SIZE;
        data[DSDT_OFFSET_PCI_QWORD_MEM..(DSDT_OFFSET_PCI_QWORD_MEM + 8)]
            .copy_from_slice(&pcie_mmio_64_start.to_le_bytes());
        data[(DSDT_OFFSET_PCI_QWORD_MEM + 8)..(DSDT_OFFSET_PCI_QWORD_MEM + 16)]
            .copy_from_slice(&pcei_mmio_64_max.to_le_bytes());
        let sum = wrapping_sum(&*data);
        let checksum = &mut data[offset_of!(AcpiTableHeader, checksum)];
        *checksum = checksum.wrapping_sub(sum);
    }

    fn create_acpi(&self) -> AcpiTable {
        let mut table_bytes = Vec::new();
        let mut pointers = vec![];
        let mut checksums = vec![];

        let mut xsdt: AcpiTableXsdt3 = AcpiTableXsdt3::new_zeroed();
        let offset_xsdt = 0;
        table_bytes.extend(xsdt.as_bytes());

        let offset_dsdt = offset_xsdt + size_of_val(&xsdt);
        let mut dsdt = DSDT_TEMPLATE;
        self.patch_dsdt(&mut dsdt);
        table_bytes.extend(dsdt);

        let offset_fadt = offset_dsdt + size_of_val(&DSDT_TEMPLATE);
        debug_assert_eq!(offset_fadt % 4, 0);
        let fadt = create_fadt(offset_dsdt as u64);
        let pointer_fadt_to_dsdt = offset_fadt + offset_of!(AcpiTableFadt, xdsdt);
        table_bytes.extend(fadt.as_bytes());
        pointers.push(pointer_fadt_to_dsdt);
        checksums.push((offset_fadt, size_of_val(&fadt)));

        let offset_madt = offset_fadt + size_of_val(&fadt);
        debug_assert_eq!(offset_madt % 4, 0);
        let apic_ids: Vec<u32> = (0..self.spec.cpu.count)
            .map(|index| self.encode_cpu_identity(index) as u32)
            .collect();
        let (madt, madt_ioapic, madt_apics) = create_madt(&apic_ids);
        table_bytes.extend(madt.as_bytes());
        table_bytes.extend(madt_ioapic.as_bytes());
        for apic in madt_apics {
            table_bytes.extend(apic.as_bytes());
        }

        let offset_mcfg = offset_madt + madt.header.length as usize;
        debug_assert_eq!(offset_mcfg % 4, 0);
        let mcfg = create_mcfg();
        table_bytes.extend(mcfg.as_bytes());

        debug_assert_eq!(offset_xsdt % 4, 0);
        let xsdt_entries = [offset_fadt as u64, offset_madt as u64, offset_mcfg as u64];
        xsdt = create_xsdt(xsdt_entries);
        xsdt.write_to_prefix(&mut table_bytes).unwrap();
        for index in 0..xsdt_entries.len() {
            pointers.push(offset_xsdt + offset_of!(AcpiTableXsdt3, entries) + index * 8);
        }
        checksums.push((offset_xsdt, size_of_val(&xsdt)));

        let rsdp = create_rsdp(offset_xsdt as u64);

        AcpiTable {
            rsdp,
            tables: table_bytes,
            table_checksums: checksums,
            table_pointers: pointers,
        }
    }

    pub fn create_firmware_data(&self, _init_state: &InitState) -> Result<()> {
        let mut acpi_table = self.create_acpi();
        let memory = &self.memory;
        memory.add_io_dev(PORT_ACPI_RESET, Arc::new(FadtReset))?;
        memory.add_io_dev(PORT_ACPI_SLEEP_CONTROL, Arc::new(FadtSleepControl))?;
        memory.add_io_dev(PORT_ACPI_TIMER, Arc::new(AcpiPmTimer::new()))?;
        if self.spec.coco.is_none() {
            let ram = memory.ram_bus();
            acpi_table.relocate(EBDA_START + size_of::<AcpiTableRsdp>() as u64);
            acpi_table.update_checksums();
            ram.write_range(
                EBDA_START,
                size_of::<AcpiTableRsdp>() as u64,
                acpi_table.rsdp().as_bytes(),
            )?;
            ram.write_range(
                EBDA_START + size_of::<AcpiTableRsdp>() as u64,
                acpi_table.tables().len() as u64,
                acpi_table.tables(),
            )?;
        }
        if let Some(fw_cfg) = &*self.fw_cfg.lock() {
            let mut dev = fw_cfg.lock();
            dev.add_acpi(acpi_table).context(error::FwCfg)?;
            let mem_regions = memory.mem_region_entries();
            dev.add_e820(&mem_regions).context(error::FwCfg)?;
            dev.add_ram_size(self.spec.mem.size);
            dev.add_cpu_count(self.spec.cpu.count);
        }
        Ok(())
    }

    pub fn arch_init(&self) -> Result<()> {
        let io_apic = self.arch.io_apic.clone();
        self.mmio_devs.write().push((IOAPIC_START, io_apic));
        Ok(())
    }
}

const DSDT_TEMPLATE: [u8; 352] = [
    0x44, 0x53, 0x44, 0x54, 0x5D, 0x01, 0x00, 0x00, 0x02, 0x5D, 0x41, 0x4C, 0x49, 0x4F, 0x54, 0x48,
    0x41, 0x4C, 0x49, 0x4F, 0x54, 0x48, 0x56, 0x4D, 0x01, 0x00, 0x00, 0x00, 0x49, 0x4E, 0x54, 0x4C,
    0x28, 0x06, 0x23, 0x20, 0x5B, 0x82, 0x37, 0x2E, 0x5F, 0x53, 0x42, 0x5F, 0x43, 0x4F, 0x4D, 0x31,
    0x08, 0x5F, 0x48, 0x49, 0x44, 0x0C, 0x41, 0xD0, 0x05, 0x01, 0x08, 0x5F, 0x55, 0x49, 0x44, 0x01,
    0x08, 0x5F, 0x53, 0x54, 0x41, 0x0A, 0x0F, 0x08, 0x5F, 0x43, 0x52, 0x53, 0x11, 0x10, 0x0A, 0x0D,
    0x47, 0x01, 0xF8, 0x03, 0xF8, 0x03, 0x00, 0x08, 0x22, 0x10, 0x00, 0x79, 0x00, 0x08, 0x5F, 0x53,
    0x35, 0x5F, 0x12, 0x04, 0x01, 0x0A, 0x05, 0x5B, 0x82, 0x44, 0x0F, 0x2E, 0x5F, 0x53, 0x42, 0x5F,
    0x50, 0x43, 0x49, 0x30, 0x08, 0x5F, 0x48, 0x49, 0x44, 0x0C, 0x41, 0xD0, 0x0A, 0x08, 0x08, 0x5F,
    0x43, 0x49, 0x44, 0x0C, 0x41, 0xD0, 0x0A, 0x03, 0x08, 0x5F, 0x53, 0x45, 0x47, 0x00, 0x08, 0x5F,
    0x55, 0x49, 0x44, 0x00, 0x14, 0x32, 0x5F, 0x44, 0x53, 0x4D, 0x04, 0xA0, 0x29, 0x93, 0x68, 0x11,
    0x13, 0x0A, 0x10, 0xD0, 0x37, 0xC9, 0xE5, 0x53, 0x35, 0x7A, 0x4D, 0x91, 0x17, 0xEA, 0x4D, 0x19,
    0xC3, 0x43, 0x4D, 0xA0, 0x09, 0x93, 0x6A, 0x00, 0xA4, 0x11, 0x03, 0x01, 0x21, 0xA0, 0x07, 0x93,
    0x6A, 0x0A, 0x05, 0xA4, 0x00, 0xA4, 0x00, 0x08, 0x5F, 0x43, 0x52, 0x53, 0x11, 0x40, 0x09, 0x0A,
    0x8C, 0x88, 0x0D, 0x00, 0x02, 0x0C, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01,
    0x00, 0x47, 0x01, 0xF8, 0x0C, 0xF8, 0x0C, 0x01, 0x08, 0x87, 0x17, 0x00, 0x00, 0x0C, 0x07, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80, 0xFF, 0xFF, 0xFF, 0x9F, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x20, 0x87, 0x17, 0x00, 0x00, 0x0C, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0xA0, 0xFF, 0xFF, 0xFF, 0xBF, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x20, 0x8A, 0x2B, 0x00,
    0x00, 0x0C, 0x07, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01,
    0x00, 0x00, 0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x88, 0x0D, 0x00, 0x01, 0x0C,
    0x03, 0x00, 0x00, 0x00, 0x10, 0xFF, 0xFF, 0x00, 0x00, 0x00, 0xF0, 0x79, 0x00, 0x00, 0x00, 0x00,
];

const DSDT_OFFSET_PCI_QWORD_MEM: usize = 0x12b;

fn parse_model_name(model_str: &str) -> Option<(&str, u32)> {
    if let Some(pos) = model_str.rfind("-v") {
        let (name, ver_str) = model_str.split_at(pos);
        let ver_str = &ver_str[2..]; // skip "-v"
        if let Ok(version) = ver_str.parse::<u32>() {
            return Some((name, version));
        }
    }
    None
}

fn resolve_model(model_str: &str) -> Result<(&CpuModel, u32), super::Error> {
    if let Some((name, version)) = parse_model_name(model_str) {
        if let Some(model) = CPU_MODELS.iter().find(|m| m.name == name) {
            return Ok((model, version));
        }
    } else {
        if let Some(model) = CPU_MODELS.iter().find(|m| m.name == model_str) {
            return Ok((model, 1));
        }
    }
    error::InvalidCpuModel {
        model: model_str.to_owned(),
    }
    .fail()
}

fn set_cpuid_bit(
    cpuids: &mut HashMap<CpuidIn, CpuidResult>,
    feat: &CpuFeature,
    enable: bool,
) {
    let leaf = CpuidIn {
        func: feat.func,
        index: feat.index,
    };
    let entry = cpuids.entry(leaf).or_insert(CpuidResult {
        eax: 0,
        ebx: 0,
        ecx: 0,
        edx: 0,
    });
    let reg_val = match feat.reg {
        CpuidReg::Eax => &mut entry.eax,
        CpuidReg::Ebx => &mut entry.ebx,
        CpuidReg::Ecx => &mut entry.ecx,
        CpuidReg::Edx => &mut entry.edx,
    };
    if enable {
        *reg_val |= 1 << feat.bit;
    } else {
        *reg_val &= !(1 << feat.bit);
    }
}

fn get_cpuid_bit(cpuids: &HashMap<CpuidIn, CpuidResult>, feat: &CpuFeature) -> bool {
    let leaf = CpuidIn {
        func: feat.func,
        index: feat.index,
    };
    if let Some(entry) = cpuids.get(&leaf) {
        let reg_val = match feat.reg {
            CpuidReg::Eax => entry.eax,
            CpuidReg::Ebx => entry.ebx,
            CpuidReg::Ecx => entry.ecx,
            CpuidReg::Edx => entry.edx,
        };
        return (reg_val & (1 << feat.bit)) != 0;
    }
    false
}

fn encode_string_to_cpuid(s: &str) -> Vec<CpuidResult> {
    let mut bytes = s.as_bytes().to_vec();
    bytes.resize(48, 0); // pad with nulls up to 48 bytes
    let mut results = Vec::new();
    for chunk in bytes.chunks_exact(16) {
        let eax = u32::from_le_bytes(chunk[0..4].try_into().unwrap());
        let ebx = u32::from_le_bytes(chunk[4..8].try_into().unwrap());
        let ecx = u32::from_le_bytes(chunk[8..12].try_into().unwrap());
        let edx = u32::from_le_bytes(chunk[12..16].try_into().unwrap());
        results.push(CpuidResult { eax, ebx, ecx, edx });
    }
    results
}

fn expand_cpu_model(
    spec: &CpuSpec,
    host_supported: &HashMap<CpuidIn, CpuidResult>,
) -> Result<HashMap<CpuidIn, CpuidResult>, super::Error> {
    if spec.model == "host" {
        let mut cpuids = host_supported.clone();
        // Apply customizations to host model
        for feat_str in &spec.features {
            let (enable, feat_name) = if let Some(stripped) = feat_str.strip_prefix('+') {
                (true, stripped)
            } else if let Some(stripped) = feat_str.strip_prefix('-') {
                (false, stripped)
            } else {
                (true, feat_str.as_str())
            };
            if let Some(feat) = lookup_feature(feat_name) {
                if enable {
                    // Check if host supports it
                    if !get_cpuid_bit(host_supported, &feat) {
                        return error::UnsupportedCpuFeature {
                            feature: feat_name.to_owned(),
                        }
                        .fail();
                    }
                }
                set_cpuid_bit(&mut cpuids, &feat, enable);
            } else {
                return error::InvalidCpuFeature {
                    feature: feat_str.clone(),
                }
                .fail();
            }
        }
        // Ensure host brand and cache are populated from raw CPUID
        let leaf_8000_0000 = __cpuid(0x8000_0000);
        cpuids.insert(
            CpuidIn {
                func: 0x8000_0000,
                index: None,
            },
            leaf_8000_0000,
        );
        for func in 0x8000_0002..=0x8000_0006 {
            let host_cpuid = __cpuid(func);
            cpuids.insert(CpuidIn { func, index: None }, host_cpuid);
        }
        return Ok(cpuids);
    }

    let (model, version) = resolve_model(&spec.model)?;

    // 1. Initialize CPUID map with model base features
    let mut cpuids = HashMap::new();

    // Set level and vendor
    let leaf0 = CpuidIn {
        func: 0,
        index: None,
    };
    cpuids.insert(
        leaf0,
        CpuidResult {
            eax: model.level,
            ebx: model.vendor[0],
            edx: model.vendor[1],
            ecx: model.vendor[2],
        },
    );

    // Set xlevel
    let leaf_8000_0000 = CpuidIn {
        func: 0x8000_0000,
        index: None,
    };
    cpuids.insert(
        leaf_8000_0000,
        CpuidResult {
            eax: model.xlevel,
            ebx: 0,
            ecx: 0,
            edx: 0,
        },
    );

    // Family, model, stepping are encoded in leaf 1 EAX
    let family_encoded = if model.family >= 16 {
        ((model.family - 15) & 0xff) << 20
    } else {
        0
    };
    let model_encoded = if model.family >= 6 {
        ((model.model >> 4) & 0xf) << 16
    } else {
        0
    };
    let eax_val = ((model.family & 0xf) << 8)
        | ((model.model & 0xf) << 4)
        | (model.stepping & 0xf)
        | family_encoded
        | model_encoded;

    let leaf1 = CpuidIn {
        func: 1,
        index: None,
    };
    cpuids.insert(
        leaf1,
        CpuidResult {
            eax: eax_val,
            ebx: 0,
            ecx: 0,
            edx: 0,
        },
    );

    // Populate base features
    for &feat_name in model.features {
        if let Some(feat) = lookup_feature(feat_name) {
            set_cpuid_bit(&mut cpuids, &feat, true);
        } else {
            panic!("Feature {} not found in registry", feat_name);
        }
    }

    // 2. Apply version properties (inheritance)
    for vdef in model.versions {
        if vdef.version > version {
            break;
        }
        for &(prop_name, enable) in vdef.props {
            if let Some(feat) = lookup_feature(prop_name) {
                set_cpuid_bit(&mut cpuids, &feat, enable);
            } else {
                panic!("Version property {} not found in registry", prop_name);
            }
        }
    }

    // 3. Apply user customizations (+/- features)
    for feat_str in &spec.features {
        let (enable, feat_name) = if let Some(stripped) = feat_str.strip_prefix('+') {
            (true, stripped)
        } else if let Some(stripped) = feat_str.strip_prefix('-') {
            (false, stripped)
        } else {
            (true, feat_str.as_str())
        };
        if let Some(feat) = lookup_feature(feat_name) {
            set_cpuid_bit(&mut cpuids, &feat, enable);
        } else {
            return error::InvalidCpuFeature {
                feature: feat_str.clone(),
            }
            .fail();
        }
    }

    // 4. Filter against host capabilities and warn/fail
    for info in FEATURE_WORDS {
        for bit in 0..32 {
            let name = info.names[bit as usize];
            if name.is_empty() {
                continue;
            }
            let feat = CpuFeature {
                name,
                func: info.func,
                index: info.index,
                reg: info.reg,
                bit: bit as u8,
            };
            if get_cpuid_bit(&cpuids, &feat) {
                if !get_cpuid_bit(host_supported, &feat) {
                    return error::UnsupportedCpuFeature {
                        feature: name.to_owned(),
                    }
                    .fail();
                }
            }
        }
    }

    // Encode model_id into 0x8000_0002..4
    let brand = encode_string_to_cpuid(model.model_id);
    for (i, res) in brand.into_iter().enumerate() {
        let func = 0x8000_0002 + i as u32;
        cpuids.insert(CpuidIn { func, index: None }, res);
    }

    // Copy 0x8000_0005 and 0x8000_0006 from host raw CPUID
    for func in 0x8000_0005..=0x8000_0006 {
        let leaf = CpuidIn { func, index: None };
        cpuids.insert(leaf, __cpuid(func));
    }

    // Copy hypervisor leaves from host_supported
    for (leaf, &res) in host_supported {
        if leaf.func >= 0x4000_0000 && leaf.func <= 0x4000_00ff {
            cpuids.insert(leaf.clone(), res);
        }
    }
    Ok(cpuids)
}

fn lookup_feature(name: &str) -> Option<CpuFeature> {
    for info in FEATURE_WORDS {
        for (bit, &feat_name) in info.names.iter().enumerate() {
            if feat_name == name {
                return Some(CpuFeature {
                    name: feat_name,
                    func: info.func,
                    index: info.index,
                    reg: info.reg,
                    bit: bit as u8,
                });
            }
        }
    }
    None
}

#[cfg(test)]
#[path = "board_amd64_test.rs"]
mod tests;
