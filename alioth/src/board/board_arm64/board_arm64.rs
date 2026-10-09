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

use std::collections::HashMap;
use std::mem::{offset_of, size_of, size_of_val};
use std::sync::Arc;

use parking_lot::Mutex;
use zerocopy::{FromBytes, FromZeros, Immutable, IntoBytes};

use crate::arch::layout::{
    ACPI_START, DEVICE_TREE_LIMIT, DEVICE_TREE_START, GIC_DIST_START, GIC_MSI_START,
    GIC_V2_CPU_INTERFACE_START, GIC_V3_REDIST_START, IO_END, IO_START, KERNEL_IMAGE_START,
    MEM_64_START, PCIE_CONFIG_START, PCIE_MMIO_32_NON_PREFETCHABLE_END,
    PCIE_MMIO_32_NON_PREFETCHABLE_START, PCIE_MMIO_32_PREFETCHABLE_END,
    PCIE_MMIO_32_PREFETCHABLE_START, PL011_START, PL031_START, RAM_32_SIZE, RAM_32_START,
    UEFI_START,
};
use crate::arch::reg::MpidrEl1;
use crate::board::{Board, BoardSpec, CpuTopology, PCIE_MMIO_64_SIZE, Result};
use crate::firmware::acpi::bindings::{
    AcpiTableFadt, AcpiTableHeader, AcpiTableRsdp, AcpiTableXsdt6, AcpiTableXsdt7,
};
use crate::firmware::acpi::{
    AcpiTable, GicVersion, MsiController, create_fadt, create_gtdt, create_iort, create_madt,
    create_mcfg, create_pptt, create_rsdp, create_spcr, create_xsdt6, create_xsdt7,
};
use crate::firmware::dt::{DeviceTree, Node, PropVal};
use crate::firmware::uefi::{
    ACPI_20_TABLE_GUID, EFI_2_100_SYSTEM_TABLE_REVISION, EFI_MEMORY_DESCRIPTOR_VERSION,
    EFI_RT_PROPERTIES_TABLE_GUID, EFI_RT_PROPERTIES_TABLE_VERSION, EFI_SYSTEM_TABLE_SIGNATURE,
    EfiConfigTable64, EfiMemoryAttribute, EfiMemoryDesc, EfiMemoryType, EfiRtPropertiesTable,
    EfiSystemTable64, EfiTableHeader,
};
use crate::hv::{GicV2, GicV2m, GicV3, Hypervisor, Its, Vm};
use crate::loader::{Executable, InitState};
use crate::mem::{MemRange, MemRegion, MemRegionEntry, MemRegionType};
use crate::utils::wrapping_sum;

#[repr(C)]
#[derive(Debug, Clone, FromBytes, Immutable, IntoBytes)]
struct StaticUefiTables {
    systab: EfiSystemTable64,
    rt_prop: EfiRtPropertiesTable,
    fw_vendor: [u16; 8],
    config_tables: [EfiConfigTable64; 2],
}

enum Gic<V>
where
    V: Vm,
{
    V2(V::GicV2),
    V3(V::GicV3),
}

enum Msi<V>
where
    V: Vm,
{
    V2m(V::GicV2m),
    Its(V::Its),
}

pub struct ArchBoard<V>
where
    V: Vm,
{
    gic: Gic<V>,
    msi: Option<Msi<V>>,
}

impl<V: Vm> ArchBoard<V> {
    pub fn new<H>(_hv: &H, vm: &V, spec: &BoardSpec) -> Result<Self>
    where
        H: Hypervisor<Vm = V>,
    {
        let gic = match vm.create_gic_v3(GIC_DIST_START, GIC_V3_REDIST_START, spec.cpu.count) {
            Ok(v3) => Gic::V3(v3),
            Err(e) => {
                log::error!("Cannot create GIC v3: {e:?}trying v2...");
                Gic::V2(vm.create_gic_v2(GIC_DIST_START, GIC_V2_CPU_INTERFACE_START)?)
            }
        };

        let create_gic_v2m = || match vm.create_gic_v2m(GIC_MSI_START) {
            Ok(v2m) => Some(Msi::V2m(v2m)),
            Err(e) => {
                log::error!("Cannot create GIC v2m: {e:?}");
                None
            }
        };

        let msi = if matches!(gic, Gic::V3(_)) {
            match vm.create_its(GIC_MSI_START) {
                Ok(its) => Some(Msi::Its(its)),
                Err(e) => {
                    log::error!("Cannot create ITS: {e:?}trying v2m...");
                    create_gic_v2m()
                }
            }
        } else {
            create_gic_v2m()
        };

        Ok(ArchBoard { gic, msi })
    }
}

fn encode_mpidr(topology: &CpuTopology, index: u16) -> MpidrEl1 {
    let (socket_id, core_id, thread_id) = topology.encode(index);
    let mut mpidr = MpidrEl1(0);
    mpidr.set_aff0(thread_id);
    mpidr.set_aff1(core_id as u8);
    mpidr.set_aff2(socket_id);
    mpidr
}

fn create_dsdt(spec: &BoardSpec) -> Vec<u8> {
    let mut dsdt = Vec::from(DSDT_TEMPLATE);
    let pcie_mmio_64_start = spec.pcie_mmio_64_start();
    let pcie_mmio_64_max = pcie_mmio_64_start - 1 + PCIE_MMIO_64_SIZE;
    dsdt[DSDT_OFFSET_PCI_QWORD_MEM..(DSDT_OFFSET_PCI_QWORD_MEM + 8)]
        .copy_from_slice(&pcie_mmio_64_start.to_le_bytes());
    dsdt[(DSDT_OFFSET_PCI_QWORD_MEM + 8)..(DSDT_OFFSET_PCI_QWORD_MEM + 16)]
        .copy_from_slice(&pcie_mmio_64_max.to_le_bytes());
    for index in 0..spec.cpu.count {
        let mut cpu_aml = AML_CPU_TEMPLATE;
        let name = format!("C{index:03X}");
        cpu_aml[8..12].copy_from_slice(name.as_bytes());
        cpu_aml[33..35].copy_from_slice(&index.to_le_bytes());
        dsdt.extend_from_slice(&cpu_aml);
    }
    let len = dsdt.len() as u32;
    let len_offset = offset_of!(AcpiTableHeader, length);
    dsdt[len_offset..(len_offset + 4)].copy_from_slice(&len.to_le_bytes());
    let checksum_offset = offset_of!(AcpiTableHeader, checksum);
    dsdt[checksum_offset] = 0;
    let sum = wrapping_sum(&dsdt);
    dsdt[checksum_offset] = 0u8.wrapping_sub(sum);
    dsdt
}

fn create_acpi(
    spec: &BoardSpec,
    gic: GicVersion,
    msi: Option<MsiController>,
) -> AcpiTable {
    let mut table_bytes = Vec::new();
    let mut pointers = vec![];
    let mut checksums = vec![];
    let has_its = msi == Some(MsiController::Its);

    let offset_xsdt = 0;
    if has_its {
        let xsdt = AcpiTableXsdt7::new_zeroed();
        table_bytes.extend(xsdt.as_bytes());
    } else {
        let xsdt = AcpiTableXsdt6::new_zeroed();
        table_bytes.extend(xsdt.as_bytes());
    }

    let offset_dsdt = table_bytes.len();
    let dsdt = create_dsdt(spec);
    table_bytes.extend(dsdt);
    table_bytes.resize(table_bytes.len().next_multiple_of(4), 0);

    let offset_fadt = table_bytes.len();
    debug_assert_eq!(offset_fadt % 4, 0);
    let fadt = create_fadt(offset_dsdt as u64);
    let pointer_fadt_to_dsdt = offset_fadt + offset_of!(AcpiTableFadt, xdsdt);
    table_bytes.extend(fadt.as_bytes());
    pointers.push(pointer_fadt_to_dsdt);
    checksums.push((offset_fadt, size_of_val(&fadt)));

    let offset_madt = table_bytes.len();
    debug_assert_eq!(offset_madt % 4, 0);
    let mpidrs: Vec<u64> = (0..spec.cpu.count)
        .map(|index| encode_mpidr(&spec.cpu.topology, index).0)
        .collect();
    let (madt, madt_subtables) = create_madt(&mpidrs, gic, msi);
    table_bytes.extend(madt.as_bytes());
    table_bytes.extend(madt_subtables);

    let offset_mcfg = table_bytes.len();
    debug_assert_eq!(offset_mcfg % 4, 0);
    let mcfg = create_mcfg();
    table_bytes.extend(mcfg.as_bytes());

    let offset_gtdt = table_bytes.len();
    debug_assert_eq!(offset_gtdt % 4, 0);
    let gtdt = create_gtdt();
    table_bytes.extend(gtdt.as_bytes());

    let offset_spcr = table_bytes.len();
    debug_assert_eq!(offset_spcr % 4, 0);
    let spcr = create_spcr();
    table_bytes.extend(spcr.as_bytes());

    let offset_pptt = table_bytes.len();
    debug_assert_eq!(offset_pptt % 4, 0);
    let (pptt, pptt_nodes) = create_pptt(&spec.cpu.topology);
    table_bytes.extend(pptt.as_bytes());
    for node in pptt_nodes {
        table_bytes.extend(node.as_bytes());
    }

    if has_its {
        let offset_iort = table_bytes.len();
        debug_assert_eq!(offset_iort % 4, 0);
        let (iort, its_group, rc) = create_iort();
        table_bytes.extend(iort.as_bytes());
        table_bytes.extend(its_group.as_bytes());
        table_bytes.extend(rc.as_bytes());

        let xsdt_entries = [
            offset_fadt as u64,
            offset_madt as u64,
            offset_mcfg as u64,
            offset_gtdt as u64,
            offset_spcr as u64,
            offset_pptt as u64,
            offset_iort as u64,
        ];
        let xsdt = create_xsdt7(xsdt_entries);
        xsdt.write_to_prefix(&mut table_bytes).unwrap();
        for index in 0..xsdt_entries.len() {
            pointers.push(offset_xsdt + offset_of!(AcpiTableXsdt7, entries) + index * 8);
        }
        checksums.push((offset_xsdt, size_of_val(&xsdt)));
    } else {
        let xsdt_entries = [
            offset_fadt as u64,
            offset_madt as u64,
            offset_mcfg as u64,
            offset_gtdt as u64,
            offset_spcr as u64,
            offset_pptt as u64,
        ];
        let xsdt = create_xsdt6(xsdt_entries);
        xsdt.write_to_prefix(&mut table_bytes).unwrap();
        for index in 0..xsdt_entries.len() {
            pointers.push(offset_xsdt + offset_of!(AcpiTableXsdt6, entries) + index * 8);
        }
        checksums.push((offset_xsdt, size_of_val(&xsdt)));
    }

    let rsdp = create_rsdp(offset_xsdt as u64);

    AcpiTable {
        rsdp,
        tables: table_bytes,
        table_checksums: checksums,
        table_pointers: pointers,
    }
}

fn create_uefi(mem_regions: &[(u64, MemRegionEntry)]) -> (StaticUefiTables, Vec<EfiMemoryDesc>) {
    let tables = StaticUefiTables {
        systab: EfiSystemTable64 {
            hdr: EfiTableHeader {
                signature: EFI_SYSTEM_TABLE_SIGNATURE,
                revision: EFI_2_100_SYSTEM_TABLE_REVISION,
                headersize: size_of::<EfiSystemTable64>() as u32,
                crc32: 0,
                reserved: 0,
            },
            fw_vendor: UEFI_START + offset_of!(StaticUefiTables, fw_vendor) as u64,
            fw_revision: 1,
            _pad: 0,
            con_in_handle: 0,
            con_in: 0,
            con_out_handle: 0,
            con_out: 0,
            stderr_handle: 0,
            stderr: 0,
            runtime: 0,
            boottime: 0,
            nr_tables: 2,
            tables: UEFI_START + offset_of!(StaticUefiTables, config_tables) as u64,
        },
        rt_prop: EfiRtPropertiesTable {
            version: EFI_RT_PROPERTIES_TABLE_VERSION,
            length: size_of::<EfiRtPropertiesTable>() as u16,
            runtime_services_supported: 0,
        },
        fw_vendor: [
            b'A' as u16,
            b'l' as u16,
            b'i' as u16,
            b'o' as u16,
            b't' as u16,
            b'h' as u16,
            0,
            0,
        ],
        config_tables: [
            EfiConfigTable64 {
                guid: ACPI_20_TABLE_GUID,
                table: ACPI_START,
            },
            EfiConfigTable64 {
                guid: EFI_RT_PROPERTIES_TABLE_GUID,
                table: UEFI_START + offset_of!(StaticUefiTables, rt_prop) as u64,
            },
        ],
    };

    let mut mmap = Vec::new();
    for (start, entry) in mem_regions {
        let ty = match entry.type_ {
            MemRegionType::Ram => EfiMemoryType::CONVENTIONAL_MEMORY,
            MemRegionType::Acpi => EfiMemoryType::ACPI_RECLAIM_MEMORY,
            _ => continue,
        };
        mmap.push(EfiMemoryDesc {
            ty,
            pad: 0,
            phys_addr: *start,
            virt_addr: *start,
            num_pages: entry.size >> 12,
            attribute: EfiMemoryAttribute::WB,
        });
    }

    (tables, mmap)
}

impl<V> Board<V>
where
    V: Vm,
{
    pub fn encode_cpu_identity(&self, index: u16) -> u64 {
        encode_mpidr(&self.spec.cpu.topology, index).0
    }

    pub fn create_ram(&self) -> Result<()> {
        let mem_size = self.spec.mem.size;
        let memory = &self.memory;

        let low_mem_size = std::cmp::min(mem_size, RAM_32_SIZE);
        let pages_low = self.create_ram_pages(low_mem_size, c"ram-low")?;
        let acpi_size = KERNEL_IMAGE_START - RAM_32_START;
        let region_low = if self.spec.acpi && low_mem_size > acpi_size {
            MemRegion {
                ranges: vec![MemRange::Ram(pages_low)],
                entries: vec![
                    MemRegionEntry {
                        size: acpi_size,
                        type_: MemRegionType::Acpi,
                    },
                    MemRegionEntry {
                        size: low_mem_size - acpi_size,
                        type_: MemRegionType::Ram,
                    },
                ],
                callbacks: Mutex::new(vec![]),
            }
        } else {
            MemRegion::with_ram(pages_low, MemRegionType::Ram)
        };
        memory.add_region(RAM_32_START, Arc::new(region_low))?;

        let high_mem_size = mem_size.saturating_sub(RAM_32_SIZE);
        if high_mem_size > 0 {
            let pages_high = self.create_ram_pages(high_mem_size, c"ram-high")?;
            memory.add_region(
                MEM_64_START,
                Arc::new(MemRegion::with_ram(pages_high, MemRegionType::Ram)),
            )?;
        }

        Ok(())
    }

    pub fn coco_init(&self) -> Result<()> {
        Ok(())
    }

    pub fn arch_init(&self) -> Result<()> {
        match &self.arch.gic {
            Gic::V2(v2) => v2.init(),
            Gic::V3(v3) => v3.init(),
        }?;
        match &self.arch.msi {
            Some(Msi::V2m(v2m)) => v2m.init(),
            Some(Msi::Its(its)) => its.init(),
            None => Ok(()),
        }?;
        Ok(())
    }

    fn create_chosen_node(&self, init_state: &InitState, uefi_mmap_size: Option<usize>, root: &mut Node) {
        let payload = self.payload.read();
        let Some(payload) = payload.as_ref() else {
            return;
        };
        if !matches!(payload.executable, Some(Executable::Linux(_))) {
            return;
        }
        let mut node = Node::default();
        if let Some(cmdline) = &payload.cmdline {
            let cmdline = cmdline.to_string();
            node.props.insert("bootargs", PropVal::String(cmdline));
        }
        if let Some(initramfs_range) = &init_state.initramfs {
            node.props.insert(
                "linux,initrd-start",
                PropVal::U64(initramfs_range.start),
            );
            node.props
                .insert("linux,initrd-end", PropVal::U64(initramfs_range.end));
        }
        if let Some(mmap_size) = uefi_mmap_size {
            let mmap_start = UEFI_START + size_of::<StaticUefiTables>() as u64;
            node.props
                .insert("linux,uefi-system-table", PropVal::U64(UEFI_START));
            node.props
                .insert("linux,uefi-mmap-start", PropVal::U64(mmap_start));
            node.props
                .insert("linux,uefi-mmap-size", PropVal::U32(mmap_size as u32));
            node.props.insert(
                "linux,uefi-mmap-desc-size",
                PropVal::U32(size_of::<EfiMemoryDesc>() as u32),
            );
            node.props.insert(
                "linux,uefi-mmap-desc-ver",
                PropVal::U32(EFI_MEMORY_DESCRIPTOR_VERSION),
            );
        } else {
            node.props.insert(
                "stdout-path",
                PropVal::String(format!("/pl011@{PL011_START:x}")),
            );
        }
        root.nodes.push(("chosen".to_owned(), node));
    }

    pub fn create_memory_node(&self, root: &mut Node) {
        let regions = self.memory.mem_region_entries();
        for (start, region) in regions {
            if region.type_ != MemRegionType::Ram {
                continue;
            };
            let node = Node {
                props: HashMap::from([
                    ("device_type", PropVal::Str("memory")),
                    ("reg", PropVal::U64List(vec![start, region.size])),
                ]),
                nodes: Vec::new(),
            };
            root.nodes.push((format!("memory@{start:x}"), node));
        }
    }

    pub fn create_cpu_nodes(&self, root: &mut Node) {
        let topology = &self.spec.cpu.topology;

        let thread_node = |socket_id: u8, core_id: u16, thread_id: u8| {
            let phandle = PHANDLE_CPU | topology.decode(socket_id, core_id, thread_id) as u32;
            Node {
                props: HashMap::from([("cpu", PropVal::PHandle(phandle))]),
                nodes: Vec::new(),
            }
        };
        let core_node = |socket_id: u8, core_id: u16| {
            if topology.smt {
                Node {
                    props: HashMap::new(),
                    nodes: vec![
                        ("thread0".to_owned(), thread_node(socket_id, core_id, 0)),
                        ("thread1".to_owned(), thread_node(socket_id, core_id, 1)),
                    ],
                }
            } else {
                thread_node(socket_id, core_id, 0)
            }
        };
        let socket_node = |socket_id: u8| Node {
            props: HashMap::new(),
            nodes: vec![(
                "cluster0".to_owned(),
                Node {
                    props: HashMap::new(),
                    nodes: (0..topology.cores)
                        .map(|core_id| (format!("core{core_id}"), core_node(socket_id, core_id)))
                        .collect(),
                },
            )],
        };
        let cpu_map_node = Node {
            props: HashMap::new(),
            nodes: (0..topology.sockets)
                .map(|socket_id| (format!("socket{socket_id}"), socket_node(socket_id)))
                .collect(),
        };

        let cpus_nodes = (0..(self.spec.cpu.count))
            .map(|index| {
                let mpidr = self.encode_cpu_identity(index);
                (
                    format!("cpu@{mpidr:x}"),
                    Node {
                        props: HashMap::from([
                            ("device_type", PropVal::Str("cpu")),
                            ("compatible", PropVal::Str("arm,arm-v8")),
                            ("enable-method", PropVal::Str("psci")),
                            ("reg", PropVal::U64(mpidr)),
                            ("phandle", PropVal::PHandle(PHANDLE_CPU | index as u32)),
                        ]),
                        nodes: Vec::new(),
                    },
                )
            })
            .chain([("cpu-map".to_owned(), cpu_map_node)])
            .collect();

        let cpus = Node {
            props: HashMap::from([
                ("#address-cells", PropVal::U32(2)),
                ("#size-cells", PropVal::U32(0)),
            ]),
            nodes: cpus_nodes,
        };
        root.nodes.push(("cpus".to_owned(), cpus));
    }

    fn create_clock_node(&self, root: &mut Node) {
        let node = Node {
            props: HashMap::from([
                ("compatible", PropVal::Str("fixed-clock")),
                ("clock-frequency", PropVal::U32(24000000)),
                ("clock-output-names", PropVal::Str("clk24mhz")),
                ("phandle", PropVal::PHandle(PHANDLE_CLOCK)),
                ("#clock-cells", PropVal::U32(0)),
            ]),
            nodes: Vec::new(),
        };
        root.nodes.push(("apb-pclk".to_owned(), node));
    }

    fn create_pl011_node(&self, root: &mut Node) {
        let pin = 1;
        let edge_trigger = 1;
        let spi = 0;
        let node = Node {
            props: HashMap::from([
                ("compatible", PropVal::Str("arm,primecell\0arm,pl011")),
                ("reg", PropVal::U64List(vec![PL011_START, 0x1000])),
                ("interrupts", PropVal::U32List(vec![spi, pin, edge_trigger])),
                ("clock-names", PropVal::Str("uartclk\0apb_pclk")),
                (
                    "clocks",
                    PropVal::U32List(vec![PHANDLE_CLOCK, PHANDLE_CLOCK]),
                ),
            ]),
            nodes: Vec::new(),
        };
        root.nodes.push((format!("pl011@{PL011_START:x}"), node));
    }

    fn create_pl031_node(&self, root: &mut Node) {
        let node = Node {
            props: HashMap::from([
                ("compatible", PropVal::Str("arm,primecell\0arm,pl031")),
                ("reg", PropVal::U64List(vec![PL031_START, 0x1000])),
                ("clock-names", PropVal::Str("apb_pclk")),
                ("clocks", PropVal::U32List(vec![PHANDLE_CLOCK])),
            ]),
            nodes: Vec::new(),
        };
        root.nodes.push((format!("pl031@{PL031_START:x}"), node));
    }

    // Documentation/devicetree/bindings/timer/arm,arch_timer.yaml
    fn create_timer_node(&self, root: &mut Node) {
        let mut interrupts = vec![];
        let irq_pins = [13, 14, 11, 10];
        let ppi = 1;
        let level_trigger = 4;
        let cpu_mask = match self.arch.gic {
            Gic::V2(_) => (1 << self.spec.cpu.count) - 1,
            Gic::V3 { .. } => 0,
        };
        for pin in irq_pins {
            interrupts.extend([ppi, pin, (cpu_mask << 8) | level_trigger]);
        }
        let node = Node {
            props: HashMap::from([
                ("compatible", PropVal::Str("arm,armv8-timer")),
                ("interrupts", PropVal::U32List(interrupts)),
                ("always-on", PropVal::Empty),
            ]),
            nodes: Vec::new(),
        };
        root.nodes.push(("timer".to_owned(), node));
    }

    fn create_gic_msi_node(&self) -> Vec<(String, Node)> {
        let Some(msi) = &self.arch.msi else {
            return Vec::new();
        };
        match msi {
            Msi::Its(_) => {
                let node = Node {
                    props: HashMap::from([
                        ("compatible", PropVal::Str("arm,gic-v3-its")),
                        ("msi-controller", PropVal::Empty),
                        ("#msi-cells", PropVal::U32(1)),
                        ("reg", PropVal::U64List(vec![GIC_MSI_START, 128 << 10])),
                        ("phandle", PropVal::PHandle(PHANDLE_MSI)),
                    ]),
                    nodes: Vec::new(),
                };
                vec![(format!("its@{GIC_MSI_START:x}"), node)]
            }
            Msi::V2m(_) => {
                let node = Node {
                    props: HashMap::from([
                        ("compatible", PropVal::Str("arm,gic-v2m-frame")),
                        ("msi-controller", PropVal::Empty),
                        ("reg", PropVal::U64List(vec![GIC_MSI_START, 64 << 10])),
                        ("phandle", PropVal::PHandle(PHANDLE_MSI)),
                    ]),
                    nodes: Vec::new(),
                };
                vec![(format!("v2m@{GIC_MSI_START:x}"), node)]
            }
        }
    }

    fn create_gic_node(&self, root: &mut Node) {
        let msi = self.create_gic_msi_node();
        let node = match self.arch.gic {
            // Documentation/devicetree/bindings/interrupt-controller/arm,gic.yaml
            Gic::V2(_) => Node {
                props: HashMap::from([
                    ("compatible", PropVal::Str("arm,cortex-a15-gic")),
                    ("#interrupt-cells", PropVal::U32(3)),
                    (
                        "reg",
                        PropVal::U64List(vec![
                            GIC_DIST_START,
                            0x1000,
                            GIC_V2_CPU_INTERFACE_START,
                            0x2000,
                        ]),
                    ),
                    ("phandle", PropVal::U32(PHANDLE_GIC)),
                    ("interrupt-controller", PropVal::Empty),
                ]),
                nodes: msi,
            },
            // Documentation/devicetree/bindings/interrupt-controller/arm,gic-v3.yaml
            Gic::V3(_) => Node {
                props: HashMap::from([
                    ("compatible", PropVal::Str("arm,gic-v3")),
                    ("#interrupt-cells", PropVal::U32(3)),
                    ("#address-cells", PropVal::U32(2)),
                    ("#size-cells", PropVal::U32(2)),
                    ("interrupt-controller", PropVal::Empty),
                    ("ranges", PropVal::Empty),
                    (
                        "reg",
                        PropVal::U64List(vec![
                            GIC_DIST_START,
                            64 << 10,
                            GIC_V3_REDIST_START,
                            self.spec.cpu.count as u64 * (128 << 10),
                        ]),
                    ),
                    ("phandle", PropVal::U32(PHANDLE_GIC)),
                ]),
                nodes: msi,
            },
        };
        root.nodes.push((format!("intc@{GIC_DIST_START:x}"), node));
    }

    // Documentation/devicetree/bindings/arm/psci.yaml
    fn create_psci_node(&self, root: &mut Node) {
        let node = Node {
            props: HashMap::from([
                ("method", PropVal::Str("hvc")),
                ("compatible", PropVal::Str("arm,psci-0.2\0arm,psci")),
            ]),
            nodes: Vec::new(),
        };
        root.nodes.push(("psci".to_owned(), node));
    }

    // https://elinux.org/Device_Tree_Usage#PCI_Host_Bridge
    // Documentation/devicetree/bindings/pci/host-generic-pci.yaml
    // IEEE Std 1275-1994
    fn create_pci_bridge_node(&self, root: &mut Node) {
        let Some(max_bus) = self.pci_bus.segment.max_bus() else {
            return;
        };
        let pcie_mmio_64_start = self.spec.pcie_mmio_64_start();
        let prefetchable = 1 << 30;
        let io = 0b01 << 24;
        let mem_32 = 0b10 << 24;
        let mem_64 = 0b11 << 24;
        let node = Node {
            props: HashMap::from([
                ("compatible", PropVal::Str("pci-host-ecam-generic")),
                ("device_type", PropVal::Str("pci")),
                ("reg", PropVal::U64List(vec![PCIE_CONFIG_START, 256 << 20])),
                ("bus-range", PropVal::U32List(vec![0, max_bus as u32])),
                ("#address-cells", PropVal::U32(3)),
                ("#size-cells", PropVal::U32(2)),
                (
                    "ranges",
                    PropVal::U32List(vec![
                        io,
                        0,
                        0,
                        0,
                        IO_START as u32,
                        0,
                        (IO_END - IO_START) as u32,
                        mem_32 | prefetchable,
                        0,
                        PCIE_MMIO_32_PREFETCHABLE_START as u32,
                        0,
                        PCIE_MMIO_32_PREFETCHABLE_START as u32,
                        0,
                        (PCIE_MMIO_32_PREFETCHABLE_END - PCIE_MMIO_32_PREFETCHABLE_START) as u32,
                        mem_32,
                        0,
                        PCIE_MMIO_32_NON_PREFETCHABLE_START as u32,
                        0,
                        PCIE_MMIO_32_NON_PREFETCHABLE_START as u32,
                        0,
                        (PCIE_MMIO_32_NON_PREFETCHABLE_END - PCIE_MMIO_32_NON_PREFETCHABLE_START)
                            as u32,
                        mem_64 | prefetchable,
                        (pcie_mmio_64_start >> 32) as u32,
                        pcie_mmio_64_start as u32,
                        (pcie_mmio_64_start >> 32) as u32,
                        pcie_mmio_64_start as u32,
                        (PCIE_MMIO_64_SIZE >> 32) as u32,
                        PCIE_MMIO_64_SIZE as u32,
                    ]),
                ),
                (
                    "msi-map",
                    // Identity map from RID (BDF) to msi-specifier.
                    // Documentation/devicetree/bindings/pci/pci-msi.txt
                    PropVal::U32List(vec![0, PHANDLE_MSI, 0, 0x10000]),
                ),
            ]),
            nodes: Vec::new(),
        };
        root.nodes
            .push((format!("pci@{PCIE_CONFIG_START:x}"), node));
    }

    pub fn create_firmware_data(&self, init_state: &InitState) -> Result<()> {
        let ram = self.memory.ram_bus();
        let mut device_tree = DeviceTree::new();
        let root = &mut device_tree.root;
        root.props.insert("#address-cells", PropVal::U32(2));
        root.props.insert("#size-cells", PropVal::U32(2));

        if self.spec.acpi {
            let gic = match self.arch.gic {
                Gic::V2(_) => GicVersion::V2,
                Gic::V3(_) => GicVersion::V3,
            };
            let msi = match &self.arch.msi {
                Some(Msi::Its(_)) => Some(MsiController::Its),
                Some(Msi::V2m(_)) => Some(MsiController::GicV2m),
                None => None,
            };
            let mut acpi_table = create_acpi(&self.spec, gic, msi);
            let tables_gpa = ACPI_START + size_of::<AcpiTableRsdp>() as u64;
            assert!(tables_gpa + acpi_table.tables().len() as u64 <= UEFI_START);
            acpi_table.relocate(tables_gpa);
            acpi_table.update_checksums();
            ram.write_range(
                ACPI_START,
                size_of::<AcpiTableRsdp>() as u64,
                acpi_table.rsdp().as_bytes(),
            )?;
            ram.write_range(
                tables_gpa,
                acpi_table.tables().len() as u64,
                acpi_table.tables(),
            )?;

            let mem_regions = self.memory.mem_region_entries();
            let (uefi_tables, mmap) = create_uefi(&mem_regions);
            let mmap_gpa = UEFI_START + size_of::<StaticUefiTables>() as u64;
            let mmap_bytes = mmap.as_bytes();
            assert!(mmap_gpa + mmap_bytes.len() as u64 <= KERNEL_IMAGE_START);
            ram.write_range(
                UEFI_START,
                size_of::<StaticUefiTables>() as u64,
                uefi_tables.as_bytes(),
            )?;
            ram.write_range(mmap_gpa, mmap_bytes.len() as u64, mmap_bytes)?;

            self.create_chosen_node(init_state, Some(mmap_bytes.len()), root);
        } else {
            root.props.insert("model", PropVal::Str("linux,dummy-virt"));
            root.props
                .insert("compatible", PropVal::Str("linux,dummy-virt"));
            root.props
                .insert("interrupt-parent", PropVal::PHandle(PHANDLE_GIC));

            self.create_chosen_node(init_state, None, root);
            self.create_pl011_node(root);
            self.create_pl031_node(root);
            self.create_memory_node(root);
            self.create_cpu_nodes(root);
            self.create_gic_node(root);
            if self.arch.msi.is_some() {
                self.create_pci_bridge_node(root);
            }
            self.create_clock_node(root);
            self.create_timer_node(root);
            self.create_psci_node(root);
        }

        log::debug!("device tree: {device_tree:#x?}");
        let blob = device_tree.to_blob();
        assert!(blob.len() as u64 <= DEVICE_TREE_LIMIT);
        ram.write_range(DEVICE_TREE_START, blob.len() as u64, &*blob)?;
        Ok(())
    }
}

const PHANDLE_GIC: u32 = 1;
const PHANDLE_CLOCK: u32 = 2;
const PHANDLE_MSI: u32 = 3;
const PHANDLE_CPU: u32 = 1 << 31;

const DSDT_TEMPLATE: [u8; 363] = [
    0x44, 0x53, 0x44, 0x54, 0x6B, 0x01, 0x00, 0x00, 0x02, 0xCD, 0x41, 0x4C, 0x49, 0x4F, 0x54, 0x48,
    0x41, 0x4C, 0x49, 0x4F, 0x54, 0x48, 0x56, 0x4D, 0x01, 0x00, 0x00, 0x00, 0x49, 0x4E, 0x54, 0x4C,
    0x12, 0x12, 0x25, 0x20, 0x5B, 0x82, 0x47, 0x04, 0x2E, 0x5F, 0x53, 0x42, 0x5F, 0x43, 0x4F, 0x4D,
    0x30, 0x08, 0x5F, 0x48, 0x49, 0x44, 0x0D, 0x41, 0x52, 0x4D, 0x48, 0x30, 0x30, 0x31, 0x31, 0x00,
    0x08, 0x5F, 0x55, 0x49, 0x44, 0x00, 0x08, 0x5F, 0x53, 0x54, 0x41, 0x0A, 0x0F, 0x08, 0x5F, 0x43,
    0x52, 0x53, 0x11, 0x1A, 0x0A, 0x17, 0x86, 0x09, 0x00, 0x01, 0x00, 0xF0, 0xFF, 0x2F, 0x00, 0x10,
    0x00, 0x00, 0x89, 0x06, 0x00, 0x03, 0x01, 0x21, 0x00, 0x00, 0x00, 0x79, 0x00, 0x5B, 0x82, 0x4C,
    0x0F, 0x2E, 0x5F, 0x53, 0x42, 0x5F, 0x50, 0x43, 0x49, 0x30, 0x08, 0x5F, 0x48, 0x49, 0x44, 0x0C,
    0x41, 0xD0, 0x0A, 0x08, 0x08, 0x5F, 0x43, 0x49, 0x44, 0x0C, 0x41, 0xD0, 0x0A, 0x03, 0x08, 0x5F,
    0x53, 0x45, 0x47, 0x00, 0x08, 0x5F, 0x55, 0x49, 0x44, 0x00, 0x08, 0x5F, 0x43, 0x43, 0x41, 0x01,
    0x14, 0x32, 0x5F, 0x44, 0x53, 0x4D, 0x04, 0xA0, 0x29, 0x93, 0x68, 0x11, 0x13, 0x0A, 0x10, 0xD0,
    0x37, 0xC9, 0xE5, 0x53, 0x35, 0x7A, 0x4D, 0x91, 0x17, 0xEA, 0x4D, 0x19, 0xC3, 0x43, 0x4D, 0xA0,
    0x09, 0x93, 0x6A, 0x00, 0xA4, 0x11, 0x03, 0x01, 0x21, 0xA0, 0x07, 0x93, 0x6A, 0x0A, 0x05, 0xA4,
    0x00, 0xA4, 0x00, 0x08, 0x5F, 0x43, 0x52, 0x53, 0x11, 0x42, 0x09, 0x0A, 0x8E, 0x88, 0x0D, 0x00,
    0x02, 0x0C, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x87, 0x17, 0x00,
    0x00, 0x0C, 0x07, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xC0, 0xFF, 0xFF, 0xFF, 0xDF, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x20, 0x87, 0x17, 0x00, 0x00, 0x0C, 0x01, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0xE0, 0xFF, 0xFF, 0xFF, 0xFF, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x20, 0x8A, 0x2B, 0x00, 0x00, 0x0C, 0x07, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0x00, 0x01, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x87,
    0x17, 0x00, 0x01, 0x0C, 0x13, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xFF, 0xFF, 0x00,
    0x00, 0x00, 0x00, 0xFF, 0x0F, 0x00, 0x00, 0x01, 0x00, 0x79, 0x00,
];

const DSDT_OFFSET_PCI_QWORD_MEM: usize = 0x12f;

const AML_CPU_TEMPLATE: [u8; 42] = [
    0x5B, 0x82, 0x28, 0x2E, 0x5F, 0x53, 0x42, 0x5F, // Device (_SB.C000)
    0x43, 0x30, 0x30, 0x30, // "C000"
    0x08, 0x5F, 0x48, 0x49, 0x44, 0x0D, 0x41, 0x43, 0x50, 0x49, 0x30, 0x30, 0x30, 0x37,
    0x00, // Name (_HID, "ACPI0007")
    0x08, 0x5F, 0x55, 0x49, 0x44, 0x0B, 0x00, 0x00, // Name (_UID, 0x0000)
    0x08, 0x5F, 0x53, 0x54, 0x41, 0x0A, 0x0F, // Name (_STA, 0x0F)
];

#[cfg(test)]
#[path = "board_arm64_test.rs"]
mod tests;
