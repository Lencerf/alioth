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

use std::collections::HashMap;

#[cfg(target_arch = "x86_64")]
use alioth::{arch::sev::SevPolicy, hv::Coco};
use rstest::rstest;

use crate::boot::{BootArgs, parse_config};

#[cfg(target_arch = "x86_64")]
#[rstest]
#[case("sev,policy=0x1", Some(Coco::AmdSev{policy: SevPolicy(0x1)}))]
fn test_parse_coco(#[case] arg: &str, #[case] want: Option<Coco>) {
    let boot_arg = BootArgs {
        coco: Some(arg.to_owned()),
        ..Default::default()
    };
    let config = parse_config(boot_arg, HashMap::new()).unwrap();
    assert_eq!(config.board.coco, want)
}
