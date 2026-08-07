// Copyright 2025 Google LLC
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

use rstest::rstest;

use crate::board::CpuTopology;
use crate::board::x86_64::encode_x2apic_id;

#[rstest]
#[case(CpuTopology{smt: false, cores: 1, sockets: 1, ..Default::default()}, 0, 0)]
#[case(CpuTopology{smt: true, cores: 2, sockets: 1, thread_contiguous: false}, 0, 0)]
#[case(CpuTopology{smt: true, cores: 2, sockets: 1, thread_contiguous: false}, 1, 2)]
#[case(CpuTopology{smt: true, cores: 2, sockets: 1, thread_contiguous: false}, 2, 1)]
#[case(CpuTopology{smt: true, cores: 2, sockets: 1, thread_contiguous: false}, 3, 3)]
#[case(CpuTopology{smt: true, cores: 2, sockets: 1, thread_contiguous: true}, 0, 0)]
#[case(CpuTopology{smt: true, cores: 2, sockets: 1, thread_contiguous: true}, 1, 1)]
#[case(CpuTopology{smt: true, cores: 2, sockets: 1, thread_contiguous: true}, 2, 2)]
#[case(CpuTopology{smt: true, cores: 2, sockets: 1, thread_contiguous: true}, 3, 3)]
#[case(CpuTopology{smt: true, cores: 6, sockets: 2, thread_contiguous: false}, 4, 8)]
#[case(CpuTopology{smt: true, cores: 6, sockets: 2, thread_contiguous: false}, 11, 26)]
#[case(CpuTopology{smt: true, cores: 6, sockets: 2, thread_contiguous: false}, 14, 5)]
#[case(CpuTopology{smt: true, cores: 6, sockets: 2, thread_contiguous: false}, 23, 27)]
fn test_encode_x2apic(#[case] topology: CpuTopology, #[case] index: u16, #[case] x2apic: u32) {
    assert_eq!(encode_x2apic_id(&topology, index), x2apic)
}

#[cfg(test)]
mod expand_tests {
    use super::super::*; // Import from board_amd64.rs
    use std::collections::HashMap;
    use crate::arch::cpuid::CpuidIn;
    use std::arch::x86_64::CpuidResult;
    use crate::board::CpuSpec;
    use crate::arch::x86_64::features::{CpuidReg, CpuFeature};

    // Helper to create a base host_supported map with common features enabled
    fn mock_host_supported() -> HashMap<CpuidIn, CpuidResult> {
        let mut map = HashMap::new();
        let leaves = &[
            CpuidIn { func: 1, index: None },
            CpuidIn { func: 7, index: Some(0) },
            CpuidIn { func: 7, index: Some(1) },
            CpuidIn { func: 0x8000_0001, index: None },
            CpuidIn { func: 0x8000_0008, index: None },
            CpuidIn { func: 0x8000_0021, index: None },
            CpuidIn { func: 0xd, index: Some(1) },
            CpuidIn { func: 6, index: None },
            CpuidIn { func: 0x8000_000a, index: None },
            CpuidIn { func: 0x4000_0000, index: None },
        ];
        for leaf in leaves {
            map.insert(
                leaf.clone(),
                CpuidResult {
                    eax: 0xffffffff,
                    ebx: 0xffffffff,
                    ecx: 0xffffffff,
                    edx: 0xffffffff,
                },
            );
        }
        map
    }

    #[test]
    fn test_expand_host_model() {
        let host = mock_host_supported();
        let spec = CpuSpec {
            model: "host".to_owned(),
            features: vec!["-sse2".to_owned()], // disable sse2
            ..Default::default()
        };
        let res = expand_cpu_model(&spec, &host).unwrap();

        // Verify sse2 is disabled
        let leaf1 = CpuidIn {
            func: 1,
            index: None,
        };
        let out = res.get(&leaf1).unwrap();
        assert_eq!(out.edx & (1 << 26), 0);
        // hypervisor should still be enabled
        assert_ne!(out.ecx & (1 << 31), 0);
    }

    fn create_test_model_file() -> tempfile::NamedTempFile {
        use std::io::Write;
        let mut temp_file = tempfile::NamedTempFile::new().unwrap();
        let yaml_content = r#"
name: "custom-test-model"
level: 13
xlevel: 2147483681
vendor: [1753289025, 1769888869, 1145987171] # AMD
family: 25
model: 17
stepping: 0
model_id: "Custom Test Model"
features:
  - "sse2"
  - "avx2"
versions:
  - version: 1
    props: []
  - version: 2
    props:
      - ["avx512f", true]
    model_id: "Custom Test Model v2"
"#;
        temp_file.write_all(yaml_content.as_bytes()).unwrap();
        temp_file
    }

    #[test]
    fn test_expand_named_model_genuina_features() {
        let file = create_test_model_file();
        let path_str = file.path().to_str().unwrap();
        let host = mock_host_supported();
        let spec = CpuSpec {
            model: path_str.to_owned(),
            ..Default::default()
        };
        let res = expand_cpu_model(&spec, &host).unwrap();

        // Model has sse2. Host has sse2. Should be enabled.
        let leaf1 = CpuidIn {
            func: 1,
            index: None,
        };
        let out = res.get(&leaf1).unwrap();
        assert_ne!(out.edx & (1 << 26), 0);

        // Model has avx2. Host has avx2. Should be enabled.
        let leaf7 = CpuidIn {
            func: 7,
            index: Some(0),
        };
        let out = res.get(&leaf7).unwrap();
        assert_ne!(out.ebx & (1 << 5), 0);

        // Model-v1 does NOT have avx512f. Should be disabled.
        let leaf7_0 = CpuidIn {
            func: 7,
            index: Some(0),
        };
        if let Some(out) = res.get(&leaf7_0) {
            assert_eq!(out.ebx & (1 << 16), 0); // avx512f is bit 16 of EBX leaf 7 subleaf 0
        }

        // Verify model_id is encoded
        let brand1 = res
            .get(&CpuidIn {
                func: 0x8000_0002,
                index: None,
            })
            .unwrap();
        assert_ne!(brand1.eax, 0); // Should contain part of "Custom Test Model"

        // Verify hypervisor leaf is copied
        let hv_leaf = res
            .get(&CpuidIn {
                func: 0x4000_0000,
                index: None,
            })
            .unwrap();
        assert_eq!(hv_leaf.eax, 0xffffffff);
    }

    #[test]
    fn test_expand_named_model_versioning() {
        let file = create_test_model_file();
        let path_str = file.path().to_str().unwrap();
        let host = mock_host_supported();
        // v2 adds avx512f
        let spec = CpuSpec {
            model: format!("{}-v2", path_str),
            ..Default::default()
        };
        let res = expand_cpu_model(&spec, &host).unwrap();

        // Model-v2 has avx512f. Host has it. Should be enabled.
        let leaf7 = CpuidIn {
            func: 7,
            index: Some(0),
        };
        let out = res.get(&leaf7).unwrap();
        assert_ne!(out.ebx & (1 << 16), 0); // avx512f
    }

    #[test]
    fn test_expand_named_model_unsupported() {
        let file = create_test_model_file();
        let path_str = file.path().to_str().unwrap();
        let mut host = mock_host_supported();
        // Disable avx2 on host
        let leaf7 = CpuidIn {
            func: 7,
            index: Some(0),
        };
        let entry = host.get_mut(&leaf7).unwrap();
        entry.ebx &= !(1 << 5);

        let spec = CpuSpec {
            model: path_str.to_owned(),
            ..Default::default()
        };
        let res = expand_cpu_model(&spec, &host);

        // Model has avx2, but host doesn't. Should fail.
        assert!(matches!(
            res,
            Err(crate::board::Error::UnsupportedCpuFeature { feature, .. }) if feature == "avx2"
        ));
    }

    #[test]
    fn test_expand_named_model_unsupported_but_disabled() {
        let file = create_test_model_file();
        let path_str = file.path().to_str().unwrap();
        let mut host = mock_host_supported();
        // Disable avx2 on host
        let leaf7 = CpuidIn {
            func: 7,
            index: Some(0),
        };
        let entry = host.get_mut(&leaf7).unwrap();
        entry.ebx &= !(1 << 5); // Clear avx2 bit

        // Model has avx2, which is unsupported, but we explicitly disable it
        let spec = CpuSpec {
            model: path_str.to_owned(),
            features: vec!["-avx2".to_owned()],
            ..Default::default()
        };
        let res = expand_cpu_model(&spec, &host);

        // Should succeed because the unsupported feature was disabled
        assert!(res.is_ok());
        let cpuids = res.unwrap();
        let out = cpuids.get(&leaf7).unwrap();
        assert_eq!(out.ebx & (1 << 5), 0); // avx2 should be disabled
    }

    #[test]
    fn test_expand_customization() {
        let file = create_test_model_file();
        let path_str = file.path().to_str().unwrap();
        let host = mock_host_supported();
        let spec = CpuSpec {
            model: path_str.to_owned(),
            features: vec!["-avx2".to_owned(), "+avx512f".to_owned()],
            ..Default::default()
        };
        let res = expand_cpu_model(&spec, &host).unwrap();

        // avx2 explicitly disabled
        let leaf7 = CpuidIn {
            func: 7,
            index: Some(0),
        };
        let out = res.get(&leaf7).unwrap();
        assert_eq!(out.ebx & (1 << 5), 0);

        // avx512f explicitly enabled (host supports it)
        assert_ne!(out.ebx & (1 << 16), 0);
    }

    #[test]
    fn test_expand_invalid_customization() {
        let mut host = mock_host_supported();
        let leaf7 = CpuidIn { func: 7, index: Some(0) };
        let entry = host.get_mut(&leaf7).unwrap();
        entry.ebx &= !(1 << 16);
        // Enable feature not supported by host
        let spec = CpuSpec {
            model: "host".to_owned(),
            features: vec!["+avx512f".to_owned()], // host doesn't have it in mock_host_supported
            ..Default::default()
        };
        let res = expand_cpu_model(&spec, &host);
        assert!(matches!(
            res,
            Err(crate::board::Error::UnsupportedCpuFeature { feature, .. }) if feature == "avx512f"
        ));
    }

    #[test]
    fn test_lookup_feature() {
        assert_eq!(
            lookup_feature("sse2"),
            Some(CpuFeature {
                name: "sse2",
                func: 1,
                index: None,
                reg: CpuidReg::Edx,
                bit: 26
            })
        );
        assert_eq!(
            lookup_feature("avx2"),
            Some(CpuFeature {
                name: "avx2",
                func: 7,
                index: Some(0),
                reg: CpuidReg::Ebx,
                bit: 5
            })
        );
        assert_eq!(lookup_feature("invalid_feat"), None);
    }

    #[test]
    fn test_expand_custom_yaml_model() {
        use std::io::Write;
        let mut temp_file = tempfile::NamedTempFile::new().unwrap();
        let yaml_content = r#"
name: "custom-test-model"
level: 13
xlevel: 2147483681
vendor: [1753289025, 1769888869, 1145987171] # AMD
family: 25
model: 17
stepping: 0
model_id: "Custom Test Model"
features:
  - "sse2"
  - "avx2"
versions:
  - version: 1
    props: []
  - version: 2
    props:
      - ["avx512f", true]
    model_id: "Custom Test Model v2"
"#;
        temp_file.write_all(yaml_content.as_bytes()).unwrap();
        let path_str = temp_file.path().to_str().unwrap();

        let host = mock_host_supported(); // has sse2, avx2, avx512f

        // Test loading v1
        let spec = CpuSpec {
            count: 1,
            model: format!("{}", path_str),
            topology: Default::default(),
            features: vec![],
        };
        let res = expand_cpu_model(&spec, &host).unwrap();
        assert!(get_cpuid_bit(&res, &lookup_feature("sse2").unwrap()));
        assert!(get_cpuid_bit(&res, &lookup_feature("avx2").unwrap()));
        assert!(!get_cpuid_bit(&res, &lookup_feature("avx512f").unwrap())); // avx512f is v2 only

        // Test loading v2
        let spec_v2 = CpuSpec {
            count: 1,
            model: format!("{}-v2", path_str),
            topology: Default::default(),
            features: vec![],
        };
        let res_v2 = expand_cpu_model(&spec_v2, &host).unwrap();
        assert!(get_cpuid_bit(&res_v2, &lookup_feature("sse2").unwrap()));
        assert!(get_cpuid_bit(&res_v2, &lookup_feature("avx2").unwrap()));
        assert!(get_cpuid_bit(&res_v2, &lookup_feature("avx512f").unwrap())); // avx512f is enabled in v2
    }
}
