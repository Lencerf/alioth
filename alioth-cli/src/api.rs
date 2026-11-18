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

//! Serves the VM API of `alioth::api` over HTTP.

use std::fs::{self, Permissions};
use std::future::Future;
use std::io::{self, ErrorKind};
use std::net::{Ipv4Addr, SocketAddr};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener as StdUnixListener, UnixStream};
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;

use alioth::api::{
    Code, CreateVmRequest, Error, Instance, ListVmsRequest, ListVmsResponse, Vm, VmManager,
};
use alioth::vm::config::VmSpec;
use axum::Router;
use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{Json, Path as UrlPath, Query, State};
use axum::http::header::ALLOW;
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Serialize;
use serde_aco::Help;
use tokio::net::{TcpListener, UnixListener};
use tokio::signal::unix::{self, SignalKind};

/// The address to serve the API at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiAddr {
    /// `unix:<path>`, a Unix domain socket, readable and writable only by
    /// the user of alioth.
    Unix(Box<Path>),
    /// `http://localhost:<port>`, a loopback TCP port.
    Tcp(SocketAddr),
}

impl FromStr for ApiAddr {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Some(path) = s.strip_prefix("unix:") {
            return Ok(ApiAddr::Unix(Path::new(path).into()));
        }
        let Some(host_port) = s.strip_prefix("http://") else {
            return Err(format!(
                "{s:?} is neither unix:<path> nor http://<host>:<port>"
            ));
        };
        let host_port = host_port.trim_end_matches('/');
        let addr = if let Some(port) = host_port.strip_prefix("localhost:") {
            let port = port.parse().map_err(|_| format!("invalid port {port:?}"))?;
            SocketAddr::from((Ipv4Addr::LOCALHOST, port))
        } else {
            let addr = host_port.parse::<SocketAddr>();
            addr.map_err(|_| format!("invalid address {host_port:?}"))?
        };
        // The API accepts host paths, so it must not be reachable remotely.
        if !addr.ip().is_loopback() {
            return Err(format!("{addr} is not a loopback address"));
        }
        Ok(ApiAddr::Tcp(addr))
    }
}

/// An API error as an HTTP response, with the body defined by AIP-193.
#[derive(Debug)]
struct ApiError(Error);

#[derive(Serialize, Help)]
pub(crate) struct ErrorBody {
    /// The error.
    error: Error,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.0.code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        (status, Json(ErrorBody { error: self.0 })).into_response()
    }
}

impl From<Error> for ApiError {
    fn from(error: Error) -> Self {
        ApiError(error)
    }
}

macro_rules! invalid_argument_from {
    ($($rejection:ty),*) => {
        $(impl From<$rejection> for ApiError {
            fn from(rejection: $rejection) -> Self {
                ApiError(Error::new(Code::InvalidArgument, rejection.body_text()))
            }
        })*
    };
}

invalid_argument_from!(JsonRejection, PathRejection, QueryRejection);

/// The response of Delete, an empty message.
#[derive(Serialize, Help)]
pub(crate) struct Empty {}

type Manager<V> = Arc<VmManager<V>>;

/// The ID of the VM in the URL. Routes without one are aliases of `vms/vm0`.
type VmId = Result<Option<UrlPath<String>>, PathRejection>;

fn vm_id(id: VmId) -> Result<String, ApiError> {
    Ok(id?.map_or_else(|| "vm0".to_owned(), |UrlPath(id)| id))
}

/// Runs `op` on a blocking thread, since VM operations may block.
async fn call<V, T, O>(manager: Manager<V>, op: O) -> Result<Json<T>, ApiError>
where
    V: Instance,
    T: Send + 'static,
    O: FnOnce(&VmManager<V>) -> Result<T, Error> + Send + 'static,
{
    match tokio::task::spawn_blocking(move || op(&manager)).await {
        Ok(ret) => Ok(Json(ret?)),
        Err(e) => Err(ApiError(Error::new(Code::Internal, e.to_string()))),
    }
}

async fn create_vm<V: Instance>(
    State(manager): State<Manager<V>>,
    query: Result<Query<CreateVmRequest>, QueryRejection>,
    spec: Result<Json<VmSpec>, JsonRejection>,
) -> Result<Json<Vm>, ApiError> {
    let (Query(request), Json(spec)) = (query?, spec?);
    call(manager, move |m| m.create(request.vm_id.as_deref(), spec)).await
}

async fn list_vms<V: Instance>(
    State(manager): State<Manager<V>>,
    query: Result<Query<ListVmsRequest>, QueryRejection>,
) -> Result<Json<ListVmsResponse>, ApiError> {
    let Query(request) = query?;
    call(manager, move |m| m.list(&request)).await
}

async fn get_vm<V: Instance>(
    State(manager): State<Manager<V>>,
    id: VmId,
) -> Result<Json<Vm>, ApiError> {
    let id = vm_id(id)?;
    call(manager, move |m| m.get(&id)).await
}

async fn delete_vm<V: Instance>(
    State(manager): State<Manager<V>>,
    id: VmId,
) -> Result<Json<Empty>, ApiError> {
    let id = vm_id(id)?;
    call(manager, move |m| m.delete(&id).map(|()| Empty {})).await
}

async fn boot_vm<V: Instance>(
    State(manager): State<Manager<V>>,
    id: VmId,
) -> Result<Json<Vm>, ApiError> {
    let id = vm_id(id)?;
    call(manager, move |m| m.boot(&id)).await
}

async fn shutdown_vm<V: Instance>(
    State(manager): State<Manager<V>>,
    id: VmId,
) -> Result<Json<Vm>, ApiError> {
    let id = vm_id(id)?;
    call(manager, move |m| m.shutdown(&id)).await
}

async fn reboot_vm<V: Instance>(
    State(manager): State<Manager<V>>,
    id: VmId,
) -> Result<Json<Vm>, ApiError> {
    let id = vm_id(id)?;
    call(manager, move |m| m.reboot(&id)).await
}

async fn pause_vm<V: Instance>(
    State(manager): State<Manager<V>>,
    id: VmId,
) -> Result<Json<Vm>, ApiError> {
    let id = vm_id(id)?;
    call(manager, move |m| m.pause(&id)).await
}

async fn resume_vm<V: Instance>(
    State(manager): State<Manager<V>>,
    id: VmId,
) -> Result<Json<Vm>, ApiError> {
    let id = vm_id(id)?;
    call(manager, move |m| m.resume(&id)).await
}

/// Runs a custom method, `POST /v1/vms/{vm}:{verb}`, see AIP-136.
///
/// TODO: route `/v1/vms/{vm}:boot` etc. once axum 0.9 is released. The router
/// of axum 0.8 cannot match text after a parameter in the same path segment,
/// so this handler takes the whole segment, e.g. `vm0:boot`.
async fn custom_method<V: Instance>(
    manager: State<Manager<V>>,
    segment: Result<UrlPath<String>, PathRejection>,
) -> Result<Response, ApiError> {
    let UrlPath(segment) = segment?;
    let vm = |id: &str| -> VmId { Ok(Some(UrlPath(id.to_owned()))) };
    let response = match segment.rsplit_once(':') {
        Some((id, "boot")) => boot_vm(manager, vm(id)).await.into_response(),
        Some((id, "shutdown")) => shutdown_vm(manager, vm(id)).await.into_response(),
        Some((id, "reboot")) => reboot_vm(manager, vm(id)).await.into_response(),
        Some((id, "pause")) => pause_vm(manager, vm(id)).await.into_response(),
        Some((id, "resume")) => resume_vm(manager, vm(id)).await.into_response(),
        // POST is not a method of the VM itself.
        _ => ([(ALLOW, "GET,HEAD,DELETE")], method_not_allowed().await).into_response(),
    };
    Ok(response)
}

async fn method_not_allowed() -> ApiError {
    // AIP-193 has no canonical code for HTTP 405. UNIMPLEMENTED is the
    // closest one.
    ApiError(Error {
        code: StatusCode::METHOD_NOT_ALLOWED.as_u16(),
        message: "method not allowed".to_owned(),
        status: Code::Unimplemented,
    })
}

async fn not_found(uri: Uri) -> ApiError {
    let msg = format!("{} not found", uri.path());
    ApiError(Error::new(Code::NotFound, msg))
}

/// Routes API v1 requests. Without `create`, the VMs are fixed, and Create
/// returns 405 Method Not Allowed.
pub fn router<V: Instance>(manager: Manager<V>, create: bool) -> Router {
    let vms = if create {
        get(list_vms::<V>).post(create_vm::<V>)
    } else {
        get(list_vms::<V>)
    };
    let vm = || get(get_vm::<V>).delete(delete_vm::<V>);
    Router::new()
        .route("/v1/vms", vms)
        .route("/v1/vms/{vm}", vm().post(custom_method::<V>))
        // `vm` is an alias of `vms/vm0`.
        .route("/v1/vm", vm())
        .route("/v1/vm:boot", post(boot_vm::<V>))
        .route("/v1/vm:shutdown", post(shutdown_vm::<V>))
        .route("/v1/vm:reboot", post(reboot_vm::<V>))
        .route("/v1/vm:pause", post(pause_vm::<V>))
        .route("/v1/vm:resume", post(resume_vm::<V>))
        .method_not_allowed_fallback(method_not_allowed)
        .fallback(not_found)
        .with_state(manager)
}

/// Binds a Unix domain socket that only the user of alioth can use.
///
/// A socket left behind by a previous alioth is replaced.
fn bind_unix(path: &Path) -> io::Result<StdUnixListener> {
    match UnixStream::connect(path) {
        Ok(_) => return Err(io::Error::from(ErrorKind::AddrInUse)),
        Err(e) if e.kind() == ErrorKind::ConnectionRefused => {
            if fs::symlink_metadata(path)?.file_type().is_socket() {
                fs::remove_file(path)?;
            }
        }
        Err(_) => {}
    }
    let listener = StdUnixListener::bind(path)?;
    fs::set_permissions(path, Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

async fn signaled() -> io::Result<()> {
    let mut interrupt = unix::signal(SignalKind::interrupt())?;
    let mut terminate = unix::signal(SignalKind::terminate())?;
    tokio::select! {
        _ = interrupt.recv() => log::info!("Received SIGINT"),
        _ = terminate.recv() => log::info!("Received SIGTERM"),
    }
    Ok(())
}

/// Serves the API at `addr` until `stop` completes, or alioth receives
/// SIGINT or SIGTERM.
pub fn serve<V, S>(addr: &ApiAddr, manager: Manager<V>, create: bool, stop: S) -> io::Result<()>
where
    V: Instance,
    S: Future<Output = ()> + Send + 'static,
{
    let router = router(manager, create);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let signaled = signaled();
        let shutdown = async move {
            tokio::select! {
                _ = stop => {}
                ret = signaled => {
                    if let Err(e) = ret {
                        log::error!("Failed to wait for signals: {e}");
                    }
                }
            }
        };
        match addr {
            ApiAddr::Unix(path) => {
                let listener = UnixListener::from_std(bind_unix(path)?)?;
                log::info!("Serving the API at unix:{}", path.display());
                let ret = axum::serve(listener, router)
                    .with_graceful_shutdown(shutdown)
                    .await;
                if let Err(e) = fs::remove_file(path) {
                    log::error!("Failed to remove {}: {e}", path.display());
                }
                ret
            }
            ApiAddr::Tcp(addr) => {
                let listener = TcpListener::bind(addr).await?;
                log::warn!("Serving the API at http://{addr} without authentication");
                axum::serve(listener, router)
                    .with_graceful_shutdown(shutdown)
                    .await
            }
        }
    })
}

#[cfg(test)]
#[path = "api_test.rs"]
mod tests;
