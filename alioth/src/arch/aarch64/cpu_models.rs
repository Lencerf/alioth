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

use super::reg::SReg;

#[derive(Debug, Clone)]
pub struct CpuModel {
    pub name: String,
    pub midr: u64,
    pub id_regs: Vec<(SReg, u64)>,
}

#[derive(serde::Deserialize)]
pub struct YamlCpuModel {
    pub name: String,
    pub midr: u64,
    pub id_regs: HashMap<String, u64>,
}

fn parse_sreg(name: &str) -> Option<SReg> {
    match name {
        "ID_AA64PFR0_EL1" => Some(SReg::ID_AA64PFR0_EL1),
        "ID_AA64PFR1_EL1" => Some(SReg::ID_AA64PFR1_EL1),
        "ID_AA64DFR0_EL1" => Some(SReg::ID_AA64DFR0_EL1),
        "ID_AA64ISAR0_EL1" => Some(SReg::ID_AA64ISAR0_EL1),
        "ID_AA64ISAR1_EL1" => Some(SReg::ID_AA64ISAR1_EL1),
        "ID_AA64MMFR0_EL1" => Some(SReg::ID_AA64MMFR0_EL1),
        "ID_AA64MMFR1_EL1" => Some(SReg::ID_AA64MMFR1_EL1),
        _ => None,
    }
}

impl TryFrom<YamlCpuModel> for CpuModel {
    type Error = String;
    fn try_from(y: YamlCpuModel) -> Result<Self, Self::Error> {
        let mut id_regs = Vec::new();
        for (reg_name, val) in y.id_regs {
            if let Some(sreg) = parse_sreg(&reg_name) {
                id_regs.push((sreg, val));
            } else {
                return Err(format!("Unknown SReg: {}", reg_name));
            }
        }
        Ok(CpuModel {
            name: y.name,
            midr: y.midr,
            id_regs,
        })
    }
}
