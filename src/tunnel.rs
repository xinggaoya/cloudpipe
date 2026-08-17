//! Tunnel lifecycle orchestration: create → connect → cleanup.
//!
//! Runs entirely inside the CLI — no backend server is involved.

use std::process::Child;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use colored::Colorize;

use crate::cloudflare::{CloudflareApi, IngressEntry};
use crate::cloudflared::{self, LineKind};
use crate::config::{Config, ConfigStore};
use crate::ui;

/// Tunnels are cleaned up automatically after this much time.
pub const TUNNEL_TIMEOUT: Duration = Duration::from_secs(4 * 60 * 60);

/// Set by the Ctrl+C handler; the main loop polls it.
static EXIT_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Asks the main loop to shut down gracefully.
pub fn request_exit() {
    EXIT_REQUESTED.store(true, Ordering::SeqCst);
}

fn exit_requested() -> bool {
    EXIT_REQUESTED.load(Ordering::SeqCst)
}

/// Options for starting a tunnel.
#[derive(Debug, Clone)]
pub struct TunnelOptions {
    /// Local protocol scheme.
    pub protocol: Protocol,
    /// Local port to forward.
    pub port: u16,
    /// Optional subdomain; a random one is generated otherwise.
    pub subdomain: Option<String>,
}

/// Protocol schemes accepted by Cloudflare Tunnel ingress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    Http,
    Https,
    Tcp,
    Udp,
    Ssh,
}

impl Protocol {
    /// Parses the CLI token, accepting the canonical names (`http`, `https`,
    /// `tcp`, `udp`, `ssh`). `http2` and other variants are not supported.
    pub fn parse(value: &str) -> Result<Self> {
        match value.to_ascii_lowercase().as_str() {
            "http" => Ok(Self::Http),
            "https" => Ok(Self::Https),
            "tcp" => Ok(Self::Tcp),
            "udp" => Ok(Self::Udp),
            "ssh" => Ok(Self::Ssh),
            other => bail!(
                "unknown protocol \"{other}\" — supported: http, https, tcp, udp, ssh"
            ),
        }
    }

    /// Canonical lowercase name, suitable for display.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
            Self::Tcp => "tcp",
            Self::Udp => "udp",
            Self::Ssh => "ssh",
        }
    }

    /// The local service URL this protocol produces for Cloudflare ingress,
    /// e.g. `http://localhost:8080`.
    pub fn local_service(self, port: u16) -> String {
        format!("{}://localhost:{port}", self.as_str())
    }
}

/// Generates a random subdomain (`user-1234`).
fn random_subdomain() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let seed = RandomState::new().build_hasher().finish() as u32;
    format!("user-{:04}", seed % 10000)
}

/// Validates a DNS label (subdomain) and returns a friendly error.
pub fn validate_subdomain(subdomain: &str) -> Result<()> {
    if subdomain.len() < 3 || subdomain.len() > 63 {
        bail!("subdomain must be between 3 and 63 characters");
    }
    let valid = subdomain
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !subdomain.starts_with('-')
        && !subdomain.ends_with('-');
    if !valid {
        bail!(
            "invalid subdomain \"{subdomain}\": use lowercase letters, digits and dashes, \
             not starting/ending with a dash"
        );
    }
    Ok(())
}

/// Resolves subdomain conflicts before creating anything:
/// stale tunnels (down/inactive/degraded) and orphaned DNS records are removed.
fn resolve_conflicts(
    api: &CloudflareApi,
    account_id: &str,
    zone_id: &str,
    subdomain: &str,
    full_name: &str,
) -> Result<()> {
    match api.find_tunnel_by_name(account_id, subdomain) {
        Ok(tunnels) => {
            if let Some(existing) = tunnels.into_iter().next() {
                if existing.status == "healthy" {
                    bail!(
                        "subdomain \"{subdomain}\" is currently in use by an active tunnel — \
                         choose another name"
                    );
                }
                println!(
                    "  {}",
                    format!("Reclaiming stale tunnel \"{subdomain}\" ({})", existing.status)
                        .yellow()
                );
                if let Ok(Some(record)) = api.find_dns_record(zone_id, full_name) {
                    let _ = api.delete_dns_record(zone_id, &record.id);
                }
                api.cleanup_connections(account_id, &existing.id)?;
                api.delete_tunnel(account_id, &existing.id)?;
            }
        }
        Err(e) => {
            println!(
                "  {}",
                format!("(could not check for existing tunnels: {e})").yellow()
            );
        }
    }

    if let Ok(Some(record)) = api.find_dns_record(zone_id, full_name) {
        println!(
            "  {}",
            format!("Removing orphaned DNS record for {full_name}").yellow()
        );
        api.delete_dns_record(zone_id, &record.id)?;
    }
    Ok(())
}

/// Active tunnel session. Cleans up Cloudflare resources when dropped, so
/// Ctrl+C, errors and panics all release the tunnel and DNS record.
struct Session {
    api: CloudflareApi,
    account_id: String,
    zone_id: String,
    full_name: String,
    tunnel_id: String,
    child: Option<Child>,
    cleaned: bool,
}

impl Session {
    fn cleanup(&mut self) {
        if self.cleaned {
            return;
        }
        self.cleaned = true;

        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }

        if let Ok(Some(record)) = self.api.find_dns_record(&self.zone_id, &self.full_name) {
            let _ = self.api.delete_dns_record(&self.zone_id, &record.id);
        }
        let _ = self.api.cleanup_connections(&self.account_id, &self.tunnel_id);
        let _ = self.api.delete_tunnel(&self.account_id, &self.tunnel_id);
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.cleanup();
    }
}

/// Loads the config and checks that all required credentials are present.
fn load_ready_config(store: &ConfigStore) -> Result<(Config, String)> {
    let config = store.load();

    let token = match config.effective_token() {
        Some(token) => token,
        None => {
            ui::no_token_guide();
            bail!("no Cloudflare API token configured");
        }
    };

    if config.account_id.is_none()
        || config.zone_id.is_none()
        || config.domain.is_none()
    {
        bail!(
            "account/zone not configured — run `cfp key <token>` (or `cfp domain <name>`) first"
        );
    }

    Ok((config, token))
}

/// Runs the tunnel command: creates resources, connects cloudflared and
/// blocks until Ctrl+C, a 4h timeout, or cloudflared exits.
pub fn run_tunnel(opts: &TunnelOptions) -> Result<()> {
    let store = ConfigStore::default()?;
    let (config, token) = load_ready_config(&store)?;
    let account_id = config.account_id.clone().unwrap();
    let zone_id = config.zone_id.clone().unwrap();
    let domain = config.domain.clone().unwrap();

    let subdomain = opts
        .subdomain
        .clone()
        .unwrap_or_else(random_subdomain)
        .to_lowercase();
    validate_subdomain(&subdomain)?;

    let full_name = format!("{subdomain}.{domain}");
    let api = CloudflareApi::new(token)?;

    ui::banner();
    let binary =
        cloudflared::ensure_installed().context("preparing cloudflared binary")?;

    resolve_conflicts(&api, &account_id, &zone_id, &subdomain, &full_name)?;

    // 1. Create the named tunnel.
    println!("  {}", format!("Creating tunnel \"{subdomain}\"...").dimmed());
    let tunnel = api
        .create_tunnel(&account_id, &subdomain)
        .context("creating Cloudflare tunnel")?;
    let tunnel_token = tunnel
        .token
        .clone()
        .ok_or_else(|| anyhow!("Cloudflare returned a tunnel without a token"))?;

    // 2. Configure remote ingress so cloudflared knows what to forward.
    let ingress = vec![
        IngressEntry {
            hostname: Some(full_name.clone()),
            service: opts.protocol.local_service(opts.port),
        },
        IngressEntry {
            hostname: None,
            service: "http_status:404".to_string(),
        },
    ];
    println!("  {}", format!("Configuring ingress ({})...", opts.protocol.as_str()).dimmed());
    if let Err(e) = api.set_tunnel_ingress(&account_id, &tunnel.id, &ingress) {
        let _ = api.delete_tunnel(&account_id, &tunnel.id);
        bail!("setting tunnel ingress: {e}");
    }

    // 3. Point DNS at the tunnel.
    let cname_target = format!("{}.cfargotunnel.com", tunnel.id);
    println!("  {}", format!("Creating DNS {full_name}...").dimmed());
    if let Err(e) = api.create_dns_record(&zone_id, &full_name, &cname_target) {
        let _ = api.delete_tunnel(&account_id, &tunnel.id);
        bail!("creating DNS record: {e}");
    }

    // 4. Spawn cloudflared using the remote ingress configuration.
    let mut child = cloudflared::spawn(&binary, &tunnel_token).context("starting cloudflared")?;

    let url = format!("https://{full_name}");
    ui::tunnel_live(&url, opts.port, opts.protocol.as_str(), &subdomain);

    // 4. Stream stderr for status lines.
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("cloudflared stderr not piped"))?;
    let connections = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&connections);
    let stderr_handle = cloudflared::stream_stderr(stderr, move |line| {
        match cloudflared::classify_line(&line) {
            LineKind::Connection => {
                let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
                if n == 1 {
                    println!("  {}", "Edge connection established".green());
                } else if n == 4 {
                    println!("  {}", "All 4 connections established".green());
                }
            }
            LineKind::Error => {
                eprintln!("  [cloudflared] {}", line.yellow());
            }
            LineKind::Ignore => {}
        }
    });

    // 5. Block until shutdown.
    let mut session = Session {
        api,
        account_id,
        zone_id,
        full_name,
        tunnel_id: tunnel.id,
        child: Some(child),
        cleaned: false,
    };

    let started = Instant::now();
    loop {
        if exit_requested() {
            break;
        }
        if started.elapsed() >= TUNNEL_TIMEOUT {
            println!("  {}", "4h age limit reached — cleaning up.".yellow());
            break;
        }
        let exited = session
            .child
            .as_mut()
            .map(|child| child.try_wait().ok().flatten().is_some())
            .unwrap_or(true);
        if exited {
            println!("  {}", "cloudflared exited — cleaning up.".yellow());
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }

    println!("  {}", "Cleaning up tunnel and DNS record...".dimmed());
    session.cleanup();
    let _ = stderr_handle.join();
    Ok(())
}

/// Runs the `cfp cleanup` command: removes dead tunnels, their DNS records
/// and orphaned CNAME records pointing at non-existent tunnels.
pub fn run_cleanup() -> Result<()> {
    let store = ConfigStore::default()?;
    let (config, token) = load_ready_config(&store)?;
    let account_id = config.account_id.clone().unwrap();
    let zone_id = config.zone_id.clone().unwrap();
    let domain = config.domain.clone().unwrap();

    let api = CloudflareApi::new(token)?;
    let mut removed = 0usize;

    // Dead tunnels.
    for status in ["down", "inactive", "degraded"] {
        let tunnels = match api.list_tunnels(&account_id, status) {
            Ok(tunnels) => tunnels,
            Err(e) => {
                println!("  {} {e}", "Skipping status scan:".yellow());
                continue;
            }
        };
        for tunnel in tunnels {
            println!("  Removing dead tunnel \"{}\" ({})...", tunnel.name, status);
            let full_name = format!("{}.{domain}", tunnel.name);
            if let Ok(Some(record)) = api.find_dns_record(&zone_id, &full_name) {
                let _ = api.delete_dns_record(&zone_id, &record.id);
                removed += 1;
            }
            api.cleanup_connections(&account_id, &tunnel.id)?;
            api.delete_tunnel(&account_id, &tunnel.id)?;
            removed += 1;
        }
    }

    // Orphaned CNAME records pointing at tunnels that no longer exist.
    let cnames = api.list_dns_records(&zone_id, "CNAME")?;
    for record in cnames {
        let Some(tunnel_id) = record.content.strip_suffix(".cfargotunnel.com") else {
            continue;
        };
        if !api.tunnel_exists(&account_id, tunnel_id) {
            println!("  Removing orphaned DNS record {}...", record.name);
            api.delete_dns_record(&zone_id, &record.id)?;
            removed += 1;
        }
    }

    ui::cleanup_summary(removed);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_round_trips() {
        for proto in [Protocol::Http, Protocol::Https, Protocol::Tcp, Protocol::Udp, Protocol::Ssh] {
            assert_eq!(Protocol::parse(proto.as_str()).unwrap(), proto);
        }
    }

    #[test]
    fn protocol_parse_is_case_insensitive() {
        assert_eq!(Protocol::parse("HTTP").unwrap(), Protocol::Http);
        assert_eq!(Protocol::parse("Tcp").unwrap(), Protocol::Tcp);
    }

    #[test]
    fn protocol_parse_rejects_unknown() {
        assert!(Protocol::parse("ftp").is_err());
        assert!(Protocol::parse("").is_err());
    }

    #[test]
    fn protocol_local_service() {
        assert_eq!(Protocol::Http.local_service(8080), "http://localhost:8080");
        assert_eq!(Protocol::Https.local_service(8443), "https://localhost:8443");
        assert_eq!(Protocol::Tcp.local_service(22), "tcp://localhost:22");
        assert_eq!(Protocol::Udp.local_service(5353), "udp://localhost:5353");
        assert_eq!(Protocol::Ssh.local_service(22), "ssh://localhost:22");
    }

    #[test]
    fn subdomain_validation() {
        assert!(validate_subdomain("myapp").is_ok());
        assert!(validate_subdomain("my-app-2").is_ok());
        assert!(validate_subdomain("MyApp").is_err());
        assert!(validate_subdomain("-myapp").is_err());
        assert!(validate_subdomain("myapp-").is_err());
        assert!(validate_subdomain("ab").is_err());
        assert!(validate_subdomain(&"a".repeat(64)).is_err());
        assert!(validate_subdomain("my app").is_err());
    }

    #[test]
    fn random_subdomain_shape() {
        for _ in 0..20 {
            let sub = random_subdomain();
            assert!(sub.starts_with("user-"), "got {sub}");
            assert!(validate_subdomain(&sub).is_ok(), "invalid {sub}");
        }
    }
}
