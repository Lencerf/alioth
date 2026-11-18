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

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use alioth::api::{Code, Error, Instance, VmManager, VmState};
use alioth::cpu::State;
use alioth::vm::config::VmSpec;
use axum::Router;
use axum::body::{self, Body};
use axum::http::header::{ALLOW, CONTENT_TYPE};
use axum::http::{Method, Request, StatusCode};
use parking_lot::{Condvar, Mutex};
use rstest::rstest;
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::api::{ApiAddr, router};

#[derive(Debug)]
struct FakeVm {
    state: Mutex<State>,
    cond: Condvar,
}

impl FakeVm {
    fn transit(&self, want: &[State], next: State) -> Result<(), Error> {
        let mut state = self.state.lock();
        if !want.contains(&state) {
            let msg = format!("the VM is {}", VmState::from(*state));
            return Err(Error::new(Code::FailedPrecondition, msg));
        }
        *state = next;
        self.cond.notify_all();
        Ok(())
    }
}

impl Instance for FakeVm {
    type Hypervisor = ();

    fn create(_: &(), _: VmSpec) -> Result<Self, Error> {
        Ok(FakeVm {
            state: Mutex::new(State::Stopped),
            cond: Condvar::new(),
        })
    }

    fn state(&self) -> State {
        *self.state.lock()
    }

    fn boot(&self) -> Result<(), Error> {
        self.transit(&[State::Stopped], State::Running)
    }

    fn shutdown(&self) -> Result<(), Error> {
        self.transit(&[State::Running, State::Paused], State::Stopped)
    }

    fn reboot(&self) -> Result<(), Error> {
        self.transit(&[State::Running], State::Running)
    }

    fn pause(&self) -> Result<(), Error> {
        self.transit(&[State::Running], State::Paused)
    }

    fn resume(&self) -> Result<(), Error> {
        self.transit(&[State::Paused], State::Running)
    }

    fn wait_stopped(&self) {
        let mut state = self.state.lock();
        self.cond.wait_while(&mut state, |s| {
            !matches!(s, State::Stopped | State::Terminated)
        });
    }

    fn terminate(&self) -> Result<(), Error> {
        self.transit(&[State::Stopped, State::Terminated], State::Terminated)
    }
}

fn fixture_router(create: bool) -> Router {
    router(Arc::new(VmManager::<FakeVm>::new(())), create)
}

const BODY: &str = r#"{"payload": {"executable": {"linux": "/vmlinuz"}}}"#;

/// Sends a request, and returns the status and the JSON body of the response.
async fn send(
    router: &Router,
    method: Method,
    uri: &str,
    body: Option<&str>,
) -> (StatusCode, Value) {
    let (status, _, value) = send_full(router, method, uri, body).await;
    (status, value)
}

async fn send_full(
    router: &Router,
    method: Method,
    uri: &str,
    body: Option<&str>,
) -> (StatusCode, Option<String>, Value) {
    let request = Request::builder().method(method).uri(uri);
    let request = match body {
        Some(body) => request
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_owned())),
        None => request.body(Body::empty()),
    };
    let response = router.clone().oneshot(request.unwrap()).await.unwrap();
    let status = response.status();
    let allow = response
        .headers()
        .get(ALLOW)
        .map(|v| v.to_str().unwrap().to_owned());
    let bytes = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, allow, serde_json::from_slice(&bytes).unwrap())
}

fn assert_error(value: &Value, code: u16, status: &str) {
    assert_eq!(value["error"]["code"], code, "{value}");
    assert_eq!(value["error"]["status"], status, "{value}");
}

#[tokio::test]
async fn test_resource() {
    let router = fixture_router(true);

    // Omitted fields get the defaults.
    let (status, vm) = send(&router, Method::POST, "/v1/vms?vm_id=web", Some(BODY)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(vm["name"], "vms/web");
    assert_eq!(vm["state"], "STOPPED");
    assert_eq!(vm["payload"]["executable"], json!({"linux": "/vmlinuz"}));
    assert_eq!(vm["board"]["cpu"]["count"], 1);
    assert_eq!(vm["board"]["mem"]["size"], 1u64 << 30);
    assert_eq!(vm["console"], json!({"pty": {"link": null}}));
    assert_eq!(vm["net"], json!([]));

    let (status, vm) = send(&router, Method::POST, "/v1/vms", Some(BODY)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(vm["name"], "vms/vm0");
    let (status, value) = send(&router, Method::POST, "/v1/vms?vm_id=web", Some(BODY)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_error(&value, 409, "ALREADY_EXISTS");

    let (status, vm) = send(&router, Method::GET, "/v1/vms/web", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(vm["name"], "vms/web");

    let (status, page) = send(&router, Method::GET, "/v1/vms?page_size=1", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["vms"][0]["name"], "vms/vm0");
    assert_eq!(page["next_page_token"], "vm0");
    let uri = "/v1/vms?page_size=1&page_token=vm0";
    let (_, page) = send(&router, Method::GET, uri, None).await;
    assert_eq!(page["vms"][0]["name"], "vms/web");
    assert_eq!(page["next_page_token"], "");

    let (status, value) = send(&router, Method::DELETE, "/v1/vms/web", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(value, json!({}));
    let (status, value) = send(&router, Method::GET, "/v1/vms/web", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_error(&value, 404, "NOT_FOUND");
}

#[tokio::test]
async fn test_methods() {
    let router = fixture_router(true);
    send(&router, Method::POST, "/v1/vms", Some(BODY)).await;

    let (status, vm) = send(&router, Method::POST, "/v1/vms/vm0:boot", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(vm["state"], "RUNNING");
    let (status, value) = send(&router, Method::POST, "/v1/vms/vm0:boot", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_error(&value, 400, "FAILED_PRECONDITION");

    let (_, vm) = send(&router, Method::POST, "/v1/vms/vm0:reboot", None).await;
    assert_eq!(vm["state"], "RUNNING");

    let (status, vm) = send(&router, Method::POST, "/v1/vms/vm0:pause", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(vm["state"], "PAUSED");
    let (status, value) = send(&router, Method::POST, "/v1/vms/vm0:pause", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_error(&value, 400, "FAILED_PRECONDITION");
    let (status, vm) = send(&router, Method::POST, "/v1/vms/vm0:resume", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(vm["state"], "RUNNING");

    let (_, vm) = send(&router, Method::POST, "/v1/vms/vm0:shutdown", None).await;
    assert_eq!(vm["state"], "STOPPED");
}

#[tokio::test]
async fn test_alias() {
    let router = fixture_router(true);
    send(&router, Method::POST, "/v1/vms", Some(BODY)).await;

    let (_, vm) = send(&router, Method::GET, "/v1/vm", None).await;
    assert_eq!(vm["name"], "vms/vm0");
    let (_, vm) = send(&router, Method::POST, "/v1/vm:boot", None).await;
    assert_eq!(vm["state"], "RUNNING");
    let (_, vm) = send(&router, Method::POST, "/v1/vm:pause", None).await;
    assert_eq!(vm["state"], "PAUSED");
    let (_, vm) = send(&router, Method::POST, "/v1/vm:resume", None).await;
    assert_eq!(vm["state"], "RUNNING");
    let (_, vm) = send(&router, Method::POST, "/v1/vm:shutdown", None).await;
    assert_eq!(vm["state"], "STOPPED");
    let (status, _) = send(&router, Method::DELETE, "/v1/vm", None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = send(&router, Method::GET, "/v1/vm", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[rstest]
#[case(Method::POST, "/v1/vms", Some("{"))]
#[case(Method::POST, "/v1/vms", Some("{}"))]
#[case(Method::POST, "/v1/vms", Some(r#"{"payload": {}}"#))]
#[case(Method::POST, "/v1/vms?vm_id=Web", Some(BODY))]
#[case(Method::GET, "/v1/vms?page_size=-1", None)]
#[case(Method::GET, "/v1/vms?page_token=..", None)]
#[case(Method::POST, "/v1/vms/%FF:boot", None)]
#[tokio::test]
async fn test_invalid_argument(
    #[case] method: Method,
    #[case] uri: &str,
    #[case] body: Option<&str>,
) {
    let router = fixture_router(true);
    let (status, value) = send(&router, method, uri, body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_error(&value, 400, "INVALID_ARGUMENT");
}

#[tokio::test]
async fn test_method_not_allowed() {
    // Without Create, e.g. for `alioth boot --api`.
    let router = fixture_router(false);
    let (status, allow, value) = send_full(&router, Method::POST, "/v1/vms", Some(BODY)).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(allow.as_deref(), Some("GET,HEAD"));
    assert_error(&value, 405, "UNIMPLEMENTED");

    let (status, _) = send(&router, Method::GET, "/v1/vms", None).await;
    assert_eq!(status, StatusCode::OK);

    // POST on a VM needs a custom method that exists.
    for uri in ["/v1/vms/vm0", "/v1/vms/vm0:hibernate"] {
        let (status, allow, value) = send_full(&router, Method::POST, uri, None).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(allow.as_deref(), Some("GET,HEAD,DELETE"));
        assert_error(&value, 405, "UNIMPLEMENTED");
    }
}

#[tokio::test]
async fn test_not_found() {
    let router = fixture_router(true);
    let (status, value) = send(&router, Method::GET, "/v2/vms", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_error(&value, 404, "NOT_FOUND");
}

#[rstest]
#[case("unix:/run/alioth.sock", ApiAddr::Unix(Path::new("/run/alioth.sock").into()))]
#[case("http://localhost:8080", ApiAddr::Tcp(SocketAddr::from(([127, 0, 0, 1], 8080))))]
#[case("http://127.0.0.1:80/", ApiAddr::Tcp(SocketAddr::from(([127, 0, 0, 1], 80))))]
#[case("http://[::1]:9000", ApiAddr::Tcp("[::1]:9000".parse().unwrap()))]
fn test_api_addr(#[case] s: &str, #[case] want: ApiAddr) {
    assert_eq!(s.parse::<ApiAddr>().unwrap(), want);
}

#[rstest]
#[case("http://0.0.0.0:80")]
#[case("http://10.0.0.1:80")]
#[case("http://localhost:http")]
#[case("tcp://localhost:80")]
fn test_api_addr_invalid(#[case] s: &str) {
    assert!(s.parse::<ApiAddr>().is_err());
}
