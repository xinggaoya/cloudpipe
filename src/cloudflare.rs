//! Minimal Cloudflare API v4 client.
//!
//! Talks directly to `https://api.cloudflare.com/client/v4` using the user's
//! own API token. No backend server is involved — just a thin typed wrapper
//! around the HTTP API.

use anyhow::{anyhow, bail, Result};
use reqwest::blocking::Client;
use serde::Deserialize;

/// Cloudflare API base URL.
pub const CF_API_BASE: &str = "https://api.cloudflare.com/client/v4";

/// Standard Cloudflare API response envelope.
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
#[derive(Debug, Clone, Deserialize)]
pub struct Zone {
    pub id: String,
    pub name: String,
    pub account: ZoneAccount,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ZoneAccount {
    pub id: String,
}

/// A named Cloudflare Tunnel.
#[derive(Debug, Clone, Deserialize)]
pub struct Tunnel {
    pub id: String,
    pub name: String,
    pub status: String,
    pub token: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub created_at: Option<String>,
}

/// A DNS record.
#[derive(Debug, Clone, Deserialize)]
pub struct DnsRecord {
    pub id: String,
    pub name: String,
    pub content: String,
}

/// Result of `GET /user/tokens/verify`.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct TokenStatus {
    pub id: String,
    pub status: String,
}

/// Converts Cloudflare error codes into actionable messages.
///
/// Pure function so it can be unit-tested without network access.
pub fn friendly_error(code: Option<u64>, message: &str) -> String {
    match code {
        Some(10429) => format!(
            "Cloudflare API rate limit hit ({message}). \
             Wait a minute or two and retry — the quota resets automatically."
        ),
        Some(81053) => format!("DNS record already exists ({message})."),
        Some(10001) | Some(10000) | Some(9109) => format!(
            "Cloudflare authentication failed ({message}). \
             Check that the token is valid and has the required permissions \
             (see README for the exact permissions needed)."
        ),
        Some(1038) => format!("Invalid API token ({message})."),
        Some(code) => format!("Cloudflare API error [{code}]: {message}"),
        None => format!("Cloudflare API error: {message}"),
    }
}

/// Client for the Cloudflare API authenticated with a user token.
pub struct CloudflareApi {
    client: Client,
    token: String,
    base: String,
}

impl CloudflareApi {
    /// Creates a client for the given API token.
    pub fn new(token: String) -> Result<Self> {
        let user_agent = format!(
            "{}/{}",
            env!("CARGO_PKG_NAME"),
            env!("CARGO_PKG_VERSION")
        );
        Ok(Self {
            client: Client::builder().user_agent(user_agent).build()?,
            token,
            base: CF_API_BASE.to_string(),
        })
    }

    fn request<T: for<'de> Deserialize<'de>>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<T> {
        let url = format!("{}{}", self.base, path);
        let mut req = self
            .client
            .request(method, &url)
            .bearer_auth(&self.token)
            .header("Content-Type", "application/json");

        if let Some(body) = body {
            req = req.json(body);
        }

        let response = req
            .send()
            .map_err(|e| anyhow!("request to {url} failed: {e}"))?;

        let status = response.status();
        let text = response
            .text()
            .map_err(|e| anyhow!("reading response from {url} failed: {e}"))?;

        let envelope: CfResponse<T> = serde_json::from_str(&text)
            .map_err(|_| anyhow!("non-JSON response (HTTP {status}): {text}"))?;

        if !status.is_success() || !envelope.success {
            let detail = envelope
                .errors
                .as_ref()
                .and_then(|errors| errors.first())
                .map(|e| friendly_error(e.code, &e.message))
                .unwrap_or_else(|| format!("HTTP {status}: {text}"));
            bail!(detail);
        }

        envelope
            .result
            .ok_or_else(|| anyhow!("API response missing 'result' field"))
    }

    /// Verifies the token is valid (`GET /user/tokens/verify`).
    pub fn verify_token(&self) -> Result<TokenStatus> {
        self.request(reqwest::Method::GET, "/user/tokens/verify", None)
    }

    /// Lists all DNS zones the token can access.
    pub fn list_zones(&self) -> Result<Vec<Zone>> {
        self.request(reqwest::Method::GET, "/zones?per_page=50", None)
    }

    /// Finds a zone by exact domain name.
    pub fn find_zone(&self, domain: &str) -> Result<Option<Zone>> {
        Ok(self
            .list_zones()?
            .into_iter()
            .find(|zone| zone.name == domain))
    }

    /// Creates a named tunnel (`POST /accounts/{id}/tunnels`).
    pub fn create_tunnel(&self, account_id: &str, name: &str) -> Result<Tunnel> {
        let body = serde_json::json!({ "name": name, "config_src": "cloudflare" });
        self.request(
            reqwest::Method::POST,
            &format!("/accounts/{account_id}/tunnels"),
            Some(&body),
        )
    }

    /// Finds non-deleted tunnels by name.
    pub fn find_tunnel_by_name(&self, account_id: &str, name: &str) -> Result<Vec<Tunnel>> {
        self.request(
            reqwest::Method::GET,
            &format!(
                "/accounts/{account_id}/tunnels?name={name}&is_deleted=false"
            ),
            None,
        )
    }

    /// Lists tunnels by status (e.g. `down`, `inactive`, `degraded`, `healthy`).
    pub fn list_tunnels(&self, account_id: &str, status: &str) -> Result<Vec<Tunnel>> {
        self.request(
            reqwest::Method::GET,
            &format!(
                "/accounts/{account_id}/tunnels?is_deleted=false&status={status}"
            ),
            None,
        )
    }

    /// Cleans up all active connections of a tunnel (required before deletion).
    pub fn cleanup_connections(&self, account_id: &str, tunnel_id: &str) -> Result<()> {
        // Connections might already be gone; a failure here is not fatal for
        // the caller, so this returns Ok(()) on any error.
        let _: Result<serde_json::Value> = self.request(
            reqwest::Method::DELETE,
            &format!("/accounts/{account_id}/tunnels/{tunnel_id}/connections"),
            None,
        );
        Ok(())
    }

    /// Deletes a tunnel by ID.
    pub fn delete_tunnel(&self, account_id: &str, tunnel_id: &str) -> Result<()> {
        self.request::<serde_json::Value>(
            reqwest::Method::DELETE,
            &format!("/accounts/{account_id}/tunnels/{tunnel_id}"),
            None,
        )?;
        Ok(())
    }

    /// Finds a DNS record by exact full name (e.g. `foo.example.com`).
    pub fn find_dns_record(&self, zone_id: &str, full_name: &str) -> Result<Option<DnsRecord>> {
        let records: Vec<DnsRecord> = self.request(
            reqwest::Method::GET,
            &format!("/zones/{zone_id}/dns_records?name={full_name}"),
            None,
        )?;
        Ok(records.into_iter().find(|r| r.name == full_name))
    }

    /// Creates a proxied CNAME record pointing at a tunnel's `cfargotunnel.com` target.
    pub fn create_dns_record(
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
    }

    /// Deletes a DNS record by record ID.
    pub fn delete_dns_record(&self, zone_id: &str, record_id: &str) -> Result<()> {
        self.request::<serde_json::Value>(
            reqwest::Method::DELETE,
            &format!("/zones/{zone_id}/dns_records/{record_id}"),
            None,
        )?;
        Ok(())
    }

    /// Lists DNS records of a given type (e.g. `CNAME`).
    pub fn list_dns_records(&self, zone_id: &str, record_type: &str) -> Result<Vec<DnsRecord>> {
        self.request(
            reqwest::Method::GET,
            &format!("/zones/{zone_id}/dns_records?type={record_type}&per_page=100"),
            None,
        )
    }

    /// Whether a tunnel with the given ID still exists.
    pub fn tunnel_exists(&self, account_id: &str, tunnel_id: &str) -> bool {
        let result: Result<Tunnel> = self.request(
            reqwest::Method::GET,
            &format!("/accounts/{account_id}/tunnels/{tunnel_id}"),
            None,
        );
        result.is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn friendly_error_rate_limit() {
        let msg = friendly_error(Some(10429), "Rate limited.");
        assert!(msg.contains("rate limit"), "got: {msg}");
    }

    #[test]
    fn friendly_error_auth() {
        let msg = friendly_error(Some(10001), "Unable to authenticate request");
        assert!(msg.contains("authentication failed"), "got: {msg}");
        let msg = friendly_error(Some(9109), "bad token");
        assert!(msg.contains("authentication failed"), "got: {msg}");
    }

    #[test]
    fn friendly_error_unknown_code() {
        let msg = friendly_error(Some(1234), "boom");
        assert!(msg.contains("[1234]"), "got: {msg}");
        let msg = friendly_error(None, "boom");
        assert!(msg.contains("boom"), "got: {msg}");
    }

    /// Tiny in-process HTTP server for client tests.
    fn mock_server(handler: impl Fn(&str, &str) -> (u16, String) + Send + 'static) -> String {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]).to_string();
                let method = request.split_whitespace().next().unwrap_or("");
                let (status, body) = handler(method, &request);
                let response = format!(
                    "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        format!("http://{addr}")
    }

    fn client_for(base: &str) -> CloudflareApi {
        let mut api = CloudflareApi::new("test-token".into()).unwrap();
        api.base = base.to_string();
        api
    }

    #[test]
    fn get_request_parses_success_envelope() {
        let base = mock_server(|method, request| {
            assert_eq!(method, "GET");
            assert!(request.contains("Bearer test-token"), "missing auth");
            assert!(request.contains("/user/tokens/verify"), "bad path");
            (200, r#"{"success":true,"result":{"id":"tok","status":"active"}}"#.into())
        });
        let api = client_for(&base);
        let token = api.verify_token().unwrap();
        assert_eq!(token.status, "active");
    }

    #[test]
    fn api_error_becomes_friendly_message() {
        let base = mock_server(|_method, _request| {
            (
                400,
                r#"{"success":false,"result":null,"errors":[{"code":10429,"message":"Rate limited."}]}"#
                    .into(),
            )
        });
        let api = client_for(&base);
        let err = api.list_zones().unwrap_err();
        assert!(err.to_string().contains("rate limit"), "got: {err}");
    }

    #[test]
    fn non_json_response_is_rejected() {
        let base = mock_server(|_method, _request| (502, "gateway error".into()));
        let api = client_for(&base);
        assert!(api.list_zones().is_err());
    }
}
