//! One-shot cleanup of dead tunnels and orphaned DNS records.
//!
//! Useful both from the CLI (`cfp cleanup`) and from programs that want to
//! reconcile stale resources left behind by a previous crash.
//!
//! The walk is bounded and safe to run on a live zone: it only ever
//! touches tunnels Cloudflare itself reports as `down` / `inactive` /
//! `degraded`, plus CNAME records pointing at tunnel ids that no longer
//! resolve. Everything else is left alone.

use std::fmt;

use serde::Serialize;

use crate::client::CloudflareApi;
use crate::error::Result;

/// Tunnel statuses Cloudflare uses for a tunnel with no live connector.
const DEAD_STATUSES: &[&str] = &["down", "inactive", "degraded"];

/// Suffix Cloudflare gives to tunnel-backed CNAME targets.
const TUNNEL_CNAME_SUFFIX: &str = ".cfargotunnel.com";

/// A single thing [`cleanup_with_report`] would remove.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum CleanupAction {
    /// A dead tunnel, identified by id and name.
    Tunnel {
        /// Cloudflare tunnel id.
        id: String,
        /// Tunnel name (== the subdomain it served).
        name: String,
    },
    /// A DNS CNAME record that pointed at a tunnel.
    DnsRecord {
        /// DNS record id.
        id: String,
        /// Full hostname.
        name: String,
    },
}

impl fmt::Display for CleanupAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tunnel { id, name } => write!(f, "tunnel {id} ({name})"),
            Self::DnsRecord { id, name } => write!(f, "dns {id} ({name})"),
        }
    }
}

/// What a cleanup run found and (unless `dry_run`) did.
///
/// Serializes with camelCase keys so the `--json` output can be consumed
/// directly by the Node/JS tooling without a translation layer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CleanupReport {
    /// Resources actually deleted.
    pub removed: Vec<CleanupAction>,
    /// Resources that would have been deleted, when `dry_run` was set.
    pub would_remove: Vec<CleanupAction>,
    /// Per-resource failures. A cleanup that hits one dead record should
    /// still finish the rest of the zone.
    pub errors: Vec<String>,
}

impl CleanupReport {
    /// Whether the run touched (or would touch) anything at all.
    pub fn is_empty(&self) -> bool {
        self.removed.is_empty() && self.would_remove.is_empty()
    }

    /// Number of resources removed — the "old" scalar, kept because it is
    /// what the CLI's one-line summary prints.
    pub fn removed_count(&self) -> usize {
        self.removed.len()
    }

    fn record(&mut self, action: CleanupAction, dry_run: bool) {
        if dry_run {
            self.would_remove.push(action);
        } else {
            self.removed.push(action);
        }
    }
}

/// Removes tunnels in `down` / `inactive` / `degraded` status, their DNS
/// records, and orphaned CNAMEs pointing at non-existent tunnels.
///
/// Returns the number of resources removed.
pub async fn cleanup(
    api: &CloudflareApi,
    account_id: &str,
    zone_id: &str,
    domain: &str,
) -> Result<usize> {
    Ok(cleanup_with_report(api, account_id, zone_id, domain)
        .await?
        .removed_count())
}

/// Same as [`cleanup`] but reports every action and every failure instead
/// of collapsing the result into a count.
///
/// With `dry_run = true` nothing is deleted; the findings land in
/// [`CleanupReport::would_remove`]. That mode exists so a user can preview
/// the blast radius on a production zone before letting `cfp cleanup` run
/// for real.
pub async fn cleanup_with_report(
    api: &CloudflareApi,
    account_id: &str,
    zone_id: &str,
    domain: &str,
    // `dry_run` is threaded through the public helper below rather than
    // being a parameter of this signature so the common case stays terse.
) -> Result<CleanupReport> {
    cleanup_inner(api, account_id, zone_id, domain, false).await
}

/// Preview variant of [`cleanup_with_report`].
pub async fn preview_cleanup(
    api: &CloudflareApi,
    account_id: &str,
    zone_id: &str,
    domain: &str,
) -> Result<CleanupReport> {
    cleanup_inner(api, account_id, zone_id, domain, true).await
}

async fn cleanup_inner(
    api: &CloudflareApi,
    account_id: &str,
    zone_id: &str,
    domain: &str,
    dry_run: bool,
) -> Result<CleanupReport> {
    let mut report = CleanupReport::default();

    for status in DEAD_STATUSES {
        let tunnels = match api.list_tunnels(account_id, status).await {
            Ok(t) => t,
            Err(err) => {
                // A token scoped to DNS but not Tunnel:Edit lands here.
                // Skip that status rather than failing the whole run.
                tracing::warn!("skipping status {status}: {err}");
                report
                    .errors
                    .push(format!("listing tunnels with status {status}: {err}"));
                continue;
            }
        };
        for tunnel in tunnels {
            let full_name = format!("{}.{}", tunnel.name, domain);
            if let Ok(Some(record)) = api.find_dns_record(zone_id, &full_name).await {
                let action = CleanupAction::DnsRecord {
                    id: record.id.clone(),
                    name: record.name.clone(),
                };
                if dry_run {
                    report.record(action, true);
                } else if let Err(err) = api.delete_dns_record(zone_id, &record.id).await {
                    report.errors.push(format!("deleting dns {}: {err}", record.id));
                } else {
                    report.record(action, false);
                }
            }
            let action = CleanupAction::Tunnel {
                id: tunnel.id.clone(),
                name: tunnel.name.clone(),
            };
            if dry_run {
                report.record(action, true);
                continue;
            }
            let _ = api.cleanup_connections(account_id, &tunnel.id).await;
            if let Err(err) = api.delete_tunnel(account_id, &tunnel.id).await {
                report.errors.push(format!("deleting tunnel {}: {err}", tunnel.id));
            } else {
                report.record(action, false);
            }
        }
    }

    // Orphaned CNAME records pointing at tunnels that no longer exist.
    let cnames = api.list_dns_records(zone_id, "CNAME").await?;
    for record in cnames {
        let Some(tunnel_id) = record.content.strip_suffix(TUNNEL_CNAME_SUFFIX) else {
            continue;
        };
        if api.tunnel_exists(account_id, tunnel_id).await {
            continue;
        }
        let action = CleanupAction::DnsRecord {
            id: record.id.clone(),
            name: record.name.clone(),
        };
        if dry_run {
            report.record(action, true);
        } else if let Err(err) = api.delete_dns_record(zone_id, &record.id).await {
            report.errors.push(format!("deleting orphaned dns {}: {err}", record.id));
        } else {
            report.record(action, false);
        }
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_separates_dry_run_findings() {
        let mut report = CleanupReport::default();
        assert!(report.is_empty());

        report.record(
            CleanupAction::Tunnel {
                id: "t1".into(),
                name: "app".into(),
            },
            true,
        );
        assert!(report.removed.is_empty());
        assert_eq!(report.would_remove.len(), 1);
        assert!(!report.is_empty());
        assert_eq!(report.removed_count(), 0);
    }

    #[test]
    fn report_counts_removals() {
        let mut report = CleanupReport::default();
        report.record(
            CleanupAction::DnsRecord {
                id: "r1".into(),
                name: "a.example.com".into(),
            },
            false,
        );
        report.record(
            CleanupAction::Tunnel {
                id: "t1".into(),
                name: "a".into(),
            },
            false,
        );
        assert_eq!(report.removed_count(), 2);
        assert!(report.errors.is_empty());
    }

    #[test]
    fn actions_render_readably() {
        assert_eq!(
            CleanupAction::Tunnel {
                id: "t1".into(),
                name: "app".into()
            }
            .to_string(),
            "tunnel t1 (app)"
        );
        assert_eq!(
            CleanupAction::DnsRecord {
                id: "r1".into(),
                name: "app.example.com".into()
            }
            .to_string(),
            "dns r1 (app.example.com)"
        );
    }

    #[test]
    fn dead_statuses_cover_the_documented_set() {
        assert_eq!(DEAD_STATUSES, ["down", "inactive", "degraded"]);
    }

    #[test]
    fn only_cfargotunnel_cnames_are_considered() {
        assert_eq!(
            "abc.cfargotunnel.com".strip_suffix(TUNNEL_CNAME_SUFFIX),
            Some("abc")
        );
        assert_eq!("example.com".strip_suffix(TUNNEL_CNAME_SUFFIX), None);
        assert_eq!("abc.cfargotunnel.com.evil.net".strip_suffix(TUNNEL_CNAME_SUFFIX), None);
    }
}
