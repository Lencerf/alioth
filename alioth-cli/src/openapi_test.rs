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

use serde_json::{Value, json};

use crate::openapi::{openapi, to_yaml};

fn find_refs<'a>(value: &'a Value, refs: &mut Vec<&'a str>) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                match (key.as_str(), value) {
                    ("$ref", Value::String(r)) => refs.push(r),
                    _ => find_refs(value, refs),
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|item| find_refs(item, refs)),
        _ => {}
    }
}

#[test]
fn test_refs() {
    let doc = openapi();
    let mut refs = vec![];
    find_refs(&doc, &mut refs);
    for r in &refs {
        let pointer = r.strip_prefix('#').unwrap();
        assert!(doc.pointer(pointer).is_some(), "{r} is missing");
    }
    let components = doc["components"].as_object().unwrap();
    for (kind, items) in components {
        for name in items.as_object().unwrap().keys() {
            let r = format!("#/components/{kind}/{name}");
            assert!(refs.contains(&r.as_str()), "{r} is unused");
        }
    }
}

#[test]
fn test_schemas() {
    let doc = openapi();
    let schemas = &doc["components"]["schemas"];

    let vm = &schemas["Vm"]["allOf"];
    assert_eq!(vm[0]["required"], json!(["name", "state"]));
    assert_eq!(vm[1], json!({"$ref": "#/components/schemas/VmSpec"}));

    assert_eq!(schemas["VmSpec"]["required"], json!(["payload"]));
    assert_eq!(
        schemas["VmState"]["oneOf"][0],
        json!({
            "type": "string",
            "const": "STOPPED",
            "description": "The VM is not running. It can boot.",
        })
    );
    assert_eq!(
        schemas["Executable"]["oneOf"][0],
        json!({
            "type": "object",
            "properties": {"linux": {"type": "string"}},
            "required": ["linux"],
            "description": "Path to a Linux kernel image.",
        })
    );
    assert_eq!(
        schemas["PtyConsoleSpec"]["properties"]["link"]["anyOf"][1],
        json!({"type": "null"})
    );
}

#[test]
fn test_to_yaml() {
    let value = json!({
        "openapi": "3.1.0",
        "paths": {"/v1/vms/{vm}:boot": {"post": {}}},
        "required": ["name", "true"],
        "items": [{"a": 1, "b": [false, null]}, "x"],
        "empty": [],
        "description": "line 1\n\nline 2",
    });
    let yaml = "\
description: |-
  line 1

  line 2
empty: []
items:
  - a: 1
    b:
      - false
      - null
  - x
openapi: \"3.1.0\"
paths:
  \"/v1/vms/{vm}:boot\":
    post: {}
required:
  - name
  - \"true\"
";
    assert_eq!(to_yaml(&value), yaml);
}

/// `docs/openapi.yaml` is generated on Linux x86_64, since the VM spec varies
/// by platform. Run the test with `ALIOTH_UPDATE_OPENAPI=1` to update it.
#[test]
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn test_openapi_yaml() {
    use std::path::Path;
    use std::{env, fs};

    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../docs/openapi.yaml");
    let yaml = to_yaml(&openapi());
    if env::var_os("ALIOTH_UPDATE_OPENAPI").is_some() {
        fs::write(&path, &yaml).unwrap();
    }
    let expected = fs::read_to_string(&path).unwrap();
    assert!(
        yaml == expected,
        "docs/openapi.yaml is outdated, run the test with ALIOTH_UPDATE_OPENAPI=1"
    );
}
