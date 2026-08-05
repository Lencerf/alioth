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
