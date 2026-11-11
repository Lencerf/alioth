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
use std::future;
use std::sync::Arc;

use alioth::api::VmManager;
use alioth::errors::{DebugTrace, trace_error};
use alioth::hv::HvSpec;
use alioth::vm::Machine;
use clap::Args;
use serde_aco::help_text;
use snafu::{ResultExt, Snafu};

use crate::api::{self, ApiAddr};
use crate::boot;

#[trace_error]
#[derive(Snafu, DebugTrace)]
#[snafu(module, context(suffix(false)))]
pub enum Error {
    #[snafu(display("Failed to create the hypervisor"))]
    Hypervisor { source: boot::Error },
    #[snafu(display("Failed to serve the API"))]
    Serve { error: std::io::Error },
}

#[derive(Args, Debug, Clone)]
pub struct ServerArgs {
    #[arg(long, help(
        help_text::<HvSpec>("Specify the Hypervisor to run on.")
    ), value_name = "HV")]
    hypervisor: Option<String>,

    /// Address to serve the VM API at: unix:<path>, or http://localhost:<port>.
    #[arg(long, value_name = "ADDR")]
    api: ApiAddr,
}

pub fn server(args: ServerArgs) -> Result<(), Error> {
    let hypervisor =
        boot::hypervisor(args.hypervisor, &HashMap::new()).context(error::Hypervisor)?;
    let manager = Arc::new(VmManager::<Machine<_>>::new(hypervisor));
    let ret = api::serve(&args.api, manager.clone(), true, future::pending());
    manager.delete_all();
    ret.context(error::Serve)
}
