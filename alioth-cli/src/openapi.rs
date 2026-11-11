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

//! Describes the VM API in OpenAPI 3.1, using the [`Help`] of its types.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::iter;

use alioth::api::{CreateVmRequest, ListVmsRequest, ListVmsResponse, Vm};
use alioth::vm::config::VmSpec;
use serde_aco::{FieldHelp, Help, TypedHelp};
use serde_json::{Map, Value, json};

use crate::api::{Empty, ErrorBody};

const DESCRIPTION: &str = "\
alioth serves the API at a Unix domain socket, with `--api unix:<path>`, or at a
loopback TCP port, with `--api http://localhost:<port>`. The API has no
authentication: only the user of alioth can use the socket, but any local
process can connect to the port.

`alioth server` manages several VMs, or one on macOS. `alioth boot --api`
manages only `vms/vm0`, which it creates from its command-line options.
`/v1/vm` is an alias of `/v1/vms/vm0`, e.g. `POST /v1/vm:shutdown`.

Errors follow AIP-193.

This description is generated on Linux x86_64. Some fields of the VM spec vary
by platform.";

/// The schemas of named types, which other schemas refer to.
#[derive(Debug, Default)]
struct Schemas(BTreeMap<&'static str, Value>);

impl Schemas {
    /// Returns the schema of the values of `help`.
    fn schema(&mut self, help: &'static TypedHelp) -> Value {
        match help {
            TypedHelp::Struct { name: "", fields } => self.object(fields),
            TypedHelp::Struct { name, fields } => {
                let schema = self.object(fields);
                self.reference(name, schema)
            }
            TypedHelp::Enum { name, variants } => {
                let schema = self.one_of(variants);
                self.reference(name, schema)
            }
            TypedHelp::String => json!({"type": "string"}),
            TypedHelp::Int => json!({"type": "integer"}),
            TypedHelp::Float => json!({"type": "number"}),
            TypedHelp::Bool => json!({"type": "boolean"}),
            TypedHelp::Unit => json!({"type": "null"}),
            TypedHelp::Custom { desc } => json!({"type": "string", "format": desc}),
            TypedHelp::Array(item) => json!({"type": "array", "items": self.schema(item)}),
            TypedHelp::Option(value) => json!({"anyOf": [self.schema(value), {"type": "null"}]}),
        }
    }

    /// Adds the schema of type `name`, and returns a reference to it.
    fn reference(&mut self, name: &'static str, schema: Value) -> Value {
        match self.0.entry(name) {
            Entry::Vacant(entry) => {
                entry.insert(schema);
            }
            Entry::Occupied(entry) => assert_eq!(*entry.get(), schema, "two types named {name}"),
        }
        json!({"$ref": format!("#/components/schemas/{name}")})
    }

    /// Returns the schema of a struct with `fields`.
    fn object(&mut self, fields: &'static [FieldHelp]) -> Value {
        let mut properties = Map::new();
        let mut required = vec![];
        let mut flattened = vec![];
        for field in fields {
            let mut schema = self.schema(&field.ty);
            if field.name.is_empty() {
                flattened.push(schema);
                continue;
            }
            if !field.doc.is_empty() {
                schema["description"] = json!(field.doc);
            }
            if !field.optional && !matches!(field.ty, TypedHelp::Option(_)) {
                required.push(field.name);
            }
            properties.insert(field.name.to_owned(), schema);
        }
        let mut object = json!({"type": "object"});
        if !properties.is_empty() {
            object["properties"] = Value::Object(properties);
        }
        if !required.is_empty() {
            object["required"] = json!(required);
        }
        if flattened.is_empty() {
            return object;
        }
        flattened.insert(0, object);
        json!({ "allOf": flattened })
    }

    /// Returns the schema of an externally tagged enum with `variants`.
    fn one_of(&mut self, variants: &'static [FieldHelp]) -> Value {
        let mut schemas = vec![];
        for variant in variants {
            let mut schema = match &variant.ty {
                TypedHelp::Unit => json!({"type": "string", "const": variant.name}),
                ty => json!({
                    "type": "object",
                    "properties": {variant.name: self.schema(ty)},
                    "required": [variant.name],
                }),
            };
            if !variant.doc.is_empty() {
                schema["description"] = json!(variant.doc);
            }
            schemas.push(schema);
        }
        json!({ "oneOf": schemas })
    }

    /// Returns the parameters in the query of a request of type `help`.
    fn query_params(&mut self, help: &'static TypedHelp) -> Value {
        let TypedHelp::Struct { fields, .. } = help else {
            unreachable!("a query is a struct")
        };
        let mut params = vec![];
        for field in fields.iter() {
            let (required, ty) = match &field.ty {
                TypedHelp::Option(ty) => (false, *ty),
                ty => (!field.optional, ty),
            };
            params.push(json!({
                "name": field.name,
                "in": "query",
                "description": field.doc,
                "required": required,
                "schema": self.schema(ty),
            }));
        }
        json!(params)
    }

    /// Returns the responses of a method whose result is of type `help`.
    fn responses(&mut self, help: &'static TypedHelp) -> Value {
        json!({
            "200": {
                "description": "OK",
                "content": {"application/json": {"schema": self.schema(help)}},
            },
            "4XX": {"$ref": "#/components/responses/Error"},
            "5XX": {"$ref": "#/components/responses/Error"},
        })
    }

    /// Returns a custom method on a VM, see AIP-136.
    fn vm_method(&mut self, id: &str, summary: &str) -> Value {
        json!({
            "parameters": [{"$ref": "#/components/parameters/vm"}],
            "post": {
                "operationId": id,
                "summary": summary,
                "responses": self.responses(&Vm::HELP),
            },
        })
    }
}

/// Returns the OpenAPI description of the VM API.
pub fn openapi() -> Value {
    let mut schemas = Schemas::default();
    let paths = json!({
        "/v1/vms": {
            "get": {
                "operationId": "ListVms",
                "summary": "Lists VMs in the order of their IDs.",
                "parameters": schemas.query_params(&ListVmsRequest::HELP),
                "responses": schemas.responses(&ListVmsResponse::HELP),
            },
            "post": {
                "operationId": "CreateVm",
                "summary": "Creates a stopped VM.",
                "description": "`alioth boot --api` does not support it, and returns 405.",
                "parameters": schemas.query_params(&CreateVmRequest::HELP),
                "requestBody": {
                    "required": true,
                    "content": {"application/json": {"schema": schemas.schema(&VmSpec::HELP)}},
                },
                "responses": schemas.responses(&Vm::HELP),
            },
        },
        "/v1/vms/{vm}": {
            "parameters": [{"$ref": "#/components/parameters/vm"}],
            "get": {
                "operationId": "GetVm",
                "summary": "Gets a VM.",
                "responses": schemas.responses(&Vm::HELP),
            },
            "delete": {
                "operationId": "DeleteVm",
                "summary": "Deletes a VM. A VM that is not stopped is powered off first.",
                "responses": schemas.responses(&Empty::HELP),
            },
        },
        "/v1/vms/{vm}:boot": schemas.vm_method("BootVm", "Boots a stopped VM."),
        "/v1/vms/{vm}:shutdown": schemas.vm_method(
            "ShutdownVm",
            "Powers off a VM, and returns after it stops.",
        ),
        "/v1/vms/{vm}:reboot": schemas.vm_method("RebootVm", "Reboots a VM."),
    });
    let error = json!({
        "description": "An error, as defined by AIP-193.",
        "content": {"application/json": {"schema": schemas.schema(&ErrorBody::HELP)}},
    });
    json!({
        "openapi": "3.1.0",
        "info": {
            "title": "Alioth VM API",
            "version": "v1",
            "description": DESCRIPTION,
            "license": {"name": "Apache-2.0", "identifier": "Apache-2.0"},
        },
        "security": [],
        "paths": paths,
        "components": {
            "parameters": {
                "vm": {
                    "name": "vm",
                    "in": "path",
                    "description": "The ID of the VM, e.g. `vm0`.",
                    "required": true,
                    "schema": {"type": "string"},
                },
            },
            "responses": {"Error": error},
            "schemas": schemas.0,
        },
    })
}

/// Returns `s` as a YAML scalar, which is plain if that leaves it a string,
/// or double-quoted otherwise.
fn yaml_string(s: &str) -> String {
    let plain = s.starts_with(|c: char| c.is_ascii_alphabetic())
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_./-".contains(c))
        && !matches!(
            s.to_ascii_lowercase().as_str(),
            "true" | "false" | "null" | "yes" | "no" | "on" | "off" | "y" | "n"
        );
    if plain {
        s.to_owned()
    } else {
        // A JSON string is a double-quoted YAML scalar.
        serde_json::to_string(s).unwrap()
    }
}

fn push_indent(yaml: &mut String, indent: usize) {
    yaml.extend(iter::repeat_n(' ', indent));
}

/// Writes the entries of a non-empty object or array, indented by `indent`.
fn write_block(yaml: &mut String, value: &Value, indent: usize) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                push_indent(yaml, indent);
                yaml.push_str(&yaml_string(key));
                yaml.push(':');
                write_node(yaml, value, indent + 2);
            }
        }
        Value::Array(items) => {
            for item in items {
                push_indent(yaml, indent);
                yaml.push('-');
                if matches!(item, Value::Object(map) if !map.is_empty()) {
                    // The first entry follows `- `, and the others align with it.
                    let mut block = String::new();
                    write_block(&mut block, item, indent + 2);
                    yaml.push(' ');
                    yaml.push_str(&block[indent + 2..]);
                } else {
                    write_node(yaml, item, indent + 2);
                }
            }
        }
        _ => unreachable!("{value} is not a block"),
    }
}

/// Writes `value` after a key or a `-`, with its block indented by `indent`.
fn write_node(yaml: &mut String, value: &Value, indent: usize) {
    let scalar = match value {
        Value::Object(map) if !map.is_empty() => None,
        Value::Array(items) if !items.is_empty() => None,
        Value::Object(_) => Some("{}".to_owned()),
        Value::Array(_) => Some("[]".to_owned()),
        // A literal block scalar keeps a text readable, but it cannot start
        // with spaces, and it drops the trailing line break.
        Value::String(s) if s.contains('\n') && !s.starts_with(' ') && !s.ends_with('\n') => {
            yaml.push_str(" |-\n");
            for line in s.lines() {
                if !line.is_empty() {
                    push_indent(yaml, indent);
                    yaml.push_str(line);
                }
                yaml.push('\n');
            }
            return;
        }
        Value::String(s) => Some(yaml_string(s)),
        Value::Null | Value::Bool(_) | Value::Number(_) => Some(value.to_string()),
    };
    match scalar {
        Some(scalar) => {
            yaml.push(' ');
            yaml.push_str(&scalar);
            yaml.push('\n');
        }
        None => {
            yaml.push('\n');
            write_block(yaml, value, indent);
        }
    }
}

/// Converts a JSON object to YAML in block style.
pub fn to_yaml(value: &Value) -> String {
    let mut yaml = String::new();
    write_block(&mut yaml, value, 0);
    yaml
}

#[cfg(test)]
#[path = "openapi_test.rs"]
mod tests;
