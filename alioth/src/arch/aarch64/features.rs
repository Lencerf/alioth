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

use super::reg::SReg;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CpuFeature {
    pub name: &'static str,
    pub sreg: SReg,
    pub shift: u8,
    pub width: u8,
    pub value: u64,
}

pub const CPU_FEATURES: &[CpuFeature] = &[
    CpuFeature {
        name: "sve",
        sreg: SReg::ID_AA64PFR0_EL1,
        shift: 32,
        width: 4,
        value: 1,
    },
];


