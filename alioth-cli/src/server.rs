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
use std::fmt::Debug;
use std::path;
use std::sync::Arc;

use alioth::hv::Hypervisor;
use alioth::vm::{self, Machine};
use axum::body::Body;
use axum::extract::{Path, State};
use axum::response::Response;
use axum::routing::post;
use axum::{Json, Router};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tokio::fs::{self};

use crate::boot::Result;

// trait Serve: Send + Sync + 'static {
//     fn pause(&self, name: &str) -> Response;
//     fn resume(&self, name: &str) -> Response;
//     fn snapshot(&self, name: &str, param: &SnapshotParam) -> Response;
// }

trait Instance: Send + Sync + 'static {
    fn pause(&self) -> vm::Result<()>;
    fn resume(&self) -> vm::Result<()>;
    // fn snapshot(&self) -> vm::Result<vm::Snapshot>;
    fn wait(&self) -> vm::Result<()>;
}

impl<H> Instance for Machine<H>
where
    H: Hypervisor,
{
    fn pause(&self) -> vm::Result<()> {
        Machine::pause(self)
    }

    fn resume(&self) -> vm::Result<()> {
        Machine::resume(self)
    }

    // fn snapshot(&self) -> vm::Result<vm::Snapshot> {
    //     Machine::snapshot(self)
    // }

    fn wait(&self) -> vm::Result<()> {
        Machine::wait(self)
    }
}

#[axum::debug_handler]
async fn vm_pause(state: State<Arc<Vmm>>, path: Path<String>) -> Response {
    let Path(name) = path;
    let State(server) = state;
    log::info!("Pausing VM: {name}");
    server.pause(&name)
}

#[axum::debug_handler]
async fn vm_resume(state: State<Arc<Vmm>>, path: Path<String>) -> Response {
    let Path(name) = path;
    let State(server) = state;
    log::info!("Resuming VM: {name}");
    server.resume(&name)
}

// #[axum::debug_handler]
// async fn vm_snapshot(
//     state: State<Arc<Vmm>>,
//     path: Path<String>,
//     param: Json<SnapshotParam>,
// ) -> Response {
//     let Path(name) = path;
//     let State(server) = state;
//     log::info!("Snapshotting VM: {name}");
//     log::info!("received data: {param:?}");
//     let param = param.0;
//     server.snapshot(&name, &param).await
// }

struct Vmm {
    pub vms: RwLock<HashMap<Arc<str>, Arc<dyn Instance>>>,
}

impl Vmm {
    pub fn with<H>(vm: Machine<H>) -> Self
    where
        H: Hypervisor,
    {
        Self {
            vms: RwLock::new(HashMap::from([(
                "vm0".into(),
                Arc::new(vm) as Arc<dyn Instance>,
            )])),
        }
    }
}

pub struct Server {
    vmm: Arc<Vmm>,
}

fn not_found(name: &str) -> Response {
    Response::builder()
        .status(404)
        .body(Body::from(format!("Not found: {name}")))
        .unwrap()
}

fn internal_server_error<E>(e: E) -> Response
where
    E: Debug,
{
    Response::builder()
        .status(500)
        .body(Body::from(format!("Internal server error:\n{e:?}")))
        .unwrap()
}

fn into_response<T, E>(result: Result<T, E>) -> Response
where
    T: Serialize,
    E: Debug,
{
    match result {
        Ok(t) => Response::new(Body::from(serde_yaml::to_string(&t).unwrap())),
        Err(e) => internal_server_error(e),
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SnapshotParam {
    pub dest: Box<path::Path>,
}

impl Vmm {
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

    // async fn snapshot(&self, name: &str, param: &SnapshotParam) -> Response {
    //     let vm = {
    //         let vms = self.vms.read();
    //         let Some(vm) = vms.get(name) else {
    //             return not_found(name);
    //         };
    //         vm.clone()
    //     };
    //     let snapthot = match tokio::task::spawn_blocking(move || vm.snapshot()).await {
    //         Ok(Ok(snapshot)) => snapshot,
    //         Ok(Err(e)) => return internal_server_error(e),
    //         Err(e) => return internal_server_error(e),
    //     };
    //     let data = match serde_yaml::to_string(&snapthot) {
    //         Ok(data) => data,
    //         Err(e) => return internal_server_error(e),
    //     };
    //     into_response(fs::write(&param.dest, data).await)
    // }
}

impl Server {
    pub fn with<H>(vm: Machine<H>) -> Self
    where
        H: Hypervisor,
    {
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
                // .route("/vms/{name}:snapshot", post(vm_snapshot))
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
