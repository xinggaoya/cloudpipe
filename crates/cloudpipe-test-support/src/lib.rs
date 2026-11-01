//! Shared test doubles for the cloudpipe workspace.
//!
//! The Cloudflare API mock and the fake `cloudflared` scripts live here
//! rather than inside one crate's test module, so the SDK's integration
//! tests and the CLI's tests exercise the *same* mock. A second copy
//! would drift, and a drifting mock makes a green test suite meaningless.
//!
//! Nothing here touches the network or a real Cloudflare account.

// The mock's fields and helpers are a test double's innards: their names
// say what they are, and the behavioural notes that matter are already
// on the ones that need them. Documenting every field of a fake API
// would be noise.
#![allow(missing_docs)]

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// A Cloudflare tunnel's CNAME target suffix.
pub const CFARGO: &str = ".cfargotunnel.com";

#[derive(Default)]
pub struct MockState {
    pub tunnels: Mutex<HashMap<String, String>>,
    pub records: Mutex<HashMap<String, String>>,
    pub deleted_records: Mutex<Vec<String>>,
    pub deleted_tunnels: Mutex<Vec<String>>,
    /// How many times a tunnel was created — lets a test assert that the
    /// supervisor really did respawn rather than re-reporting the same
    /// generation.
    pub tunnel_creation_count: AtomicUsize,
    /// Set to make `create_tunnel` fail N times, for retry-path tests.
    pub fail_create_times: AtomicUsize,
    /// Set to make `POST .../dns_records` fail, for rollback tests.
    pub fail_dns_create: AtomicBool,
    /// Last ingress configuration written per tunnel id.
    pub ingress: Mutex<HashMap<String, Value>>,
    /// Set to make the zone listing carry no account reference.
    ///
    /// Cloudflare omits it for some tokens, and the CLI has to cope with
    /// that rather than losing the saved token over it.
    pub zones_without_account: AtomicBool,
}

impl MockState {
    /// The service string the given tunnel's hostname rule points at.
    pub fn service_for(&self, tunnel_id: &str) -> Option<String> {
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
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn tunnels(&self) -> MutexGuard<'_, HashMap<String, String>> {
        self.tunnels.lock().unwrap()
    }

    pub fn records(&self) -> MutexGuard<'_, HashMap<String, String>> {
        self.records.lock().unwrap()
    }

    pub fn deleted_tunnels(&self) -> MutexGuard<'_, Vec<String>> {
        self.deleted_tunnels.lock().unwrap()
    }

    pub fn deleted_records(&self) -> MutexGuard<'_, Vec<String>> {
        self.deleted_records.lock().unwrap()
    }

    /// Fails the next `n` tunnel creations, then succeeds.
    pub fn fail_next_creations(&self, n: usize) {
        self.fail_create_times.store(n, Ordering::SeqCst);
    }

    pub fn set_fail_dns_create(&self, fail: bool) {
        self.fail_dns_create.store(fail, Ordering::SeqCst);
    }
}

type MutexGuard<'a, T> = std::sync::MutexGuard<'a, T>;

/// A parsed request as the mock sees it.
pub struct Req {
    /// The HTTP method.
    pub method: String,
    /// The path, without the query string.
    pub path: String,
    /// The parsed query string.
    pub query: HashMap<String, String>,
    /// The parsed request body.
    pub body: Value,
}

/// A parsed HTTP request: headers, then a body sized by `Content-Length`.
///
/// The previous mock did a single `read()` and treated whatever arrived as
/// the whole message, so a request whose body straddled a packet boundary
/// (or one larger than the buffer) silently parsed as garbage.
pub async fn read_request(stream: &mut tokio::net::TcpStream) -> Option<Req> {
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
        ("GET", "/user/tokens/verify") => return ok(json!({ "id": "tok", "status": "active" })),
        ("GET", "/zones") => {
            let mut zone = json!({
                "id": "zone-1",
                "name": "example.com",
                "account": { "id": "acc-1" },
            });
            if state.zones_without_account.load(Ordering::SeqCst) {
                zone.as_object_mut()
                    .expect("a zone object")
                    .remove("account");
            }
            return ok(json!([zone]));
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
                state
                    .fail_create_times
                    .store(remaining - 1, Ordering::SeqCst);
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
            let id = format!(
                "tun-{name}-{}",
                state.tunnel_creation_count.load(Ordering::SeqCst)
            );
            state.tunnels().insert(name.clone(), id.clone());
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
                // Tunnels created with a name prefixed `dead-` are reported
                // as down, so cleanup's dead-tunnel branch is reachable.
                let reported_status = if tname.starts_with("dead-") {
                    "down"
                } else {
                    "healthy"
                };
                if let Some(want) = &status {
                    if want != reported_status {
                        continue;
                    }
                }
                out.push(json!({"id": id, "name": tname, "status": reported_status}));
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
pub async fn spawn_mock_api(state: Arc<MockState>) -> String {
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
pub async fn write_fake_cloudflared(
    dir: &Path,
    markers: Option<(&Path, &Path)>,
) -> std::path::PathBuf {
    // A `#!/bin/sh` script is not a runnable image on Windows — spawning
    // one fails with "%1 is not a valid Win32 application". Rust has
    // run `.bat`/`.cmd` files through `cmd.exe` since 1.77, so the Windows
    // fake is a batch file instead.
    #[cfg(windows)]
    {
        let path = dir.join("cloudflared.cmd");
        let script = match markers {
            Some((ready, _term)) => format!(
                "@echo off\r\n\
                 echo 2025-01-01 Registered tunnel connection connIndex=0 1>&2\r\n\
                 echo up > \"{ready}\"\r\n\
                 :loop\r\n\
                 ping -n 2 127.0.0.1 > nul\r\n\
                 goto loop\r\n",
                ready = ready.display()
            ),
            None => "@echo off\r\n\
                      echo 2025-01-01 Registered tunnel connection connIndex=0 1>&2\r\n\
                      :loop\r\n\
                      ping -n 2 127.0.0.1 > nul\r\n\
                      goto loop\r\n"
                .to_string(),
        };
        write_script(&path, &script).await;
        // The tail expression of the surviving `cfg` block: on Windows
        // this block is the last one left in the body, so it is the
        // function's return value. A `return` here would be the tail
        // expression too — and clippy's `needless_return` rejects that
        // on Windows while never seeing it on Unix.
        path
    }

    #[cfg(unix)]
    {
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
}

/// A `cloudflared` stand-in whose first invocation exits immediately (a
/// simulated crash) and later ones idle forever.
pub async fn write_crashing_cloudflared(dir: &Path, counter_file: &Path) -> std::path::PathBuf {
    // See `write_fake_cloudflared`: a shell script is not runnable on
    // Windows, so the crash-counter is a batch file there.
    #[cfg(windows)]
    {
        let path = dir.join("cloudflared.cmd");
        let script = format!(
            "@echo off\r\n\
             set \"COUNTER_FILE={}\"\r\n\
             set count=0\r\n\
             if exist \"%COUNTER_FILE%\" set /p count=<\"%COUNTER_FILE%\"\r\n\
             set /a count+=1\r\n\
             > \"%COUNTER_FILE%\" echo %count%\r\n\
             echo 2025-01-01 Registered tunnel connection connIndex=0 1>&2\r\n\
             if %count% == 1 exit /b 1\r\n\
             :loop\r\n\
             ping -n 2 127.0.0.1 > nul\r\n\
             goto loop\r\n",
            counter_file.display()
        );
        write_script(&path, &script).await;
        // See `write_fake_cloudflared`: the surviving `cfg` block is the
        // function's tail expression on this platform.
        path
    }

    #[cfg(unix)]
    {
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
}

pub async fn write_script(path: &Path, script: &str) {
    tokio::fs::write(path, script).await.unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = tokio::fs::metadata(path).await.unwrap().permissions();
        perms.set_mode(0o755);
        tokio::fs::set_permissions(path, perms).await.unwrap();
    }
}

pub async fn wait_until(mut predicate: impl FnMut() -> bool, budget: Duration, what: &str) {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if predicate() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {what}");
}
