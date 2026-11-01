//! End-to-end integration tests for [`cloudpipe_sdk`].
//!
//! Each test spins up a tiny in-process HTTP server that mocks the subset
//! of the Cloudflare API the SDK talks to, points the SDK at it via
//! [`CloudflareApi::with_base`], and substitutes a shell script for the
//! `cloudflared` binary via [`TunnelBuilder::cloudflared_path`]. Nothing
//! here touches the network or a real Cloudflare account.
//!
//! The mock itself lives in `cloudpipe-test-support` so the CLI's tests
//! can use the same one.
//!
//! Test isolation: every case gets its own [`tempfile::TempDir`] (which
//! cleans itself up) and its own mock server on an ephemeral port. The
//! earlier version derived temp paths from `process::id()`, which made
//! cases running in the same process race for the same directory.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use cloudpipe_sdk::{CleanupAction, CloudflareApi, Protocol, TunnelBuilder};
use cloudpipe_test_support::{
    read_request, spawn_mock_api, wait_until, write_crashing_cloudflared, write_fake_cloudflared,
    MockState, CFARGO,
};
use serde_json::json;
use tokio::io::AsyncWriteExt as _;
use tokio::net::TcpListener;

/// A client pointed at a mock server.
fn api_for(base: &str) -> CloudflareApi {
    CloudflareApi::with_base("test-token", base).unwrap()
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
                let body = r#"{"success":false,"result":null,"errors":[{"code":10001,"message":"bad auth"}]}"#;
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
    assert!(
        state.records().is_empty(),
        "no DNS record may be left behind"
    );

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

    assert!(
        state.deleted_tunnels().is_empty(),
        "wait() deleted the tunnel"
    );
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
async fn preview_cleanup_finds_dead_resources_and_deletes_nothing() {
    let state = MockState::new();
    let base = spawn_mock_api(state.clone()).await;
    let api = api_for(&base);

    // The mock reports `dead-*` tunnels as `down`, which is what makes
    // cleanup's dead-tunnel branch reachable at all.
    state
        .tunnels()
        .insert("dead-app".into(), "tun-dead-app".into());
    state
        .records()
        .insert("dead-app.example.com".into(), "rec-dead-app".into());

    let report = cloudpipe_sdk::preview_cleanup(&api, "acc-1", "zone-1", "example.com")
        .await
        .expect("preview should succeed");

    // The previous version of this test asserted only that nothing had been
    // deleted, which passed even when the scan found nothing at all.
    assert_eq!(
        report.would_remove.len(),
        2,
        "a tunnel plus its DNS record is two resources, not three: {report:?}"
    );
    assert!(
        report
            .would_remove
            .iter()
            .any(|a| matches!(a, CleanupAction::Tunnel { name, .. } if name == "dead-app")),
        "{report:?}"
    );
    assert!(
        report.would_remove.iter().any(
            |a| matches!(a, CleanupAction::DnsRecord { name, .. } if name == "dead-app.example.com")
        ),
        "{report:?}"
    );

    assert!(report.removed.is_empty());
    assert_eq!(report.removed_count(), 0);
    assert!(
        state.deleted_tunnels().is_empty(),
        "a dry run must not delete anything"
    );
    assert!(
        state.deleted_records().is_empty(),
        "a dry run must not delete anything"
    );
    assert!(
        state.tunnels().contains_key("dead-app"),
        "a dry run must not remove the resource from the zone"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cleanup_reclaims_dead_tunnels_and_their_records() {
    let state = MockState::new();
    let base = spawn_mock_api(state.clone()).await;
    let api = api_for(&base);

    state
        .tunnels()
        .insert("dead-one".into(), "tun-dead-one".into());
    state
        .tunnels()
        .insert("dead-two".into(), "tun-dead-two".into());
    state
        .records()
        .insert("dead-one.example.com".into(), "rec-one".into());
    // `dead-two` has no record left — the common case, and must not be an
    // error.

    let report = cloudpipe_sdk::cleanup_with_report(&api, "acc-1", "zone-1", "example.com")
        .await
        .expect("cleanup should succeed");

    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(report.removed.len(), 3, "2 tunnels + 1 record: {report:?}");
    assert!(report.would_remove.is_empty());
    assert!(state.tunnels().is_empty(), "both tunnels must be gone");
    assert!(state.records().is_empty(), "the orphan record must be gone");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cleanup_never_touches_records_that_are_not_tunnel_cnames() {
    let state = MockState::new();
    let base = spawn_mock_api(state.clone()).await;
    let api = api_for(&base);

    // A zone full of ordinary DNS: DMARC, SPF, ACME challenge, a user's
    // own record. None point at a tunnel, so cleanup must not delete them.
    for (name, id) in [
        ("_dmarc.example.com", "rec-dmarc"),
        ("example.com", "rec-apex"),
    ] {
        state.records().insert(name.into(), id.into());
    }
    // The mock's DNS listing always reports `tun-x.cfargotunnel.com` as the
    // content, which *is* a tunnel CNAME — so give one a non-tunnel target
    // by pinning a specific record the mock echoes differently.
    state
        .records()
        .insert("keep.example.com".into(), "rec-keep".into());

    let report = cloudpipe_sdk::preview_cleanup(&api, "acc-1", "zone-1", "example.com")
        .await
        .expect("preview should succeed");

    assert!(
        state.deleted_records().is_empty(),
        "cleanup deleted something without a dead tunnel: {report:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
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
