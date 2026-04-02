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

use std::sync::Arc;

use alioth::hv::Hypervisor;
use alioth::vm::Machine;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::routing::post;
use snafu::ResultExt;

use crate::boot::{Result, error};

pub struct Server<H>
where
    H: Hypervisor,
{
    pub vm: Arc<Machine<H>>,
}

trait Serve: Send + Sync + 'static {
    fn vm_pause(&self);
    fn vm_resume(&self);
}

impl<H> Serve for Machine<H>
where
    H: Hypervisor,
{
    fn vm_pause(&self) {
        self.pause().unwrap();
    }

    fn vm_resume(&self) {
        self.resume().unwrap();
    }
}

#[axum::debug_handler]
async fn vm_pause(state: State<Arc<dyn Serve>>, path: Path<String>, bytes: Bytes) {
    let Path(name) = path;
    let State(vm) = state;
    log::info!("Pausing VM: {}", name);
    log::info!("received data: {:?}", bytes);
    vm.vm_pause();
    log::info!("VM paused successfully");
}
#[axum::debug_handler]
async fn vm_resume(state: State<Arc<dyn Serve>>, path: Path<String>, bytes: Bytes) {
    let Path(name) = path;
    let State(vm) = state;
    log::info!("Resuming VM: {}", name);
    log::info!("received data: {:?}", bytes);
    vm.vm_resume();
    log::info!("VM resumed successfully");
}

impl<H> Server<H>
where
    H: Hypervisor,
{
    pub fn new(vm: Machine<H>) -> Self {
        Self { vm: Arc::new(vm) }
    }

    pub fn run(self) -> Result<()> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let r = runtime.block_on(async {
            let vm = self.vm.clone(); // as Arc<dyn Serve>;

            let app = Router::new()
                .without_v07_checks()
                .route("/vms/{name}:pause", post(vm_pause))
                .route("/vms/{name}:resume", post(vm_resume))
                .with_state(vm);

            let listener = tokio::net::TcpListener::bind("[::]:3000").await.unwrap();
            let server = axum::serve(listener, app).into_future();
            let vm = self.vm;
            let wait = tokio::task::spawn_blocking(move || vm.wait());
            tokio::select! {
                _ = server => unreachable!(),
                r = wait => r,
            }
        });
        match r {
            Ok(ret) => ret.context(error::WaitVm),
            Err(e) => {
                log::error!("Failed to join thread: {e}");
                Ok(())
            }
        }
    }
}
