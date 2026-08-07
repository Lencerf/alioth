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

#[cfg(target_arch = "x86_64")]
mod amd64 {
    use alioth::arch::cpuid::CpuidIn;
    use alioth::arch::x86_64::features::{CpuidReg, FEATURE_WORDS};
    use alioth::hv::{Kvm, KvmSpec};
    use std::arch::x86_64::__cpuid_count;

    #[derive(serde::Serialize)]
    struct YamlCpuModel {
        name: &'static str,
        level: u32,
        xlevel: u32,
        vendor: [u32; 3],
        family: u32,
        model: u32,
        stepping: u32,
        model_id: String,
        features: Vec<&'static str>,
        versions: Vec<YamlCpuVersion>,
    }

    #[derive(serde::Serialize)]
    struct YamlCpuVersion {
        version: u32,
        props: Vec<(&'static str, bool)>,
        model_id: Option<String>,
    }

    fn get_reg_val(res: &std::arch::x86_64::CpuidResult, reg: CpuidReg) -> u32 {
        match reg {
            CpuidReg::Eax => res.eax,
            CpuidReg::Ebx => res.ebx,
            CpuidReg::Ecx => res.ecx,
            CpuidReg::Edx => res.edx,
        }
    }

    pub fn detect() {
        let kvm = Kvm::new(KvmSpec::default()).expect("Failed to open KVM");
        let host_supported = kvm
            .get_supported_cpuids(None)
            .expect("Failed to get supported CPUIDs");

        let res0 = __cpuid_count(0, 0);
        let level = res0.eax;
        let vendor = [res0.ebx, res0.edx, res0.ecx];

        let res1 = __cpuid_count(1, 0);
        let stepping = res1.eax & 0xf;
        let model = (res1.eax >> 4) & 0xf;
        let family = (res1.eax >> 8) & 0xf;
        let ext_model = (res1.eax >> 16) & 0xf;
        let ext_family = (res1.eax >> 20) & 0xff;

        let actual_family = if family == 0xf {
            family + ext_family
        } else {
            family
        };
        let actual_model = if family == 0x6 || family == 0xf {
            model | (ext_model << 4)
        } else {
            model
        };

        let res_ext = __cpuid_count(0x8000_0000, 0);
        let xlevel = res_ext.eax;

        let mut brand = Vec::new();
        for func in 0x8000_0002..=0x8000_0004 {
            let res = __cpuid_count(func, 0);
            brand.extend_from_slice(&res.eax.to_le_bytes());
            brand.extend_from_slice(&res.ebx.to_le_bytes());
            brand.extend_from_slice(&res.ecx.to_le_bytes());
            brand.extend_from_slice(&res.edx.to_le_bytes());
        }
        let brand_str = String::from_utf8_lossy(&brand);
        let brand_str = brand_str.trim().replace('\0', "").to_owned();

        let mut enabled_features = Vec::new();
        for info in FEATURE_WORDS {
            let leaf = CpuidIn {
                func: info.func,
                index: info.index,
            };
            if let Some(res) = host_supported.get(&leaf) {
                let reg_val = get_reg_val(res, info.reg);
                for bit in 0..32 {
                    let name = info.names[bit as usize];
                    if name.is_empty() {
                        continue;
                    }
                    if (reg_val & (1 << bit)) != 0 {
                        enabled_features.push(name);
                    }
                }
            }
        }

        let model = YamlCpuModel {
            name: "detected-host",
            level,
            xlevel,
            vendor,
            family: actual_family,
            model: actual_model,
            stepping,
            model_id: brand_str,
            features: enabled_features,
            versions: vec![YamlCpuVersion {
                version: 1,
                props: vec![],
                model_id: None,
            }],
        };

        let yaml = serde_yml::to_string(&model).expect("Failed to serialize YAML");
        println!("{}", yaml);
    }
}

#[cfg(target_arch = "aarch64")]
mod arm64 {
    use std::collections::BTreeMap;

    use alioth::arch::reg::SReg;
    use alioth::hv::{Hypervisor, Kvm, KvmSpec, Vcpu, Vm, VmSpec};

    #[derive(serde::Serialize)]
    struct YamlCpuModel {
        name: &'static str,
        midr: u64,
        id_regs: BTreeMap<&'static str, u64>,
    }

    const SREGS_TO_READ: &[(SReg, &'static str)] = &[
        (SReg::ID_AA64PFR0_EL1, "ID_AA64PFR0_EL1"),
        (SReg::ID_AA64PFR1_EL1, "ID_AA64PFR1_EL1"),
        (SReg::ID_AA64DFR0_EL1, "ID_AA64DFR0_EL1"),
        (SReg::ID_AA64ISAR0_EL1, "ID_AA64ISAR0_EL1"),
        (SReg::ID_AA64ISAR1_EL1, "ID_AA64ISAR1_EL1"),
        (SReg::ID_AA64MMFR0_EL1, "ID_AA64MMFR0_EL1"),
        (SReg::ID_AA64MMFR1_EL1, "ID_AA64MMFR1_EL1"),
    ];

    pub fn detect() {
        let kvm = Kvm::new(KvmSpec::default()).expect("Failed to open KVM");
        let vm = kvm
            .create_vm(&VmSpec { coco: None })
            .expect("Failed to create VM");
        let mut vcpu = vm.create_vcpu(0, 0).expect("Failed to create VCPU");
        vcpu.reset(true).expect("Failed to reset VCPU");

        let midr = vcpu
            .get_sreg(SReg::MIDR_EL1)
            .expect("Failed to read MIDR_EL1");

        let mut id_regs = BTreeMap::new();
        for &(sreg, name) in SREGS_TO_READ {
            match vcpu.get_sreg(sreg) {
                Ok(val) => {
                    id_regs.insert(name, val);
                }
                Err(e) => {
                    eprintln!("Warning: Failed to read {}: {:?}", name, e);
                }
            }
        }

        let model = YamlCpuModel {
            name: "detected-host",
            midr,
            id_regs,
        };

        let yaml = serde_yml::to_string(&model).expect("Failed to serialize YAML");
        println!("{}", yaml);
    }
}

fn main() {
    #[cfg(target_arch = "x86_64")]
    amd64::detect();

    #[cfg(target_arch = "aarch64")]
    arm64::detect();

    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    println!("Unsupported architecture");
}
