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

pub const CPU_MODELS: &[CpuModel] = &[CpuModel {
    name: "EPYC-Genoa",
    level: 0xd,
    xlevel: 0x80000021,                           // Use what we support
    vendor: [0x68747541, 0x444d4163, 0x69746e65], // AuthenticAMD
    family: 25,
    model: 17,
    stepping: 0,
    model_id: "AMD EPYC-Genoa Processor",
    features: &[
        // Base features for Genoa (subset)
        "fpu",
        "vme",
        "de",
        "pse",
        "tsc",
        "msr",
        "pae",
        "mce",
        "cx8",
        "apic",
        "sep",
        "mtrr",
        "pge",
        "mca",
        "cmov",
        "pat",
        "pse36",
        "clflush",
        "mmx",
        "fxsr",
        "sse",
        "sse2",
        "pni",
        "pclmulqdq",
        "monitor",
        "ssse3",
        "fma",
        "cx16",
        "sse4.1",
        "sse4.2",
        "movbe",
        "popcnt",
        "aes",
        "xsave",
        "avx",
        "f16c",
        "rdrand",
        "nx",
        "fxsr-opt",
        "pdpe1gb",
        "rdtscp",
        "lm",
        "lahf-lm",
        "cmp-legacy",
        "svm",
        "abm",
        "sse4a",
        "misalignsse",
        "3dnowprefetch",
        "osvw",
        "topoext",
        "perfctr-core",
        "clzero",
        "xsaveerptr",
        "wbnoinvd",
        "amd-ibpb",
        "amd-ibrs",
        "amd-stibp",
        "amd-ssbd",
        "psfd",
        "no-nested-data-bp",
        "lfence-always-serializing",
        "null-sel-clr-base",
        "auto-ibrs",
        "fsgsbase",
        "bmi1",
        "avx2",
        "smep",
        "bmi2",
        "erms",
        "invpcid",
        "avx512f",
        "avx512dq",
        "rdseed",
        "adx",
        "smap",
        "avx512ifma",
        "clflushopt",
        "clwb",
        "avx512cd",
        "sha-ni",
        "avx512bw",
        "avx512vl",
        "avx512vbmi",
        "umip",
        "pku",
        "avx512vbmi2",
        "gfni",
        "vaes",
        "vpclmulqdq",
        "avx512vnni",
        "avx512bitalg",
        "avx512-vpopcntdq",
        "la57",
        "rdpid",
        "fsrm",
        "xsaveopt",
        "xsavec",
        "xgetbv1",
        "xsaves",
        "arat",
        "npt",
        "lbrv",
        "svm-lock",
        "nripsave",
        "tsc-scale",
        "vmcb-clean",
        "flushbyasid",
        "v-vmsave-vmload",
        "vgif",
        "vnmi",
    ],
    versions: &[
        CpuVersion {
            version: 1,
            props: &[],
            model_id: None,
        },
        CpuVersion {
            version: 2,
            props: &[
                ("avx512-bf16", true), // Genoa-v2 adds BF16 in our definition
            ],
            model_id: Some("AMD EPYC-Genoa-v2 Processor"),
        },
    ],
}];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arch::x86_64::features::FEATURE_WORDS;

    fn feature_exists(name: &str) -> bool {
        FEATURE_WORDS.iter().any(|w| w.names.contains(&name))
    }

    #[test]
    fn test_cpu_models_sanity() {
        for model in CPU_MODELS {
            // Check base features are valid
            for &feat in model.features {
                assert!(
                    feature_exists(feat),
                    "Model {} has unknown base feature: {}",
                    model.name,
                    feat
                );
            }

            // Check version properties are valid
            let mut last_version = 0;
            for vdef in model.versions {
                assert!(
                    vdef.version > last_version,
                    "Model {} has non-monotonic version: {}",
                    model.name,
                    vdef.version
                );
                last_version = vdef.version;

                for &(prop, _) in vdef.props {
                    assert!(
                        feature_exists(prop),
                        "Model {} v{} has unknown property: {}",
                        model.name,
                        vdef.version,
                        prop
                    );
                }
            }
        }
    }
}
