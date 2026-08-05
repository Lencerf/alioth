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

pub mod epyc_genoa;

pub struct CpuModel {
    pub name: &'static str,
    pub level: u32,
    pub xlevel: u32,
    pub vendor: [u32; 3],
    pub family: u32,
    pub model: u32,
    pub stepping: u32,
    pub model_id: &'static str,
    pub features: &'static [&'static str],
    pub versions: &'static [CpuVersion],
}

pub struct CpuVersion {
    pub version: u32,
    pub props: &'static [(&'static str, bool)], // Feature overrides
    pub model_id: Option<&'static str>,
}

use self::epyc_genoa::EPYC_GENOA;

pub const CPU_MODELS: &[CpuModel] = &[EPYC_GENOA];

#[cfg(test)]
#[path = "cpu_models_test.rs"]
mod tests;
