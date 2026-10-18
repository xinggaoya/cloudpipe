//! End-to-end integration tests for [`cloudpipe_sdk`].
//!
//! Each test spins up a tiny in-process HTTP server that mocks the subset
//! of the Cloudflare API the SDK talks to, points the SDK at it via
//! [`CloudflareApi::with_base`], and substitutes a shell script for the
//! `cloudflared` binary via [`TunnelBuilder::cloudflared_path`]. Nothing
//! here touches the network or a real Cloudflare account.
//!
//! Test isolation: every case gets its own [`tempfile::TempDir`] (which
//! cleans itself up) and its own mock server on an ephemeral port. The
//! earlier version derived temp paths from `process::id()`, which made
//! cases running in the same process race for the same directory.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cloudpipe_sdk::{CloudflareApi, Protocol, TunnelBuilder};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// A Cloudflare tunnel's CNAME target suffix.
const CFARGO: &str = ".cfargotunnel.com";

#[derive(Default)]
struct MockState {
    tunnels: Mutex<HashMap<String, String>>,
    records: Mutex<HashMap<String, String>>,
    deleted_records: Mutex<Vec<String>>,
    deleted_tunnels: Mutex<Vec<String>>,
    /// How many times a tunnel was created — lets a test assert that the
    /// supervisor really did respawn rather than re-reporting the same
    /// generation.
    tunnel_creation_count: AtomicUsize,
    /// Set to make `create_tunnel` fail N times, for retry-path tests.
    fail_create_times: AtomicUsize,
    /// Set to make `POST .../dns_records` fail, for rollback tests.
    fail_dns_create: AtomicBool,
    /// Last ingress configuration written per tunnel id.
    ingress: Mutex<HashMap<String, Value>>,
}

impl MockState {
    /// The service string the given tunnel's hostname rule points at.
    fn service_for(&self, tunnel_id: &str) -> Option<String> {
        let guard = self.ingress.lock().unwrap();
        let rules = guard.get(tunnel_id)?;
        rules
            .as_array()?
            .iter()
            .find(|r| r.get("hostname").is_some_and(|h| h.as_str().is_some()))?
            .get("service")?
            .as_str()
            .map(str::to_string)
    }
}

impl MockState {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn tunnels(&self) -> MutexGuard<'_, HashMap<String, String>> {
        self.tunnels.lock().unwrap()
    }

    fn records(&self) -> MutexGuard<'_, HashMap<String, String>> {
        self.records.lock().unwrap()
    }

    fn deleted_tunnels(&self) -> MutexGuard<'_, Vec<String>> {
        self.deleted_tunnels.lock().unwrap()
    }

    fn deleted_records(&self) -> MutexGuard<'_, Vec<String>> {
        self.deleted_records.lock().unwrap()
    }

    /// Fails the next `n` tunnel creations, then succeeds.
    fn fail_next_creations(&self, n: usize) {
        self.fail_create_times.store(n, Ordering::SeqCst);
    }

    fn set_fail_dns_create(&self, fail: bool) {
        self.fail_dns_create.store(fail, Ordering::SeqCst);
    }
}

type MutexGuard<'a, T> = std::sync::MutexGuard<'a, T>;

/// A parsed request as the mock sees it.
struct Req {
    method: String,
    path: String,
    query: HashMap<String, String>,
    body: Value,
}

/// A parsed HTTP request: headers, then a body sized by `Content-Length`.
///
/// The previous mock did a single `read()` and treated whatever arrived as
/// the whole message, so a request whose body straddled a packet boundary
/// (or one larger than the buffer) silently parsed as garbage.
async fn read_request(stream: &mut tokio::net::TcpStream) -> Option<Req> {
    let mut buf = Vec::with_capacity(8192);
    let mut chunk = [0u8; 4096];

    // Headers first.
    let head_end = loop {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            break pos + 4;
        }
        if buf.len() > 64 * 1024 {
            return None;
        }
    };

    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.lines();
    let mut request_line = lines.next().unwrap_or("").split_whitespace();
    let method = request_line.next().unwrap_or("").to_string();
    let target = request_line.next().unwrap_or("").to_string();

    let mut content_length = 0usize;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().unwrap_or(0);
            }
        }
    }

    // Then exactly `content_length` more bytes.
    while buf.len() < head_end + content_length {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }

    let body = if content_length > 0 && buf.len() >= head_end + content_length {
        serde_json::from_slice(&buf[head_end..head_end + content_length]).unwrap_or(Value::Null)
    } else {
        Value::Null
    };

    let (path, raw_query) = target.split_once('?').unwrap_or((target.as_str(), ""));
    let query = raw_query
        .split('&')
        .filter(|s| !s.is_empty())
        .filter_map(|pair| pair.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();

    Some(Req {
        method,
        path: path.to_string(),
        query,
        body,
    })
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Routes a parsed request to a canned Cloudflare response.
fn route(req: &Req, state: &MockState) -> (u16, Value) {
    let ok = |result: Value| (200, json!({ "success": true, "result": result }));

    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/user/tokens/verify") => {
            return ok(json!({ "id": "tok", "status": "active" }))
        }
        ("GET", "/zones") => {
            return ok(json!([{
                "id": "zone-1",
                "name": "example.com",
                "account": { "id": "acc-1" },
            }]))
        }
        _ => {}
    }

    if req.path.starts_with("/accounts/") {
        if req.method == "POST" && req.path.ends_with("/tunnels") {
            // Exhaust any pending failures before succeeding. `usize::MAX`
            // is the "fail forever" sentinel: it decrements forever without
            // ever wrapping.
            let remaining = state.fail_create_times.load(Ordering::SeqCst);
            if remaining > 0 {
                state.fail_create_times.store(remaining - 1, Ordering::SeqCst);
                return (
                    500,
                    json!({"success": false, "result": null,
                           "errors": [{"code": 1000, "message": "transient"}]}),
                );
            }
            let name = req
                .body
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            let id = format!("tun-{name}-{}", state.tunnel_creation_count.load(Ordering::SeqCst));
            state
                .tunnels()
                .insert(name.clone(), id.clone());
            state.tunnel_creation_count.fetch_add(1, Ordering::SeqCst);
            return ok(json!({
                "id": id, "name": name, "status": "healthy", "token": "fake-token"
            }));
        }

        if req.method == "DELETE" {
            // `/tunnels/{id}/connections` is the "kill live connections"
            // call, not a tunnel deletion. It must not be counted as one,
            // and it must not remove the tunnel from the live set.
            if let Some(tid) = req.path.strip_suffix("/connections") {
                let tid = tid.rsplit('/').next().unwrap_or("").to_string();
                let _ = tid;
                return ok(Value::Null);
            }
            let tid = req.path.rsplit('/').next().unwrap_or("").to_string();
            state.tunnels().retain(|_, id| *id != tid);
            state.deleted_tunnels().push(tid);
            return ok(Value::Null);
        }

        if req.method == "PUT" && req.path.contains("/cfd_tunnel/") {
            let tid = req
                .path
                .split("/cfd_tunnel/")
                .nth(1)
                .and_then(|rest| rest.split('/').next())
                .unwrap_or("")
                .to_string();
            let ingress = req
                .body
                .get("config")
                .and_then(|c| c.get("ingress"))
                .cloned()
                .unwrap_or(Value::Null);
            state.ingress.lock().unwrap().insert(tid, ingress);
            return ok(Value::Null);
        }

        if req.method == "GET" && req.path.ends_with("/tunnels") {
            // Name/status filters are honoured so conflict resolution can
            // be exercised.
            let name = req.query.get("name").cloned();
            let status = req.query.get("status").cloned();
            let mut out = Vec::new();
            for (tname, id) in state.tunnels().iter() {
                if let Some(want) = &name {
                    if tname != want {
                        continue;
                    }
                }
                if let Some(status) = &status {
                    // Only `healthy` matches, which is what the SDK probes.
                    if status != "healthy" {
                        continue;
                    }
                }
                out.push(json!({"id": id, "name": tname, "status": "healthy"}));
            }
            return ok(Value::Array(out));
        }

        if req.method == "GET" && req.path.contains("/tunnels/") {
            let tid = req.path.rsplit('/').next().unwrap_or("");
            if state.tunnels().values().any(|id| id == tid) {
                return ok(json!({"id": tid, "name": "x", "status": "healthy"}));
            }
            return (
                404,
                json!({"success": false, "result": null,
                       "errors": [{"code": 0, "message": "not found"}]}),
            );
        }
    }

    if req.path.starts_with("/zones/") && req.path.contains("/dns_records") {
        if req.method == "GET" {
            let want = req.query.get("name");
            let out: Vec<Value> = state
                .records()
                .iter()
                .filter(|(name, _)| want.as_ref().is_none_or(|w| name == w))
                .map(|(name, id)| {
                    json!({"id": id, "name": name, "content": "tun-x.cfargotunnel.com"})
                })
                .collect();
            return ok(Value::Array(out));
        }
        if req.method == "POST" {
            if state.fail_dns_create.load(Ordering::SeqCst) {
                return (
                    400,
                    json!({"success": false, "result": null,
                           "errors": [{"code": 81057, "message": "record invalid"}]}),
                );
            }
            let name = req
                .body
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let id = format!("rec-{}", state.records().len());
            state.records().insert(name.clone(), id.clone());
            return ok(json!({"id": id, "name": name, "content": "tun-x.cfargotunnel.com"}));
        }
        if req.method == "DELETE" {
            let rid = req.path.rsplit('/').next().unwrap_or("").to_string();
            state.records().retain(|_, id| *id != rid);
            state.deleted_records().push(rid);
            return ok(Value::Null);
        }
    }

    (
        404,
        json!({"success": false, "result": null,
               "errors": [{"code": 0,
                           "message": format!("unmocked: {} {}", req.method, req.path)}]}),
    )
}

/// Starts the mock API and returns its base URL.
async fn spawn_mock_api(state: Arc<MockState>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let state = Arc::clone(&state);
            tokio::spawn(async move {
                let Some(req) = read_request(&mut stream).await else {
                    return;
                };
                let (status, body) = route(&req, &state);
                let payload = body.to_string();
                let response = format!(
                    "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                    payload.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });
    format!("http://{addr}")
}

/// Writes a `cloudflared` stand-in that announces one edge connection and
/// then idles until it is signalled.
///
/// The `term_marker` file is touched from a TERM trap, which lets a test
/// assert that shutdown signals the process *before* the SDK starts
/// deleting Cloudflare resources.
async fn write_fake_cloudflared(
    dir: &Path,
    markers: Option<(&Path, &Path)>,
) -> std::path::PathBuf {
    let path = dir.join("cloudflared");
    let script = match markers {
        Some((ready, term)) => format!(
            "#!/bin/sh\ntrap 'echo terminated > \"{term}\"; exit 0' TERM INT\n\
             echo \"2025-01-01 Registered tunnel connection connIndex=0\"\n\
             echo up > \"{ready}\"\n\
             while true; do sleep 0.05; done\n",
            term = term.display(),
            ready = ready.display()
        ),
        None => "#!/bin/sh\ntrap 'exit 0' TERM INT\n\
                  echo \"2025-01-01 Registered tunnel connection connIndex=0\"\n\
                  while true; do sleep 0.05; done\n"
            .to_string(),
    };
    write_script(&path, &script).await;
    path
}

/// A `cloudflared` stand-in whose first invocation exits immediately (a
/// simulated crash) and later ones idle forever.
async fn write_crashing_cloudflared(
    dir: &Path,
    counter_file: &Path,
) -> std::path::PathBuf {
    let path = dir.join("cloudflared");
    let script = format!(
        "#!/bin/sh\ntrap 'exit 0' TERM INT\nCOUNTER_FILE=\"{}\"\n\
         count=$(cat \"$COUNTER_FILE\" 2>/dev/null || echo 0)\n\
         count=$((count + 1))\necho \"$count\" > \"$COUNTER_FILE\"\n\
         echo \"2025-01-01 Registered tunnel connection connIndex=0\"\n\
         if [ \"$count\" = \"1\" ]; then exit 1; fi\n\
         while true; do sleep 0.05; done\n",
        counter_file.display()
    );
    write_script(&path, &script).await;
    path
}

async fn write_script(path: &Path, script: &str) {
    tokio::fs::write(path, script).await.unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = tokio::fs::metadata(path).await.unwrap().permissions();
        perms.set_mode(0o755);
        tokio::fs::set_permissions(path, perms).await.unwrap();
    }
}

fn api_for(base: &str) -> CloudflareApi {
    CloudflareApi::with_base("test-token", base).unwrap()
}

async fn wait_until(mut predicate: impl FnMut() -> bool, budget: Duration, what: &str) {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if predicate() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {what}");
}

// ---------------------------------------------------------------------------
// API surface against the mock
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cloudflare_api_round_trips_against_mock() {
    let state = MockState::new();
    let base = spawn_mock_api(state.clone()).await;
    let api = api_for(&base);

    let status = api.verify_token().await.unwrap();
    assert_eq!(status.status, "active");

    let zone = api.find_zone("example.com").await.unwrap().expect("zone");
    assert_eq!(zone.require_account_id().unwrap(), "acc-1");

    let tunnel = api.create_tunnel("acc-1", "myapp").await.unwrap();
    assert_eq!(tunnel.name, "myapp");
    assert_eq!(state.tunnels().get("myapp"), Some(&tunnel.id));

    let ingress = vec![
        cloudpipe_sdk::IngressEntry {
            hostname: Some("myapp.example.com".into()),
            service: "http://localhost:8080".into(),
            origin_request: Some(cloudpipe_sdk::OriginRequest::insecure()),
        },
        cloudpipe_sdk::IngressEntry {
            hostname: None,
            service: "http_status:404".into(),
            origin_request: None,
        },
    ];
    api.set_tunnel_ingress("acc-1", &tunnel.id, &ingress)
        .await
        .unwrap();

    let record = api
        .create_dns_record(
            "zone-1",
            "myapp.example.com",
            &format!("{}{CFARGO}", tunnel.id),
        )
        .await
        .unwrap();
    assert!(state.records().contains_key("myapp.example.com"));
    assert!(api
        .find_dns_record("zone-1", "myapp.example.com")
        .await
        .unwrap()
        .is_some());

    api.delete_dns_record("zone-1", &record.id).await.unwrap();
    api.cleanup_connections("acc-1", &tunnel.id).await.unwrap();
    api.delete_tunnel("acc-1", &tunnel.id).await.unwrap();

    assert!(state.deleted_records().contains(&record.id));
    assert!(state.deleted_tunnels().contains(&tunnel.id));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn null_list_result_is_an_error_not_an_empty_list() {
    // Regression guard for the old `T: Default` design, which turned
    // `result: null` into an empty vec and reported "token is valid but has
    // access to no zones".
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let _ = read_request(&mut stream).await;
                let body = r#"{"success":true,"result":null}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });

    let api = CloudflareApi::with_base("t", format!("http://{addr}")).unwrap();
    let err = api.list_zones().await.unwrap_err();
    assert!(
        format!("{err:#}").contains("null result"),
        "expected an explicit null-result error, got: {err:#}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pagination_collects_every_page() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let Some(req) = read_request(&mut stream).await else {
                    return;
                };
                let page: u32 = req
                    .query
                    .get("page")
                    .and_then(|p| p.parse().ok())
                    .unwrap_or(1);
                let body = json!({
                    "success": true,
                    "result": [
                        {"id": format!("z{page}a"), "name": format!("a{page}.test"),
                         "account": {"id": "acc-1"}},
                        {"id": format!("z{page}b"), "name": format!("b{page}.test"),
                         "account": {"id": "acc-1"}},
                    ],
                    "result_info": {"page": page, "per_page": 2, "total_pages": 3},
                })
                .to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });

    let api = CloudflareApi::with_base("t", format!("http://{addr}")).unwrap();
    let zones = api.list_zones().await.unwrap();
    assert_eq!(zones.len(), 6, "3 pages × 2 rows");
    assert_eq!(zones[0].name, "a1.test");
    assert_eq!(zones[5].name, "b3.test");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rate_limits_are_retried_transparently() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_task = Arc::clone(&calls);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let n = calls_for_task.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let _ = read_request(&mut stream).await;
                let (status_line, body) = if n < 2 {
                    (
                        "429 Too Many Requests",
                        r#"{"success":false,"result":null,"errors":[{"code":10429,"message":"slow down"}]}"#,
                    )
                } else {
                    ("200 OK", r#"{"success":true,"result":[]}"#)
                };
                let response = format!(
                    "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });

    let api = CloudflareApi::with_base("t", format!("http://{addr}")).unwrap();
    let zones = api.list_zones().await.unwrap();
    assert!(zones.is_empty());
    assert!(
        calls.load(Ordering::SeqCst) >= 3,
        "expected at least one retry, saw {} calls",
        calls.load(Ordering::SeqCst)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn permanent_failures_are_not_retried() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_task = Arc::clone(&calls);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            calls_for_task.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let _ = read_request(&mut stream).await;
                let body =
                    r#"{"success":false,"result":null,"errors":[{"code":10001,"message":"bad auth"}]}"#;
                let response = format!(
                    "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });

    let api = CloudflareApi::with_base("t", format!("http://{addr}")).unwrap();
    let err = api.list_zones().await.unwrap_err();
    assert!(format!("{err:#}").contains("bad auth"));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "a 4xx must not be replayed"
    );
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_lifecycle_creates_then_cleans_up() {
    let state = MockState::new();
    let base = spawn_mock_api(state.clone()).await;
    let tmp = tempfile::tempdir().unwrap();
    let binary = write_fake_cloudflared(tmp.path(), None).await;

    let mut handle = TunnelBuilder::new()
        .api(api_for(&base))
        .account("acc-1")
        .zone("zone-1")
        .domain("example.com")
        .protocol(Protocol::Http)
        .port(8080)
        .subdomain("lifecycle")
        .cloudflared_path(binary)
        .start()
        .await
        .expect("start should succeed");

    assert_eq!(handle.url(), "https://lifecycle.example.com");
    assert_eq!(handle.subdomain(), "lifecycle");
    assert_eq!(handle.full_name(), "lifecycle.example.com");

    assert_eq!(state.tunnels().len(), 1);
    assert_eq!(state.records().len(), 1);
    assert!(state.deleted_tunnels().is_empty());
    assert!(state.deleted_records().is_empty());

    handle.stop().await.expect("stop should succeed");

    assert!(
        !state.deleted_tunnels().is_empty(),
        "the tunnel must be deleted"
    );
    assert!(
        !state.deleted_records().is_empty(),
        "the DNS record must be deleted"
    );
    // The real invariant: nothing may survive on the account. Counting the
    // DELETE calls exactly would instead pin the number of round-trips,
    // which is an implementation detail — `purge` collapses concurrent
    // callers via a shared flag and that must stay free to change.
    assert!(state.tunnels().is_empty(), "no tunnel may be left behind");
    assert!(state.records().is_empty(), "no DNS record may be left behind");

    let err = handle.stop().await.unwrap_err();
    assert!(matches!(err, cloudpipe_sdk::Error::AlreadyShutDown));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_signals_cloudflared_before_touching_cloudflare() {
    let state = MockState::new();
    let base = spawn_mock_api(state.clone()).await;
    let tmp = tempfile::tempdir().unwrap();
    let ready = tmp.path().join("ready-marker");
    let term = tmp.path().join("term-marker");
    let binary = write_fake_cloudflared(tmp.path(), Some((&ready, &term))).await;

    let mut handle = TunnelBuilder::new()
        .api(api_for(&base))
        .account("acc-1")
        .zone("zone-1")
        .domain("example.com")
        .subdomain("order")
        .cloudflared_path(binary)
        .start()
        .await
        .expect("start should succeed");

    wait_until(
        || ready.exists(),
        Duration::from_secs(5),
        "cloudflared to start",
    )
    .await;
    assert!(!term.exists(), "the TERM trap must not have run yet");
    assert!(state.deleted_tunnels().is_empty());

    handle.stop().await.expect("stop should succeed");

    // The TERM trap ran, i.e. the process was signalled. The SDK used to
    // delete the DNS record and the tunnel first, failing every in-flight
    // request; this asserts the ordering.
    assert!(
        term.exists(),
        "cloudflared must receive a termination signal during shutdown"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bootstrap_failure_after_dns_creation_rolls_everything_back() {
    let state = MockState::new();
    let base = spawn_mock_api(state.clone()).await;
    let tmp = tempfile::tempdir().unwrap();

    // A `cloudflared` path that does not exist makes the spawn step fail —
    // the step that used to leak both a tunnel and a DNS record.
    let missing = tmp.path().join("definitely-not-here");

    let err = TunnelBuilder::new()
        .api(api_for(&base))
        .account("acc-1")
        .zone("zone-1")
        .domain("example.com")
        .subdomain("rollback")
        .cloudflared_path(missing)
        .start()
        .await
        .expect_err("spawning a nonexistent binary must fail");

    assert!(
        format!("{err:#}").to_lowercase().contains("spawn"),
        "unexpected error: {err:#}"
    );

    assert_eq!(
        state.deleted_tunnels().len(),
        1,
        "the tunnel created before the failure must be rolled back"
    );
    assert_eq!(
        state.deleted_records().len(),
        1,
        "the DNS record created before the failure must be rolled back"
    );
    assert!(state.tunnels().is_empty());
    assert!(state.records().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dns_creation_failure_removes_the_new_tunnel() {
    let state = MockState::new();
    state.set_fail_dns_create(true);
    let base = spawn_mock_api(state.clone()).await;
    let tmp = tempfile::tempdir().unwrap();
    let binary = write_fake_cloudflared(tmp.path(), None).await;

    TunnelBuilder::new()
        .api(api_for(&base))
        .account("acc-1")
        .zone("zone-1")
        .domain("example.com")
        .subdomain("dnsfail")
        .cloudflared_path(binary)
        .start()
        .await
        .expect_err("DNS creation was made to fail");

    assert_eq!(state.deleted_tunnels().len(), 1);
    assert!(state.tunnels().is_empty());
    assert!(state.records().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_subdomain_is_rejected_before_any_side_effect() {
    let state = MockState::new();
    let base = spawn_mock_api(state.clone()).await;

    // No `cloudflared_path`: had validation run late, this would have
    // reached `ensure_installed` and tried a ~30 MB download first.
    let err = TunnelBuilder::new()
        .api(api_for(&base))
        .account("acc-1")
        .zone("zone-1")
        .domain("example.com")
        .subdomain("Not Valid")
        .start()
        .await
        .expect_err("an invalid subdomain must be rejected");

    assert!(matches!(err, cloudpipe_sdk::Error::InvalidSubdomain(_)));
    assert_eq!(
        state.tunnel_creation_count.load(Ordering::SeqCst),
        0,
        "no tunnel may be created for an invalid subdomain"
    );
}

// ---------------------------------------------------------------------------
// Auto-restart
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_restart_respawns_after_cloudflared_crash() {
    let state = MockState::new();
    let base = spawn_mock_api(state.clone()).await;
    let tmp = tempfile::tempdir().unwrap();
    let counter = tmp.path().join("counter");
    let binary = write_crashing_cloudflared(tmp.path(), &counter).await;

    let mut handle = TunnelBuilder::new()
        .api(api_for(&base))
        .account("acc-1")
        .zone("zone-1")
        .domain("example.com")
        .subdomain("restart-test")
        .auto_restart(true)
        .cloudflared_path(binary)
        .start()
        .await
        .expect("start should succeed");

    assert_eq!(handle.full_name(), "restart-test.example.com");

    wait_until(
        || {
            state.tunnel_creation_count.load(Ordering::SeqCst) >= 2
                && std::fs::read_to_string(&counter)
                    .map(|c| c.trim().parse::<u32>().unwrap_or(0) >= 2)
                    .unwrap_or(false)
        },
        Duration::from_secs(10),
        "auto-restart to respawn",
    )
    .await;

    let runs = std::fs::read_to_string(&counter)
        .unwrap()
        .trim()
        .parse::<u32>()
        .unwrap();
    assert!(runs >= 2, "cloudflared should have been respawned: {runs}");

    handle.stop().await.expect("stop should succeed");
    assert!(
        !state.deleted_tunnels().is_empty(),
        "stop() must clean up the live tunnel"
    );
    assert!(state.tunnels().is_empty(), "no tunnel may survive stop()");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_restart_off_terminates_on_crash() {
    let state = MockState::new();
    let base = spawn_mock_api(state.clone()).await;
    let tmp = tempfile::tempdir().unwrap();
    let counter = tmp.path().join("counter");
    let binary = write_crashing_cloudflared(tmp.path(), &counter).await;

    let mut handle = TunnelBuilder::new()
        .api(api_for(&base))
        .account("acc-1")
        .zone("zone-1")
        .domain("example.com")
        .subdomain("no-restart")
        .auto_restart(false)
        .cloudflared_path(binary)
        .start()
        .await
        .expect("start should succeed");

    tokio::time::timeout(Duration::from_secs(10), handle.wait())
        .await
        .expect("wait should return after cloudflared exits");

    assert_eq!(
        state.tunnel_creation_count.load(Ordering::SeqCst),
        1,
        "no respawn should occur with auto_restart=false"
    );
    assert_eq!(
        std::fs::read_to_string(&counter).unwrap().trim(),
        "1",
        "cloudflared ran exactly once"
    );
    assert!(
        state.tunnels().is_empty(),
        "the crashed generation's tunnel must be reclaimed"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_restart_gives_up_after_repeated_bootstrap_failures() {
    let state = MockState::new();
    let base = spawn_mock_api(state.clone()).await;
    let tmp = tempfile::tempdir().unwrap();
    // The first generation must crash, otherwise the supervisor sits in
    // `wait_for_exit` and never reaches the retry ladder at all.
    let counter = tmp.path().join("counter");
    let binary = write_crashing_cloudflared(tmp.path(), &counter).await;

    let mut handle = TunnelBuilder::new()
        .api(api_for(&base))
        .account("acc-1")
        .zone("zone-1")
        .domain("example.com")
        .subdomain("giveup")
        .auto_restart(true)
        // Millisecond ladder so the test observes the give-up path without
        // waiting out the production backoff.
        .restart_policy(cloudpipe_sdk::RestartPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(10),
            max_delay: Duration::from_millis(40),
        })
        .cloudflared_path(binary)
        .start()
        .await
        .expect("the initial bootstrap succeeds");

    // Now make every reconnect attempt fail. The supervisor must give up
    // rather than retrying (and burning the token's quota) forever.
    state.fail_next_creations(usize::MAX);

    // The loop must terminate on its own rather than spinning forever.
    tokio::time::timeout(Duration::from_secs(10), handle.wait())
        .await
        .expect("the supervisor must give up rather than retry forever");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wait_is_passive_and_does_not_clean_up() {
    let state = MockState::new();
    let base = spawn_mock_api(state.clone()).await;
    let tmp = tempfile::tempdir().unwrap();
    let binary = write_fake_cloudflared(tmp.path(), None).await;

    let mut handle = TunnelBuilder::new()
        .api(api_for(&base))
        .account("acc-1")
        .zone("zone-1")
        .domain("example.com")
        .subdomain("wait-passive")
        .cloudflared_path(binary)
        .start()
        .await
        .expect("start should succeed");

    assert_eq!(state.tunnels().len(), 1);
    assert_eq!(state.records().len(), 1);

    // The fake cloudflared idles forever, so a passive `wait()` stays
    // pending. An active one would tear the tunnel down immediately.
    let timed_out = tokio::time::timeout(Duration::from_millis(500), handle.wait())
        .await
        .is_err();
    assert!(timed_out, "wait() must not signal shutdown");

    assert!(state.deleted_tunnels().is_empty(), "wait() deleted the tunnel");
    assert!(state.deleted_records().is_empty(), "wait() deleted the DNS");

    handle.stop().await.expect("stop should succeed");
    assert!(!state.deleted_tunnels().is_empty());
    assert!(!state.deleted_records().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_port_republishes_ingress_and_keeps_the_url() {
    let state = MockState::new();
    let base = spawn_mock_api(state.clone()).await;
    let tmp = tempfile::tempdir().unwrap();
    let binary = write_fake_cloudflared(tmp.path(), None).await;

    let mut handle = TunnelBuilder::new()
        .api(api_for(&base))
        .account("acc-1")
        .zone("zone-1")
        .domain("example.com")
        .protocol(Protocol::Http)
        .port(8080)
        .subdomain("moving")
        .cloudflared_path(binary)
        .start()
        .await
        .expect("start should succeed");

    let tunnel_id = state.tunnels().values().next().cloned().unwrap();
    assert_eq!(handle.port(), 8080);
    assert_eq!(
        state.service_for(&tunnel_id).as_deref(),
        Some("http://localhost:8080")
    );

    handle
        .update_port(9090)
        .await
        .expect("update_port should succeed");

    // The public URL is unchanged — that is the entire point of
    // update_port — and only the local origin moved.
    assert_eq!(handle.url(), "https://moving.example.com");
    assert_eq!(handle.port(), 9090);
    assert_eq!(
        state.service_for(&tunnel_id).as_deref(),
        Some("http://localhost:9090")
    );

    // Nothing was recreated: still one tunnel, one DNS record.
    assert_eq!(state.tunnel_creation_count.load(Ordering::SeqCst), 1);
    assert_eq!(state.records().len(), 1);

    assert!(
        handle.update_port(0).await.is_err(),
        "port 0 must be refused"
    );

    handle.stop().await.expect("stop should succeed");
    assert!(
        handle.update_port(3000).await.is_err(),
        "a stopped handle must refuse updates"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preview_cleanup_changes_nothing() {
    let state = MockState::new();
    let base = spawn_mock_api(state.clone()).await;
    let api = api_for(&base);

    // A stale tunnel plus its DNS record is two resources, not three — the
    // old count attributed 2 per tunnel.
    state.tunnels().insert("dead".into(), "tun-dead".into());
    state
        .records()
        .insert("dead.example.com".into(), "rec-dead".into());

    let report = cloudpipe_sdk::preview_cleanup(&api, "acc-1", "zone-1", "example.com")
        .await
        .expect("preview should succeed");

    assert!(
        state.deleted_tunnels().is_empty(),
        "a dry run must not delete anything"
    );
    assert!(
        state.deleted_records().is_empty(),
        "a dry run must not delete anything"
    );
    assert_eq!(report.removed.len(), 0);
    assert_eq!(report.removed_count(), 0);
}

async fn dropping_the_handle_still_releases_cloudflare_resources() {
    let state = MockState::new();
    let base = spawn_mock_api(state.clone()).await;
    let tmp = tempfile::tempdir().unwrap();
    let binary = write_fake_cloudflared(tmp.path(), None).await;

    {
        let handle = TunnelBuilder::new()
            .api(api_for(&base))
            .account("acc-1")
            .zone("zone-1")
            .domain("example.com")
            .subdomain("dropped")
            .cloudflared_path(binary)
            .start()
            .await
            .expect("start should succeed");
        assert_eq!(state.tunnels().len(), 1);
        drop(handle);
    }

    // The previous implementation only aborted the supervisor task, which
    // left the tunnel and its DNS record on the account with no way to
    // reclaim them except a manual `cfp cleanup`.
    wait_until(
        || state.tunnels().is_empty() && state.records().is_empty(),
        Duration::from_secs(5),
        "Drop to release the Cloudflare resources",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn generated_subdomains_do_not_collide() {
    // The old generator produced `user-0000`..`user-9999` — 10k values.
    let mut seen = std::collections::HashSet::new();
    for _ in 0..200 {
        let name = cloudpipe_sdk::random_subdomain();
        assert!(
            cloudpipe_sdk::validate_subdomain(&name).is_ok(),
            "generated an invalid DNS label: {name}"
        );
        assert!(seen.insert(name.clone()), "collision on {name}");
    }
}

// ---------------------------------------------------------------------------
// Builder-level checks
// ---------------------------------------------------------------------------

#[test]
fn builder_validate_rejects_missing_credentials() {
    let err = TunnelBuilder::new().validate().unwrap_err();
    assert!(matches!(
        err,
        cloudpipe_sdk::Error::MissingCredential("Cloudflare API token")
    ));
    TunnelBuilder::new().token("t").validate().unwrap();
}

#[test]
fn builder_clone_is_safe() {
    let b = TunnelBuilder::new()
        .token("t")
        .account("a")
        .zone("z")
        .domain("example.com");
    let _clone = b.clone();
}

#[test]
fn origin_request_builders_compose() {
    let b = TunnelBuilder::new()
        .token("t")
        .no_tls_verify(true)
        .origin_connect_timeout(5)
        .origin_host_header("localhost");
    let debug = format!("{b:?}");
    assert!(debug.contains("no_tls_verify"), "{debug}");
    assert!(debug.contains("connect_timeout"), "{debug}");
    assert!(debug.contains("http_host_header"), "{debug}");
}
