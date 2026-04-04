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
use std::sync::Arc;

use alioth::hv::Hypervisor;
use alioth::vm::{self, Machine};
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::response::Response;
use axum::routing::post;
use parking_lot::RwLock;
use snafu::ResultExt;

use crate::boot::{Result, error};

trait Serve: Send + Sync + 'static {
    fn pause(&self, name: &str) -> Response;
    fn resume(&self, name: &str) -> Response;
    fn snapshot(&self, name: &str) -> Response;
}

#[axum::debug_handler]
async fn vm_pause(state: State<Arc<dyn Serve>>, path: Path<String>, bytes: Bytes) -> Response {
    let Path(name) = path;
    let State(server) = state;
    log::info!("Pausing VM: {}", name);
    log::info!("received data: {:?}", bytes);
    server.pause(&name)
}

#[axum::debug_handler]
async fn vm_resume(state: State<Arc<dyn Serve>>, path: Path<String>, bytes: Bytes) -> Response {
    let Path(name) = path;
    let State(server) = state;
    log::info!("Resuming VM: {}", name);
    log::info!("received data: {:?}", bytes);
    server.resume(&name)
}

#[axum::debug_handler]
async fn vm_snapshot(state: State<Arc<dyn Serve>>, path: Path<String>, bytes: Bytes) -> Response {
    let Path(name) = path;
    let State(server) = state;
    log::info!("Snapshotting VM: {}", name);
    log::info!("received data: {:?}", bytes);
    server.snapshot(&name)
}

struct Vmm<H>
where
    H: Hypervisor,
{
    pub vms: RwLock<HashMap<Arc<str>, Arc<Machine<H>>>>,
}

impl<H> Vmm<H>
where
    H: Hypervisor,
{
    pub fn with(vm: Machine<H>) -> Self {
        Self {
            vms: RwLock::new(HashMap::from([("vm0".into(), Arc::new(vm))])),
        }
    }
}

pub struct Server<H>
where
    H: Hypervisor,
{
    vmm: Arc<Vmm<H>>,
}

fn not_found(name: &str) -> Response {
    Response::builder()
        .status(404)
        .body(Body::from(format!("Not found: {name}")))
        .unwrap()
}

fn into_response<T>(result: vm::Result<T>) -> Response {
    match result {
        Ok(_) => Response::new(Body::empty()),
        Err(e) => Response::builder()
            .status(500)
            .body(Body::from(format!("{e:?}")))
            .unwrap(),
    }
}

impl<H> Serve for Vmm<H>
where
    H: Hypervisor,
{
    fn pause(&self, name: &str) -> Response {
        let vms = self.vms.read();
        let Some(vm) = vms.get(name) else {
            return not_found(name);
        };
        into_response(vm.pause())
    }

    fn resume(&self, name: &str) -> Response {
        let vms = self.vms.read();
        let Some(vm) = vms.get(name) else {
            return not_found(name);
        };
        into_response(vm.resume())
    }

    fn snapshot(&self, name: &str) -> Response {
        todo!()
    }
}

impl<H> Server<H>
where
    H: Hypervisor,
{
    pub fn with(vm: Machine<H>) -> Self {
        Self {
            vmm: Arc::new(Vmm::with(vm)),
        }
    }

    pub fn run(self) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let r = runtime.block_on(async {
            let vmm = self.vmm.clone(); // as Arc<dyn Serve>;

            let app = Router::new()
                .without_v07_checks()
                .route("/vms/{name}:pause", post(vm_pause))
                .route("/vms/{name}:resume", post(vm_resume))
                .route("/vms/{name}:snapshot", post(vm_snapshot))
                .with_state(vmm);

            let listener = tokio::net::TcpListener::bind("[::]:3000").await.unwrap();
            let server = axum::serve(listener, app).into_future();
            let vmm = self.vmm;
            let wait = tokio::task::spawn_blocking(move || {
                loop {
                    let vms = vmm.vms.read();
                    let Some((name, vm)) = vms.iter().next() else {
                        log::info!("No VMs running");
                        break;
                    };
                    let (name, vm) = (name.clone(), vm.clone());
                    drop(vms);

                    if let Err(e) = vm.wait() {
                        log::error!("Failed to wait for VM {name}: {e}");
                    }
                    log::info!("VM {name} exited");
                    vmm.vms.write().remove(&name);
                }
            });
            tokio::select! {
                _ = server => unreachable!(),
                r = wait => r,
            }
        });
        if let Err(e) = r {
            log::error!("Failed to join thread: {e}");
        }
    }
}
