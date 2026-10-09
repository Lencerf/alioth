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

pub mod bindings;
pub mod reg;

use std::mem::{offset_of, size_of};

use zerocopy::{FromBytes, IntoBytes, transmute};

#[cfg(target_arch = "aarch64")]
use crate::arch::layout::{
    GIC_DIST_START, GIC_MSI_START, GIC_V2_CPU_INTERFACE_START, GIC_V3_REDIST_START, PL011_START,
};
use crate::arch::layout::PCIE_CONFIG_START;
#[cfg(target_arch = "x86_64")]
use crate::arch::layout::{
    APIC_START, IOAPIC_START, PORT_ACPI_RESET, PORT_ACPI_SLEEP_CONTROL, PORT_ACPI_SLEEP_STATUS,
    PORT_ACPI_TIMER,
};
#[cfg(target_arch = "aarch64")]
use crate::board::CpuTopology;
use crate::firmware::acpi::bindings::AcpiFadtFlag;
use crate::utils::wrapping_sum;

#[cfg(target_arch = "aarch64")]
use self::bindings::{
    AcpiFadtArmBootFlag, AcpiGtdtFlag, AcpiIortIdMapping, AcpiIortItsGroup1, AcpiIortMemoryAccess,
    AcpiIortNode, AcpiIortRootComplex1, AcpiMadtGenericDistributor, AcpiMadtGenericInterrupt,
    AcpiMadtGenericMsiFrame, AcpiMadtGenericRedistributor, AcpiMadtGenericTranslator, AcpiPpttFlag,
    AcpiPpttProcessor, AcpiTableGtdt, AcpiTableIort, AcpiTablePptt, AcpiTableSpcr, AcpiTableXsdt6,
    AcpiTableXsdt7, GTDT_REVISION, IORT_NODE_ITS_GROUP, IORT_NODE_PCI_ROOT_COMPLEX, IORT_REVISION,
    MADT_GENERIC_DISTRIBUTOR, MADT_GENERIC_INTERRUPT, MADT_GENERIC_MSI_FRAME,
    MADT_GENERIC_REDISTRIBUTOR, MADT_GENERIC_TRANSLATOR, PPTT_REVISION, PPTT_TYPE_PROCESSOR,
    SIG_GTDT, SIG_IORT, SIG_PPTT, SIG_SPCR, SPCR_INTERFACE_ARM_PL011, SPCR_INTERRUPT_TYPE_GIC,
    SPCR_REVISION,
};
#[cfg(target_arch = "x86_64")]
use self::bindings::{AcpiMadtIoApic, AcpiMadtLocalX2apic, MADT_IO_APIC, MADT_LOCAL_X2APIC};
use self::bindings::{
    AcpiGenericAddress, AcpiMcfgAllocation, AcpiSubtableHeader, AcpiTableFadt, AcpiTableHeader,
    AcpiTableMadt, AcpiTableMcfg1, AcpiTableRsdp, FADT_MAJOR_VERSION, FADT_MINOR_VERSION,
    MADT_REVISION, MCFG_REVISION, RSDP_REVISION, SIG_FADT, SIG_MADT, SIG_MCFG, SIG_RSDP, SIG_XSDT,
    XSDT_REVISION,
};
#[cfg(target_arch = "x86_64")]
use self::bindings::AcpiTableXsdt3;
#[cfg(target_arch = "x86_64")]
use self::reg::FADT_RESET_VAL;

const OEM_ID: [u8; 6] = *b"ALIOTH";

fn default_header() -> AcpiTableHeader {
    AcpiTableHeader {
        checksum: 0,
        oem_id: OEM_ID,
        oem_table_id: *b"ALIOTHVM",
        oem_revision: 1,
        asl_compiler_id: *b"ALTH",
        asl_compiler_revision: 1,
        ..Default::default()
    }
}

// https://uefi.org/htmlspecs/ACPI_Spec_6_4_html/05_ACPI_Software_Programming_Model/ACPI_Software_Programming_Model.html#root-system-description-pointer-rsdp-structure
pub fn create_rsdp(xsdt_addr: u64) -> AcpiTableRsdp {
    AcpiTableRsdp {
        signature: SIG_RSDP,
        oem_id: OEM_ID,
        revision: RSDP_REVISION,
        length: size_of::<AcpiTableRsdp>() as u32,
        xsdt_physical_address: transmute!(xsdt_addr),
        ..Default::default()
    }
}

// https://uefi.org/htmlspecs/ACPI_Spec_6_4_html/05_ACPI_Software_Programming_Model/ACPI_Software_Programming_Model.html#extended-system-description-table-fields-xsdt
#[cfg(target_arch = "x86_64")]
pub fn create_xsdt(entries: [u64; 3]) -> AcpiTableXsdt3 {
    let total_length = size_of::<AcpiTableHeader>() + size_of::<u64>() * 3;
    let entries = entries.map(|e| transmute!(e));
    AcpiTableXsdt3 {
        header: AcpiTableHeader {
            signature: SIG_XSDT,
            length: total_length as u32,
            revision: XSDT_REVISION,
            ..default_header()
        },
        entries,
    }
}

#[cfg(target_arch = "aarch64")]
pub fn create_xsdt6(entries: [u64; 6]) -> AcpiTableXsdt6 {
    let total_length = size_of::<AcpiTableHeader>() + size_of::<u64>() * 6;
    let entries = entries.map(|e| transmute!(e));
    AcpiTableXsdt6 {
        header: AcpiTableHeader {
            signature: SIG_XSDT,
            length: total_length as u32,
            revision: XSDT_REVISION,
            ..default_header()
        },
        entries,
    }
}

#[cfg(target_arch = "aarch64")]
pub fn create_xsdt7(entries: [u64; 7]) -> AcpiTableXsdt7 {
    let total_length = size_of::<AcpiTableHeader>() + size_of::<u64>() * 7;
    let entries = entries.map(|e| transmute!(e));
    AcpiTableXsdt7 {
        header: AcpiTableHeader {
            signature: SIG_XSDT,
            length: total_length as u32,
            revision: XSDT_REVISION,
            ..default_header()
        },
        entries,
    }
}

// https://uefi.org/htmlspecs/ACPI_Spec_6_4_html/05_ACPI_Software_Programming_Model/ACPI_Software_Programming_Model.html#fadt-format
#[cfg(target_arch = "x86_64")]
pub fn create_fadt(dsdt_addr: u64) -> AcpiTableFadt {
    AcpiTableFadt {
        header: AcpiTableHeader {
            signature: SIG_FADT,
            revision: FADT_MAJOR_VERSION,
            length: size_of::<AcpiTableFadt>() as u32,
            ..default_header()
        },
        reset_register: AcpiGenericAddress {
            space_id: 1,
            bit_width: 8,
            bit_offset: 0,
            access_width: 1,
            address: transmute!(PORT_ACPI_RESET as u64),
        },
        reset_value: FADT_RESET_VAL,
        xpm_timer_block: AcpiGenericAddress {
            space_id: 1,
            bit_width: 32,
            bit_offset: 0,
            access_width: 3,
            address: transmute!(PORT_ACPI_TIMER as u64),
        },
        sleep_control: AcpiGenericAddress {
            space_id: 1,
            bit_width: 8,
            bit_offset: 0,
            access_width: 1,
            address: transmute!(PORT_ACPI_SLEEP_CONTROL as u64),
        },
        sleep_status: AcpiGenericAddress {
            space_id: 1,
            bit_width: 8,
            bit_offset: 0,
            access_width: 1,
            address: transmute!(PORT_ACPI_SLEEP_STATUS as u64),
        },
        flags: AcpiFadtFlag::HW_REDUCED_ACPI
            | AcpiFadtFlag::RESET_REG_SUP
            | AcpiFadtFlag::TMR_VAL_EXT,
        minor_revision: FADT_MINOR_VERSION,
        hypervisor_id: *b"ALIOTH  ",
        xdsdt: transmute!(dsdt_addr),
        ..Default::default()
    }
}

#[cfg(target_arch = "aarch64")]
pub fn create_fadt(dsdt_addr: u64) -> AcpiTableFadt {
    AcpiTableFadt {
        header: AcpiTableHeader {
            signature: SIG_FADT,
            revision: FADT_MAJOR_VERSION,
            length: size_of::<AcpiTableFadt>() as u32,
            ..default_header()
        },
        flags: AcpiFadtFlag::HW_REDUCED_ACPI,
        arm_boot_flags: (AcpiFadtArmBootFlag::PSCI_COMPLIANT | AcpiFadtArmBootFlag::PSCI_USE_HVC)
            .bits(),
        minor_revision: FADT_MINOR_VERSION,
        hypervisor_id: *b"ALIOTH  ",
        xdsdt: transmute!(dsdt_addr),
        ..Default::default()
    }
}

// https://uefi.org/specs/ACPI/6.5/05_ACPI_Software_Programming_Model.html#multiple-apic-description-table-madt
#[cfg(target_arch = "x86_64")]
pub fn create_madt(apic_ids: &[u32]) -> (AcpiTableMadt, AcpiMadtIoApic, Vec<AcpiMadtLocalX2apic>) {
    let total_length = size_of::<AcpiTableMadt>()
        + size_of::<AcpiMadtIoApic>()
        + apic_ids.len() * size_of::<AcpiMadtLocalX2apic>();
    let mut checksum = 0u8;

    let mut madt = AcpiTableMadt {
        header: AcpiTableHeader {
            signature: SIG_MADT,
            length: total_length as u32,
            revision: MADT_REVISION,
            ..default_header()
        },
        address: APIC_START as u32,
        flags: 0,
    };
    checksum = checksum.wrapping_sub(wrapping_sum(madt.as_bytes()));

    let io_apic = AcpiMadtIoApic {
        header: AcpiSubtableHeader {
            type_: MADT_IO_APIC,
            length: size_of::<AcpiMadtIoApic>() as u8,
        },
        id: 0,
        address: IOAPIC_START as u32,
        global_irq_base: 0,
        ..Default::default()
    };
    checksum = checksum.wrapping_sub(wrapping_sum(io_apic.as_bytes()));

    let mut x2apics = vec![];
    for (index, apic_id) in apic_ids.iter().enumerate() {
        let x2apic = AcpiMadtLocalX2apic {
            header: AcpiSubtableHeader {
                type_: MADT_LOCAL_X2APIC,
                length: size_of::<AcpiMadtLocalX2apic>() as u8,
            },
            local_apic_id: *apic_id,
            uid: index as u32,
            lapic_flags: 1,
            ..Default::default()
        };
        checksum = checksum.wrapping_sub(wrapping_sum(x2apic.as_bytes()));
        x2apics.push(x2apic);
    }
    madt.header.checksum = checksum;

    (madt, io_apic, x2apics)
}

pub fn create_mcfg() -> AcpiTableMcfg1 {
    let mut mcfg = AcpiTableMcfg1 {
        header: AcpiTableHeader {
            signature: SIG_MCFG,
            length: size_of::<AcpiTableMcfg1>() as u32,
            revision: MCFG_REVISION,
            ..default_header()
        },
        reserved: [0; 8],
        allocations: [AcpiMcfgAllocation {
            address: transmute!(PCIE_CONFIG_START),
            pci_segment: 0,
            start_bus_number: 0,
            end_bus_number: 0,
            ..Default::default()
        }],
    };
    mcfg.header.checksum = 0u8.wrapping_sub(wrapping_sum(mcfg.as_bytes()));
    mcfg
}

#[cfg(target_arch = "aarch64")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GicVersion {
    V2,
    V3,
}

#[cfg(target_arch = "aarch64")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsiController {
    GicV2m,
    Its,
}

#[cfg(target_arch = "aarch64")]
pub fn create_madt(
    mpidrs: &[u64],
    gic: GicVersion,
    msi: Option<MsiController>,
) -> (AcpiTableMadt, Vec<u8>) {
    let mut subtables = Vec::new();

    let (gic_version, cpu_base) = match gic {
        GicVersion::V2 => (2, GIC_V2_CPU_INTERFACE_START),
        GicVersion::V3 => (3, 0),
    };
    let gicd = AcpiMadtGenericDistributor {
        header: AcpiSubtableHeader {
            type_: MADT_GENERIC_DISTRIBUTOR,
            length: size_of::<AcpiMadtGenericDistributor>() as u8,
        },
        base_address: transmute!(GIC_DIST_START),
        version: gic_version,
        ..Default::default()
    };
    subtables.extend_from_slice(gicd.as_bytes());

    if gic == GicVersion::V3 {
        let gicr = AcpiMadtGenericRedistributor {
            header: AcpiSubtableHeader {
                type_: MADT_GENERIC_REDISTRIBUTOR,
                length: size_of::<AcpiMadtGenericRedistributor>() as u8,
            },
            base_address: transmute!(GIC_V3_REDIST_START),
            length: (mpidrs.len() as u32) * (128 << 10),
            ..Default::default()
        };
        subtables.extend_from_slice(gicr.as_bytes());
    }

    match msi {
        Some(MsiController::Its) => {
            let its = AcpiMadtGenericTranslator {
                header: AcpiSubtableHeader {
                    type_: MADT_GENERIC_TRANSLATOR,
                    length: size_of::<AcpiMadtGenericTranslator>() as u8,
                },
                translation_id: 0,
                base_address: transmute!(GIC_MSI_START),
                ..Default::default()
            };
            subtables.extend_from_slice(its.as_bytes());
        }
        Some(MsiController::GicV2m) => {
            let v2m = AcpiMadtGenericMsiFrame {
                header: AcpiSubtableHeader {
                    type_: MADT_GENERIC_MSI_FRAME,
                    length: size_of::<AcpiMadtGenericMsiFrame>() as u8,
                },
                msi_frame_id: 0,
                base_address: transmute!(GIC_MSI_START),
                ..Default::default()
            };
            subtables.extend_from_slice(v2m.as_bytes());
        }
        None => {}
    }

    for (index, mpidr) in mpidrs.iter().enumerate() {
        let gicc = AcpiMadtGenericInterrupt {
            header: AcpiSubtableHeader {
                type_: MADT_GENERIC_INTERRUPT,
                length: size_of::<AcpiMadtGenericInterrupt>() as u8,
            },
            cpu_interface_number: index as u32,
            uid: index as u32,
            flags: 1,
            base_address: transmute!(cpu_base),
            arm_mpidr: transmute!(*mpidr),
            ..Default::default()
        };
        subtables.extend_from_slice(gicc.as_bytes());
    }

    let total_length = size_of::<AcpiTableMadt>() + subtables.len();
    let mut madt = AcpiTableMadt {
        header: AcpiTableHeader {
            signature: SIG_MADT,
            length: total_length as u32,
            revision: MADT_REVISION,
            ..default_header()
        },
        address: 0,
        flags: 0,
    };
    let checksum = wrapping_sum(madt.as_bytes()).wrapping_add(wrapping_sum(&subtables));
    madt.header.checksum = 0u8.wrapping_sub(checksum);

    (madt, subtables)
}

#[cfg(target_arch = "aarch64")]
pub fn create_gtdt() -> AcpiTableGtdt {
    let flags = AcpiGtdtFlag::INTERRUPT_POLARITY_ACTIVE_LOW | AcpiGtdtFlag::ALWAYS_ON;
    let mut gtdt = AcpiTableGtdt {
        header: AcpiTableHeader {
            signature: SIG_GTDT,
            length: size_of::<AcpiTableGtdt>() as u32,
            revision: GTDT_REVISION,
            ..default_header()
        },
        counter_block_address: [u32::MAX; 2],
        secure_el1_interrupt: 16 + 13,
        secure_el1_flags: flags,
        non_secure_el1_interrupt: 16 + 14,
        non_secure_el1_flags: flags,
        virtual_timer_interrupt: 16 + 11,
        virtual_timer_flags: flags,
        non_secure_el2_interrupt: 16 + 10,
        non_secure_el2_flags: flags,
        counter_read_block_address: [u32::MAX; 2],
        virtual_el2_timer_gsiv: 16 + 12,
        virtual_el2_timer_flags: flags,
        ..Default::default()
    };
    gtdt.header.checksum = 0u8.wrapping_sub(wrapping_sum(gtdt.as_bytes()));
    gtdt
}

#[cfg(target_arch = "aarch64")]
pub fn create_spcr() -> AcpiTableSpcr {
    let mut spcr = AcpiTableSpcr {
        header: AcpiTableHeader {
            signature: SIG_SPCR,
            length: size_of::<AcpiTableSpcr>() as u32,
            revision: SPCR_REVISION,
            ..default_header()
        },
        interface_type: SPCR_INTERFACE_ARM_PL011,
        serial_port: AcpiGenericAddress {
            space_id: 0,
            bit_width: 32,
            bit_offset: 0,
            access_width: 3,
            address: transmute!(PL011_START),
        },
        interrupt_type: SPCR_INTERRUPT_TYPE_GIC,
        interrupt: (32u32 + 1).to_le_bytes(),
        baud_rate: 7,
        stop_bits: 1,
        flow_control: 0,
        terminal_type: 3,
        pci_device_id: 0xffff,
        pci_vendor_id: 0xffff,
        ..Default::default()
    };
    spcr.header.checksum = 0u8.wrapping_sub(wrapping_sum(spcr.as_bytes()));
    spcr
}

#[cfg(target_arch = "aarch64")]
pub fn create_iort() -> (AcpiTableIort, AcpiIortItsGroup1, AcpiIortRootComplex1) {
    let total_length = size_of::<AcpiTableIort>()
        + size_of::<AcpiIortItsGroup1>()
        + size_of::<AcpiIortRootComplex1>();
    let offset_its = size_of::<AcpiTableIort>() as u32;
    let its_group = AcpiIortItsGroup1 {
        node: AcpiIortNode {
            type_: IORT_NODE_ITS_GROUP,
            length: (size_of::<AcpiIortItsGroup1>() as u16).to_le_bytes(),
            revision: 1,
            identifier: 0,
            mapping_count: 0,
            mapping_offset: 0,
        },
        its_count: 1,
        identifiers: [0],
    };
    let rc = AcpiIortRootComplex1 {
        node: AcpiIortNode {
            type_: IORT_NODE_PCI_ROOT_COMPLEX,
            length: (size_of::<AcpiIortRootComplex1>() as u16).to_le_bytes(),
            revision: 4,
            identifier: 1,
            mapping_count: 1,
            mapping_offset: offset_of!(AcpiIortRootComplex1, mappings) as u32,
        },
        memory_properties: AcpiIortMemoryAccess {
            cache_coherency: 1,
            hints: 0,
            reserved: [0; 2],
            memory_flags: 0x3,
        },
        ats_attribute: 0,
        pci_segment_number: 0,
        memory_address_limit: 64,
        pasid_capabilities: [0; 2],
        reserved: 0,
        flags: 0,
        mappings: [AcpiIortIdMapping {
            input_base: 0,
            id_count: 0xffff,
            output_base: 0,
            output_reference: offset_its,
            flags: 0,
        }],
    };
    let mut iort = AcpiTableIort {
        header: AcpiTableHeader {
            signature: SIG_IORT,
            length: total_length as u32,
            revision: IORT_REVISION,
            ..default_header()
        },
        node_count: 2,
        node_offset: offset_its,
        reserved: 0,
    };
    let checksum = wrapping_sum(iort.as_bytes())
        .wrapping_add(wrapping_sum(its_group.as_bytes()))
        .wrapping_add(wrapping_sum(rc.as_bytes()));
    iort.header.checksum = 0u8.wrapping_sub(checksum);
    (iort, its_group, rc)
}

#[cfg(target_arch = "aarch64")]
pub fn create_pptt(topology: &CpuTopology) -> (AcpiTablePptt, Vec<AcpiPpttProcessor>) {
    let mut nodes = Vec::new();
    let mut current_offset = size_of::<AcpiTablePptt>() as u32;
    let node_size = size_of::<AcpiPpttProcessor>() as u32;

    for socket_id in 0..topology.sockets {
        let socket_offset = current_offset;
        nodes.push(AcpiPpttProcessor {
            header: AcpiSubtableHeader {
                type_: PPTT_TYPE_PROCESSOR,
                length: node_size as u8,
            },
            flags: AcpiPpttFlag::PHYSICAL_PACKAGE | AcpiPpttFlag::ACPI_IDENTICAL,
            parent: 0,
            acpi_processor_id: socket_id as u32,
            ..Default::default()
        });
        current_offset += node_size;

        for core_id in 0..topology.cores {
            if topology.smt {
                let core_offset = current_offset;
                nodes.push(AcpiPpttProcessor {
                    header: AcpiSubtableHeader {
                        type_: PPTT_TYPE_PROCESSOR,
                        length: node_size as u8,
                    },
                    flags: AcpiPpttFlag::ACPI_IDENTICAL,
                    parent: socket_offset,
                    acpi_processor_id: core_id as u32,
                    ..Default::default()
                });
                current_offset += node_size;

                for thread_id in 0..2 {
                    let uid = topology.decode(socket_id, core_id, thread_id) as u32;
                    nodes.push(AcpiPpttProcessor {
                        header: AcpiSubtableHeader {
                            type_: PPTT_TYPE_PROCESSOR,
                            length: node_size as u8,
                        },
                        flags: AcpiPpttFlag::ACPI_PROCESSOR_ID_VALID
                            | AcpiPpttFlag::ACPI_PROCESSOR_IS_THREAD
                            | AcpiPpttFlag::ACPI_LEAF_NODE
                            | AcpiPpttFlag::ACPI_IDENTICAL,
                        parent: core_offset,
                        acpi_processor_id: uid,
                        ..Default::default()
                    });
                    current_offset += node_size;
                }
            } else {
                let uid = topology.decode(socket_id, core_id, 0) as u32;
                nodes.push(AcpiPpttProcessor {
                    header: AcpiSubtableHeader {
                        type_: PPTT_TYPE_PROCESSOR,
                        length: node_size as u8,
                    },
                    flags: AcpiPpttFlag::ACPI_PROCESSOR_ID_VALID
                        | AcpiPpttFlag::ACPI_LEAF_NODE
                        | AcpiPpttFlag::ACPI_IDENTICAL,
                    parent: socket_offset,
                    acpi_processor_id: uid,
                    ..Default::default()
                });
                current_offset += node_size;
            }
        }
    }

    let mut pptt = AcpiTablePptt {
        header: AcpiTableHeader {
            signature: SIG_PPTT,
            length: current_offset,
            revision: PPTT_REVISION,
            ..default_header()
        },
    };
    let checksum = wrapping_sum(pptt.as_bytes()).wrapping_add(wrapping_sum(nodes.as_bytes()));
    pptt.header.checksum = 0u8.wrapping_sub(checksum);
    (pptt, nodes)
}

pub struct AcpiTable {
    pub(crate) rsdp: AcpiTableRsdp,
    pub(crate) tables: Vec<u8>,
    pub(crate) table_pointers: Vec<usize>,
    pub(crate) table_checksums: Vec<(usize, usize)>,
}

impl AcpiTable {
    pub fn relocate(&mut self, table_addr: u64) {
        let old_addr: u64 = transmute!(self.rsdp.xsdt_physical_address);
        self.rsdp.xsdt_physical_address = transmute!(table_addr);

        for pointer in self.table_pointers.iter() {
            let (old_val, _) = u64::read_from_prefix(&self.tables[*pointer..]).unwrap();
            let new_val = old_val.wrapping_sub(old_addr).wrapping_add(table_addr);
            IntoBytes::write_to_prefix(&new_val, &mut self.tables[*pointer..]).unwrap();
        }
    }

    pub fn update_checksums(&mut self) {
        let sum = wrapping_sum(&self.rsdp.as_bytes()[0..20]);
        self.rsdp.checksum = self.rsdp.checksum.wrapping_sub(sum);
        let ext_sum = wrapping_sum(self.rsdp.as_bytes());
        self.rsdp.extended_checksum = self.rsdp.extended_checksum.wrapping_sub(ext_sum);

        for (start, len) in self.table_checksums.iter() {
            let sum = wrapping_sum(&self.tables[*start..(*start + *len)]);
            let checksum = &mut self.tables[start + offset_of!(AcpiTableHeader, checksum)];
            *checksum = checksum.wrapping_sub(sum);
        }
    }

    pub fn clear_checksums(&mut self) {
        for (start, _) in self.table_checksums.iter() {
            let checksum = &mut self.tables[start + offset_of!(AcpiTableHeader, checksum)];
            *checksum = 0;
        }
        self.rsdp.checksum = 0;
        self.rsdp.extended_checksum = 0;
    }

    pub fn rsdp(&self) -> &AcpiTableRsdp {
        &self.rsdp
    }

    pub fn tables(&self) -> &[u8] {
        &self.tables
    }

    pub fn pointers(&self) -> &[usize] {
        &self.table_pointers
    }

    pub fn checksums(&self) -> &[(usize, usize)] {
        &self.table_checksums
    }

    pub fn take(self) -> (AcpiTableRsdp, Vec<u8>) {
        (self.rsdp, self.tables)
    }
}
