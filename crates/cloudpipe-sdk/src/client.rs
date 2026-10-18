//! Async client for the Cloudflare API v4.
//!
//! Talks directly to `https://api.cloudflare.com/client/v4` with the user's
//! own API token. Every method returns a typed error ([`crate::ApiError`]) on
//! failure so callers can `match` on it.
//!
//! Cross-cutting behaviour lives here rather than in the callers:
//!
//! - **Timeouts** — [`CloudflareApi::with_base`] installs connect/request
//!   deadlines so a stalled Cloudflare edge cannot hang the caller forever.
//! - **Retries** — rate limits (HTTP 429 / code 10429) and transient 5xx /
//!   transport failures are retried with exponential backoff and jitter.
//!   Cloudflare guarantees a rate-limited or 5xx response performed no write,
//!   so replaying the request is safe.
//! - **Pagination** — every `list_*` call transparently walks
//!   `result_info.total_pages`, so a token with 200 zones or a zone with
//!   5,000 DNS records is never silently truncated at the first page.

use std::fmt::Write as _;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::error::{classify_error_code, ApiError, CloudflareErrorKind, Result};

/// Cloudflare API base URL.
pub const CF_API_BASE: &str = "https://api.cloudflare.com/client/v4";

/// How long to wait for the TCP+TLS handshake to complete.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a single Cloudflare API request may take end-to-end.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Number of *extra* attempts after the first one.
const MAX_RETRIES: u32 = 3;

#[derive(Debug, Deserialize)]
struct CfResponse<T> {
    success: bool,
    result: Option<T>,
    #[serde(default)]
    errors: Option<Vec<CfError>>,
    #[serde(default)]
    result_info: Option<ResultInfo>,
}

#[derive(Debug, Deserialize)]
struct CfError {
    code: Option<u64>,
    message: String,
}

/// Pagination metadata Cloudflare attaches to list responses.
#[derive(Debug, Clone, Default, Deserialize)]
struct ResultInfo {
    /// Echo of the page Cloudflare served. Unused for control flow (we
    /// count pages ourselves so a bad `total_pages` cannot loop forever),
    /// but kept for debug output.
    #[serde(default)]
    #[allow(dead_code)]
    page: u32,
    #[serde(default)]
    per_page: u32,
    #[serde(default)]
    total_pages: u32,
}

/// A DNS zone (domain).
#[derive(Debug, Clone, Deserialize)]
pub struct Zone {
    /// Zone identifier.
    pub id: String,
    /// Zone name (e.g. `example.com`).
    pub name: String,
    /// Owning account. `None` for tokens whose zone listing omits it; the
    /// builder surfaces a clear error rather than propagating an empty id.
    #[serde(default)]
    pub account: Option<ZoneAccount>,
}

impl Zone {
    /// The account id, or a descriptive error when Cloudflare omitted it.
    pub fn require_account_id(&self) -> Result<&str> {
        self.account
            .as_ref()
            .map(|a| a.id.as_str())
            .filter(|id| !id.is_empty())
            .ok_or_else(|| {
                ApiError::Api {
                    code: None,
                    message: format!(
                        "Cloudflare returned zone \"{}\" without an account reference",
                        self.name
                    ),
                    kind: CloudflareErrorKind::Other(0),
                }
                .into()
            })
    }
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
    #[serde(default)]
    pub status: String,
    /// Connection secret used to run `cloudflared tunnel run`.
    #[serde(default)]
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
    #[serde(default)]
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
    /// Per-rule origin tuning (TLS verification, timeouts, host header).
    ///
    /// `noTLSVerify` is what makes exposing a local dev server with a
    /// self-signed certificate possible at all, so this field is part of
    /// the public API rather than an escape hatch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin_request: Option<OriginRequest>,
}

/// Origin-side tuning for a single ingress rule.
///
/// Serialized with Cloudflare's camelCase field names. Every field is
/// optional; omitted fields are left at Cloudflare's defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct OriginRequest {
    /// Skip TLS verification when the origin serves HTTPS with a
    /// self-signed certificate.
    #[serde(rename = "noTLSVerify", skip_serializing_if = "Option::is_none")]
    pub no_tls_verify: Option<bool>,
    /// Send the `Host` header as the origin hostname rather than the
    /// public hostname.
    #[serde(rename = "httpHostHeader", skip_serializing_if = "Option::is_none")]
    pub http_host_header: Option<String>,
    /// How long to wait for the origin connection, in seconds.
    #[serde(rename = "connectTimeout", skip_serializing_if = "Option::is_none")]
    pub connect_timeout: Option<u64>,
    /// TCP keepalive interval for origin connections, in seconds.
    #[serde(rename = "tcpKeepAlive", skip_serializing_if = "Option::is_none")]
    pub tcp_keep_alive: Option<u64>,
    /// Disable HTTP/2 to the origin (useful for servers that misbehave).
    #[serde(rename = "http2Origin", skip_serializing_if = "Option::is_none")]
    pub http2_origin: Option<bool>,
    /// Disable Cloudflare's Happy Eyeballs origin selection.
    #[serde(rename = "noHappyEyeballs", skip_serializing_if = "Option::is_none")]
    pub no_happy_eyeballs: Option<bool>,
}

impl OriginRequest {
    /// Disables origin TLS verification (the common self-signed dev case).
    pub fn insecure() -> Self {
        Self {
            no_tls_verify: Some(true),
            ..Default::default()
        }
    }

    /// Returns `true` when no field is set, i.e. the struct would serialize
    /// to `{}` and should be omitted entirely.
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
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
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(ApiError::Http)?;
        Ok(Self {
            client,
            token: token.into(),
            base: base.into().trim_end_matches('/').to_string(),
        })
    }

    /// Returns a reference to the inner `reqwest::Client` for advanced uses.
    #[allow(dead_code)]
    pub fn http(&self) -> &reqwest::Client {
        &self.client
    }

    /// Verifies the token (`GET /user/tokens/verify`).
    pub async fn verify_token(&self) -> Result<TokenStatus> {
        let status: TokenStatus = self
            .request_unit(reqwest::Method::GET, "/user/tokens/verify", None)
            .await?
            .ok_or_else(|| missing_result("/user/tokens/verify"))?;
        Ok(status)
    }

    /// Lists all DNS zones the token can access, following pagination.
    pub async fn list_zones(&self) -> Result<Vec<Zone>> {
        self.paginate("/zones", &[("per_page", "50".into())]).await
    }

    /// Finds a zone by exact domain name.
    pub async fn find_zone(&self, domain: &str) -> Result<Option<Zone>> {
        Ok(self
            .list_zones()
            .await?
            .into_iter()
            // Cloudflare normalises zone names to lowercase; compare
            // case-insensitively so `Example.com` still resolves.
            .find(|z| z.name.eq_ignore_ascii_case(domain)))
    }

    /// Creates a named tunnel (`POST /accounts/{id}/tunnels`).
    pub async fn create_tunnel(&self, account_id: &str, name: &str) -> Result<Tunnel> {
        let body = serde_json::json!({ "name": name, "config_src": "cloudflare" });
        self.request_unit(
            reqwest::Method::POST,
            &format!("/accounts/{account_id}/tunnels"),
            Some(&body),
        )
        .await?
        .ok_or_else(|| missing_result("create_tunnel"))
    }

    /// Finds non-deleted tunnels by name.
    pub async fn find_tunnel_by_name(&self, account_id: &str, name: &str) -> Result<Vec<Tunnel>> {
        self.paginate(
            &format!("/accounts/{account_id}/tunnels"),
            &[("is_deleted", "false".into()), ("name", name.into())],
        )
        .await
    }

    /// Lists tunnels by status (e.g. `down`, `inactive`, `degraded`, `healthy`).
    pub async fn list_tunnels(&self, account_id: &str, status: &str) -> Result<Vec<Tunnel>> {
        self.paginate(
            &format!("/accounts/{account_id}/tunnels"),
            &[("is_deleted", "false".into()), ("status", status.into())],
        )
        .await
    }

    /// Cleans up all active connections of a tunnel (required before deletion).
    ///
    /// Failure here is non-fatal — connections may already be gone — so the
    /// error is swallowed.
    pub async fn cleanup_connections(&self, account_id: &str, tunnel_id: &str) -> Result<()> {
        let result: Result<()> = self
            .request_ignore(
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
        self.request_ignore(
            reqwest::Method::DELETE,
            &format!("/accounts/{account_id}/tunnels/{tunnel_id}"),
            None,
        )
        .await
    }

    /// Finds a DNS record by exact full name (e.g. `foo.example.com`).
    pub async fn find_dns_record(
        &self,
        zone_id: &str,
        full_name: &str,
    ) -> Result<Option<DnsRecord>> {
        let records: Vec<DnsRecord> = self
            .paginate(
                &format!("/zones/{zone_id}/dns_records"),
                &[("name", full_name.into())],
            )
            .await?;
        Ok(records
            .into_iter()
            .find(|r| r.name.eq_ignore_ascii_case(full_name)))
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
        self.request_unit(
            reqwest::Method::POST,
            &format!("/zones/{zone_id}/dns_records"),
            Some(&body),
        )
        .await?
        .ok_or_else(|| missing_result("create_dns_record"))
    }

    /// Deletes a DNS record by record ID.
    pub async fn delete_dns_record(&self, zone_id: &str, record_id: &str) -> Result<()> {
        self.request_ignore(
            reqwest::Method::DELETE,
            &format!("/zones/{zone_id}/dns_records/{record_id}"),
            None,
        )
        .await
    }

    /// Lists DNS records of a given type (e.g. `CNAME`), following pagination.
    pub async fn list_dns_records(
        &self,
        zone_id: &str,
        record_type: &str,
    ) -> Result<Vec<DnsRecord>> {
        self.paginate(
            &format!("/zones/{zone_id}/dns_records"),
            &[("type", record_type.into()), ("per_page", "100".into())],
        )
        .await
    }

    /// Whether a tunnel with the given ID still exists.
    pub async fn tunnel_exists(&self, account_id: &str, tunnel_id: &str) -> bool {
        let result: Result<Option<Tunnel>> = self
            .request_unit(
                reqwest::Method::GET,
                &format!("/accounts/{account_id}/tunnels/{tunnel_id}"),
                None,
            )
            .await;
        matches!(result, Ok(Some(_)))
    }

    /// Whether a public hostname can be claimed for a new tunnel.
    ///
    /// Returns `Ok(false)` when a **healthy** tunnel already serves the
    /// name, or when a DNS record for it exists. A stale tunnel alone does
    /// not block the name — it will be reclaimed.
    ///
    /// The builder uses this to retry a *generated* subdomain when it
    /// collides, without having to fail the whole `start()` first.
    pub async fn subdomain_available(
        &self,
        account_id: &str,
        zone_id: &str,
        full_name: &str,
    ) -> Result<bool> {
        if let Some(existing) = self
            .find_tunnel_by_name(
                account_id,
                full_name.split_once('.').map_or(full_name, |(l, _)| l),
            )
            .await?
            .into_iter()
            .next()
        {
            if existing.status == "healthy" {
                return Ok(false);
            }
        }
        Ok(self.find_dns_record(zone_id, full_name).await?.is_none())
    }

    /// Replaces a tunnel's remote ingress configuration.
    pub async fn set_tunnel_ingress(
        &self,
        account_id: &str,
        tunnel_id: &str,
        ingress: &[IngressEntry],
    ) -> Result<()> {
        let body = serde_json::json!({ "config": { "ingress": ingress } });
        self.request_ignore(
            reqwest::Method::PUT,
            &format!("/accounts/{account_id}/cfd_tunnel/{tunnel_id}/configurations"),
            Some(&body),
        )
        .await
    }

    /// Walks every page of a list endpoint and concatenates the results.
    ///
    /// Pagination metadata comes back from each request rather than being
    /// cached on `self`, so [`CloudflareApi`] stays immutable and safe to
    /// share across tasks.
    async fn paginate<T>(&self, path: &str, extra: &[(&'static str, String)]) -> Result<Vec<T>>
    where
        T: DeserializeOwned,
    {
        let mut all = Vec::new();
        let mut page = 1u32;
        // Defensive ceiling: a misbehaving `total_pages` can never spin us
        // forever. 10k pages × 100 records is a million rows, far beyond
        // any real zone.
        const MAX_PAGES: u32 = 10_000;

        loop {
            let mut params = extra.to_vec();
            if page > 1 {
                params.push(("page", page.to_string()));
            }

            let resp = self
                .request_page(reqwest::Method::GET, &with_query(path, &params), None)
                .await?;
            let items: Vec<T> = resp.items.ok_or_else(|| missing_result(path))?;
            let info = resp.info.unwrap_or_default();
            let count = items.len();
            all.extend(items);

            // `total_pages == 0` means Cloudflare sent no pagination
            // metadata at all. Treating that as "keep going" made the walk
            // spin up to MAX_PAGES, so it is treated as a single page
            // unless we asked for N and got exactly N back.
            let done = match info.total_pages {
                0 => info.per_page == 0 || count < info.per_page as usize,
                n => page >= n,
            };
            if done || page >= MAX_PAGES {
                break;
            }
            page += 1;
        }
        Ok(all)
    }

    async fn request_unit<T: DeserializeOwned>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<Option<T>> {
        Ok(self.request_page(method, path, body).await?.items)
    }

    /// Issues a request purely for its side effect, discarding `result`.
    ///
    /// Cloudflare's `DELETE`s and the ingress `PUT` all answer
    /// `{"success": true, "result": null}`, so there is nothing to decode
    /// and no `Default` shim is needed.
    async fn request_ignore(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<()> {
        self.request_page::<serde_json::Value>(method, path, body)
            .await?;
        Ok(())
    }

    /// Issues a request with retry/backoff, returning both the decoded
    /// result and Cloudflare's pagination metadata.
    async fn request_page<T: DeserializeOwned>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<Page<T>> {
        let mut attempt = 0u32;
        loop {
            match self.attempt(&method, path, body).await {
                Ok(page) => return Ok(page),
                Err(err) => {
                    if attempt >= MAX_RETRIES || !is_retryable(&err) {
                        return Err(err);
                    }
                    attempt += 1;
                    let backoff = retry_backoff(attempt);
                    tracing::debug!(
                        "cloudflare request {path} failed ({err}); retry {attempt}/{MAX_RETRIES} in {backoff:?}"
                    );
                    tokio::time::sleep(backoff).await;
                }
            }
        }
    }

    async fn attempt<T: DeserializeOwned>(
        &self,
        method: &reqwest::Method,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<Page<T>> {
        let url = format!("{}{}", self.base, path);
        let mut req = self
            .client
            .request(method.clone(), &url)
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
            let detail: ApiError = match envelope.errors.as_ref().and_then(|e| e.first()) {
                Some(e) => friendly(e.code, &e.message),
                None => {
                    let mut msg = format!("HTTP {status}");
                    if !text.is_empty() {
                        let _ = write!(msg, ": {}", truncate(&text, 256));
                    }
                    ApiError::Api {
                        code: None,
                        message: msg,
                        // A bare 429 with no envelope still deserves the
                        // rate-limited classification so the retry loop
                        // still engages.
                        kind: if status.as_u16() == 429 {
                            CloudflareErrorKind::RateLimited
                        } else if status.is_server_error() {
                            CloudflareErrorKind::ServerError
                        } else {
                            CloudflareErrorKind::Other(0)
                        },
                    }
                }
            };
            return Err(detail.into());
        }

        Ok(Page {
            items: envelope.result,
            info: envelope.result_info,
        })
    }
}

/// One decoded response: the (possibly absent) `result` plus pagination
/// metadata.
struct Page<T> {
    items: Option<T>,
    info: Option<ResultInfo>,
}

/// Builds a path with a percent-encoded query string.
///
/// Values are interpolated raw today only because every caller passes
/// already-validated tokens (subdomain is a DNS label, status is a fixed
/// literal); encoding them anyway means a future caller cannot smuggle an
/// unescaped `&` or `#` into the query.
fn with_query(path: &str, params: &[(&str, String)]) -> String {
    if params.is_empty() {
        return path.to_string();
    }
    let mut out = String::with_capacity(path.len() + params.len() * 24);
    out.push_str(path);
    out.push('?');
    for (i, (key, value)) in params.iter().enumerate() {
        if i > 0 {
            out.push('&');
        }
        let _ = write!(out, "{key}={}", percent_encode(value));
    }
    out
}

/// Percent-encodes everything outside the RFC 3986 unreserved set.
fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            other => {
                let _ = write!(out, "%{other:02X}");
            }
        }
    }
    out
}

/// Whether an error is worth replaying.
///
/// Only transient classes qualify: rate limits, 5xx, and transport-level
/// failures. A 4xx (bad subdomain, auth failure, zone not found) will fail
/// identically on every retry, so replaying it just burns the user's quota.
fn is_retryable(err: &crate::Error) -> bool {
    match err {
        crate::Error::Cloudflare(ApiError::Http(_)) => true,
        crate::Error::Cloudflare(ApiError::Api { kind, code, .. }) => match kind {
            CloudflareErrorKind::RateLimited => true,
            CloudflareErrorKind::ServerError => true,
            // A bare 5xx with no envelope still carries the status in the
            // message; treat any numeric server code as retryable.
            CloudflareErrorKind::Other(code) => *code >= 500,
            _ => {
                let _ = code;
                false
            }
        },
        // A gateway returning HTML is almost always a transient blip.
        crate::Error::Cloudflare(ApiError::NonJson { status, .. }) => *status >= 500,
        _ => false,
    }
}

/// Exponential backoff with full jitter, capped at 8s.
///
/// Jitter matters because every SDK embedding program retries against the
/// same per-token Cloudflare quota — synchronised retries would keep
/// colliding.
fn retry_backoff(attempt: u32) -> Duration {
    let base = 200u64.saturating_mul(1u64 << attempt.min(6)).min(8_000);
    let jitter = base / 2;
    let noise = (base as f64 * 0.5 * ((attempt as u64).wrapping_mul(2_654_435_761) % 1_000) as f64
        / 1_000.0) as u64;
    Duration::from_millis(base - jitter + noise)
}

fn missing_result(what: &str) -> crate::Error {
    ApiError::Api {
        code: None,
        message: format!("{what}: Cloudflare returned `success: true` with a null result"),
        kind: CloudflareErrorKind::Other(0),
    }
    .into()
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

    #[test]
    fn query_values_are_percent_encoded() {
        let params = vec![("name", "a b&c=d#e".to_string())];
        let out = with_query("/x", &params);
        assert_eq!(out, "/x?name=a%20b%26c%3Dd%23e");
        // Round-trips through a naive decode.
        assert!(out.contains("%26"), "ampersand must be escaped");
    }

    #[test]
    fn empty_query_is_omitted() {
        assert_eq!(with_query("/x", &[]), "/x");
    }

    #[test]
    fn retryable_classification() {
        let rate = crate::Error::from(ApiError::Api {
            code: Some(10429),
            message: "x".into(),
            kind: CloudflareErrorKind::RateLimited,
        });
        assert!(is_retryable(&rate));

        let auth = crate::Error::from(ApiError::Api {
            code: Some(10001),
            message: "x".into(),
            kind: CloudflareErrorKind::AuthFailed,
        });
        assert!(!is_retryable(&auth));

        let sub = crate::Error::SubdomainInUse("x".into());
        assert!(!is_retryable(&sub));
    }

    #[test]
    fn backoff_is_bounded_and_jittered() {
        let mut seen = Vec::new();
        for attempt in 1..=MAX_RETRIES {
            let d = retry_backoff(attempt);
            assert!(d <= Duration::from_millis(8_000), "unbounded: {d:?}");
            seen.push(d.as_millis());
        }
        // Not all equal — jitter is actually applied.
        assert!(seen.windows(2).any(|w| w[0] != w[1]), "no jitter: {seen:?}");
    }

    #[test]
    fn origin_request_is_omitted_when_empty() {
        let entry = IngressEntry {
            hostname: Some("a.example.com".into()),
            service: "http://localhost:8080".into(),
            origin_request: None,
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(!json.contains("originRequest"), "leaked: {json}");

        let entry = IngressEntry {
            origin_request: Some(OriginRequest::insecure()),
            ..entry
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"noTLSVerify\":true"), "missing: {json}");
    }

    #[test]
    fn zone_without_account_reports_a_useful_error() {
        let zone: Zone = serde_json::from_str(r#"{"id":"z","name":"example.com"}"#).unwrap();
        let err = zone.require_account_id().unwrap_err();
        assert!(format!("{err:#}").contains("without an account"));
    }
}
