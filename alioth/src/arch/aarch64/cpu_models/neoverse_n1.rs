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

use super::super::reg::SReg;
use super::CpuModel;

pub const NEOVERSE_N1: CpuModel = CpuModel {
    name: "neoverse-n1",
    midr: 0x414fd0c1,
    id_regs: &[
        (SReg::ID_AA64ISAR0_EL1, 0x0000100010211120),
        (SReg::ID_AA64ISAR1_EL1, 0x0000000000100001),
    ],
};
