//! Async client for the Cloudflare API v4.
//!
//! Talks directly to `https://api.cloudflare.com/client/v4` with the user's
//! own API token. Every method returns a typed error ([`crate::ApiError`]) on
//! failure so callers can `match` on it.

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use crate::error::{classify_error_code, ApiError, CloudflareErrorKind, Result};

/// Cloudflare API base URL.
pub const CF_API_BASE: &str = "https://api.cloudflare.com/client/v4";

#[derive(Debug, Deserialize)]
struct CfResponse<T> {
    success: bool,
    result: Option<T>,
    errors: Option<Vec<CfError>>,
}

#[derive(Debug, Deserialize)]
struct CfError {
    code: Option<u64>,
    message: String,
}

/// A DNS zone (domain).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Zone {
    /// Zone identifier.
    pub id: String,
    /// Zone name (e.g. `example.com`).
    pub name: String,
    /// Owning account (always populated for accessible zones).
    pub account: ZoneAccount,
}

/// Account reference attached to a [`Zone`].
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ZoneAccount {
    /// Account identifier.
    pub id: String,
}

/// A named Cloudflare Tunnel.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Tunnel {
    /// Tunnel identifier.
    pub id: String,
    /// Tunnel name (matches the subdomain on the wire).
    pub name: String,
    /// Reported status (`healthy`, `down`, `inactive`, `degraded`, …).
    pub status: String,
    /// Connection secret used to run `cloudflared tunnel run`.
    pub token: Option<String>,
    /// RFC3339 timestamp from Cloudflare (may be absent for legacy tunnels).
    #[serde(default)]
    pub created_at: Option<String>,
}

/// A DNS record.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DnsRecord {
    /// Record identifier.
    pub id: String,
    /// Full hostname.
    pub name: String,
    /// Record value (CNAME target for tunnels).
    pub content: String,
}

/// One ingress rule in a tunnel's remote configuration.
///
/// `hostname` is the public hostname (`myapp.example.com`); `None` is the
/// mandatory catch-all. `service` is the local target (`http://...`, etc.)
/// or `http_status:404`.
#[derive(Debug, Clone, Serialize)]
pub struct IngressEntry {
    /// Public hostname, or `None` for the catch-all entry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    /// Local service URL or `http_status:404`.
    pub service: String,
}

/// Result of `GET /user/tokens/verify`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TokenStatus {
    /// Token identifier.
    #[allow(dead_code)]
    pub id: String,
    /// Status string (`active`, …).
    pub status: String,
}

/// Async client for the Cloudflare API authenticated with a user token.
#[derive(Debug, Clone)]
pub struct CloudflareApi {
    client: reqwest::Client,
    token: String,
    base: String,
}

impl CloudflareApi {
    /// Builds a client for the given API token against the production API.
    pub fn new(token: impl Into<String>) -> Result<Self> {
        Self::with_base(token, CF_API_BASE)
    }

    /// Builds a client pointing at a custom base URL (for tests / proxies).
    pub fn with_base(token: impl Into<String>, base: impl Into<String>) -> Result<Self> {
        let user_agent = format!("{}/{}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
        let client = reqwest::Client::builder()
            .user_agent(user_agent)
            .build()
            .map_err(ApiError::Http)?;
        Ok(Self {
            client,
            token: token.into(),
            base: base.into(),
        })
    }

    /// Returns a reference to the inner `reqwest::Client` for advanced uses.
    #[allow(dead_code)]
    pub fn http(&self) -> &reqwest::Client {
        &self.client
    }

    /// Verifies the token (`GET /user/tokens/verify`).
    pub async fn verify_token(&self) -> Result<TokenStatus> {
        self.request(reqwest::Method::GET, "/user/tokens/verify", None)
            .await
    }

    /// Lists all DNS zones the token can access.
    pub async fn list_zones(&self) -> Result<Vec<Zone>> {
        self.request(reqwest::Method::GET, "/zones?per_page=50", None)
            .await
    }

    /// Finds a zone by exact domain name.
    pub async fn find_zone(&self, domain: &str) -> Result<Option<Zone>> {
        Ok(self
            .list_zones()
            .await?
            .into_iter()
            .find(|z| z.name == domain))
    }

    /// Creates a named tunnel (`POST /accounts/{id}/tunnels`).
    pub async fn create_tunnel(&self, account_id: &str, name: &str) -> Result<Tunnel> {
        let body = serde_json::json!({ "name": name, "config_src": "cloudflare" });
        self.request(
            reqwest::Method::POST,
            &format!("/accounts/{account_id}/tunnels"),
            Some(&body),
        )
        .await
    }

    /// Finds non-deleted tunnels by name.
    pub async fn find_tunnel_by_name(&self, account_id: &str, name: &str) -> Result<Vec<Tunnel>> {
        self.request(
            reqwest::Method::GET,
            &format!("/accounts/{account_id}/tunnels?name={name}&is_deleted=false"),
            None,
        )
        .await
    }

    /// Lists tunnels by status (e.g. `down`, `inactive`, `degraded`, `healthy`).
    pub async fn list_tunnels(&self, account_id: &str, status: &str) -> Result<Vec<Tunnel>> {
        self.request(
            reqwest::Method::GET,
            &format!("/accounts/{account_id}/tunnels?is_deleted=false&status={status}"),
            None,
        )
        .await
    }

    /// Cleans up all active connections of a tunnel (required before deletion).
    ///
    /// Failure here is non-fatal — connections may already be gone — so the
    /// error is swallowed.
    pub async fn cleanup_connections(&self, account_id: &str, tunnel_id: &str) -> Result<()> {
        let result: Result<serde_json::Value> = self
            .request(
                reqwest::Method::DELETE,
                &format!("/accounts/{account_id}/tunnels/{tunnel_id}/connections"),
                None,
            )
            .await;
        let _ = result;
        Ok(())
    }

    /// Deletes a tunnel by ID.
    pub async fn delete_tunnel(&self, account_id: &str, tunnel_id: &str) -> Result<()> {
        self.request::<serde_json::Value>(
            reqwest::Method::DELETE,
            &format!("/accounts/{account_id}/tunnels/{tunnel_id}"),
            None,
        )
        .await?;
        Ok(())
    }

    /// Finds a DNS record by exact full name (e.g. `foo.example.com`).
    pub async fn find_dns_record(
        &self,
        zone_id: &str,
        full_name: &str,
    ) -> Result<Option<DnsRecord>> {
        let records: Vec<DnsRecord> = self
            .request(
                reqwest::Method::GET,
                &format!("/zones/{zone_id}/dns_records?name={full_name}"),
                None,
            )
            .await?;
        Ok(records.into_iter().find(|r| r.name == full_name))
    }

    /// Creates a proxied CNAME record pointing at a tunnel's `cfargotunnel.com`
    /// target.
    pub async fn create_dns_record(
        &self,
        zone_id: &str,
        full_name: &str,
        target: &str,
    ) -> Result<DnsRecord> {
        let body = serde_json::json!({
            "type": "CNAME",
            "name": full_name,
            "content": target,
            "proxied": true,
            "ttl": 1,
        });
        self.request(
            reqwest::Method::POST,
            &format!("/zones/{zone_id}/dns_records"),
            Some(&body),
        )
        .await
    }

    /// Deletes a DNS record by record ID.
    pub async fn delete_dns_record(&self, zone_id: &str, record_id: &str) -> Result<()> {
        self.request::<serde_json::Value>(
            reqwest::Method::DELETE,
            &format!("/zones/{zone_id}/dns_records/{record_id}"),
            None,
        )
        .await?;
        Ok(())
    }

    /// Lists DNS records of a given type (e.g. `CNAME`).
    pub async fn list_dns_records(
        &self,
        zone_id: &str,
        record_type: &str,
    ) -> Result<Vec<DnsRecord>> {
        self.request(
            reqwest::Method::GET,
            &format!("/zones/{zone_id}/dns_records?type={record_type}&per_page=100"),
            None,
        )
        .await
    }

    /// Whether a tunnel with the given ID still exists.
    pub async fn tunnel_exists(&self, account_id: &str, tunnel_id: &str) -> bool {
        let result: Result<Tunnel> = self
            .request(
                reqwest::Method::GET,
                &format!("/accounts/{account_id}/tunnels/{tunnel_id}"),
                None,
            )
            .await;
        result.is_ok()
    }

    /// Replaces a tunnel's remote ingress configuration.
    pub async fn set_tunnel_ingress(
        &self,
        account_id: &str,
        tunnel_id: &str,
        ingress: &[IngressEntry],
    ) -> Result<()> {
        let body = serde_json::json!({ "config": { "ingress": ingress } });
        self.request::<serde_json::Value>(
            reqwest::Method::PUT,
            &format!("/accounts/{account_id}/cfd_tunnel/{tunnel_id}/configurations"),
            Some(&body),
        )
        .await?;
        Ok(())
    }

    async fn request<T>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<T>
    where
        T: for<'de> Deserialize<'de> + Default,
    {
        let url = format!("{}{}", self.base, path);
        let mut req = self
            .client
            .request(method, &url)
            .bearer_auth(&self.token)
            .header("Content-Type", "application/json");

        if let Some(body) = body {
            req = req.json(body);
        }

        let response = req.send().await.map_err(ApiError::Http)?;
        let status = response.status();
        let text = response.text().await.map_err(ApiError::Http)?;

        let envelope: CfResponse<T> = serde_json::from_str(&text).map_err(|_| {
            let truncated = truncate(&text, 512);
            ApiError::NonJson {
                status: status.as_u16(),
                body: truncated,
            }
        })?;

        if !status.is_success() || !envelope.success {
            let detail: ApiError = envelope
                .errors
                .as_ref()
                .and_then(|errors| errors.first())
                .map(|e| friendly(e.code, &e.message))
                .unwrap_or_else(|| {
                    let mut msg = format!("HTTP {status}");
                    if !text.is_empty() {
                        let _ = write!(msg, ": {}", truncate(&text, 256));
                    }
                    ApiError::Api {
                        code: None,
                        message: msg,
                        kind: CloudflareErrorKind::Other(0),
                    }
                });
            return Err(detail.into());
        }

        // Cloudflare's DELETE responses carry `result: null`; treat that as
        // a successful default-constructed payload.
        Ok(envelope.result.unwrap_or_default())
    }
}

fn friendly(code: Option<u64>, message: &str) -> ApiError {
    let kind = code
        .map(classify_error_code)
        .unwrap_or(CloudflareErrorKind::Other(0));
    ApiError::Api {
        code,
        message: message.to_string(),
        kind,
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &s[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Tiny in-process HTTP server for client tests. The handler is shared
    /// behind an `Arc<Mutex<…>>` so each spawned task can call it.
    async fn mock_server(
        handler: Arc<Mutex<impl Fn(&str, &str) -> (u16, String) + Send + 'static>>,
    ) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handler = Arc::clone(&handler);
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let handler = Arc::clone(&handler);
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&buf[..n]).to_string();
                    let method = request.split_whitespace().next().unwrap_or("");
                    let (status, body) = (handler.lock().unwrap())(method, &request);
                    let response = format!(
                        "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                });
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn get_request_parses_success_envelope() {
        let handler = Arc::new(Mutex::new(|method: &str, request: &str| -> (u16, String) {
            assert_eq!(method, "GET");
            assert!(
                request.contains("Bearer test-token"),
                "missing auth: {request}"
            );
            assert!(
                request.contains("/user/tokens/verify"),
                "bad path: {request}"
            );
            (
                200,
                r#"{"success":true,"result":{"id":"tok","status":"active"}}"#.into(),
            )
        }));
        let base = mock_server(handler).await;
        let api = CloudflareApi::with_base("test-token", base).unwrap();
        let token = api.verify_token().await.unwrap();
        assert_eq!(token.status, "active");
    }

    #[tokio::test]
    async fn api_error_becomes_classified() {
        let handler = Arc::new(Mutex::new(|_m: &str, _r: &str| -> (u16, String) {
            (
                400,
                r#"{"success":false,"result":null,"errors":[{"code":10429,"message":"Rate limited."}]}"#
                    .into(),
            )
        }));
        let base = mock_server(handler).await;
        let api = CloudflareApi::with_base("test-token", base).unwrap();
        let err = api.list_zones().await.unwrap_err();
        match err {
            crate::Error::Cloudflare(ApiError::Api { kind, .. }) => {
                assert_eq!(kind, CloudflareErrorKind::RateLimited);
            }
            other => panic!("wrong error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn non_json_response_is_rejected() {
        let handler = Arc::new(Mutex::new(|_m: &str, _r: &str| -> (u16, String) {
            (502, "gateway error".into())
        }));
        let base = mock_server(handler).await;
        let api = CloudflareApi::with_base("test-token", base).unwrap();
        match api.list_zones().await.unwrap_err() {
            crate::Error::Cloudflare(ApiError::NonJson { status, body }) => {
                assert_eq!(status, 502);
                assert!(body.contains("gateway"));
            }
            other => panic!("wrong error: {other:?}"),
        }
    }
}
