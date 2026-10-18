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

use futures::stream::StreamExt as _;
use serde::Serialize;

use crate::client::{CloudflareApi, DnsRecord, Tunnel};
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
) -> Result<CleanupReport> {
    cleanup_inner(api, account_id, zone_id, domain, false).await
}

/// Preview variant of [`cleanup_with_report`]: identical walk, but nothing
/// is deleted and every finding lands in [`CleanupReport::would_remove`].
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
    // A record can be reachable from both passes — the dead-tunnel walk
    // finds it by hostname, and the orphan scan finds it because its
    // tunnel no longer resolves. Reporting it twice inflates the count and
    // (in the non-dry-run case) means a second DELETE for the same id.
    let mut seen_records: std::collections::HashSet<String> = std::collections::HashSet::new();

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

        // Tunnels are independent of each other, so they are reclaimed
        // concurrently. The previous fully sequential walk cost three
        // round-trips per leftover, which on a zone with a few hundred of
        // them is tens of seconds of a user staring at a silent prompt.
        //
        // `buffer_unordered` keeps the fan-out bounded so a large zone
        // cannot open hundreds of sockets and trip Cloudflare's
        // per-token rate limit — which would make cleanup fail at exactly
        // the moment it is most needed.
        let mut streams = futures::stream::iter(tunnels)
            .map(|tunnel| reclaim_tunnel(api, account_id, zone_id, domain, tunnel, dry_run))
            .buffer_unordered(CLEANUP_CONCURRENCY);
        while let Some(part) = streams.next().await {
            merge_deduped(&mut report, part, &mut seen_records);
        }
    }

    // Orphaned CNAME records pointing at tunnels that no longer exist.
    let cnames = api.list_dns_records(zone_id, "CNAME").await?;
    let mut streams = futures::stream::iter(cnames)
        // Only CNAMEs pointing at a tunnel are candidates; anything else
        // in the zone belongs to the user and is never touched.
        .filter_map(|record| futures::future::ready(tunnel_target(&record)))
        .map(|(record, tunnel_id)| async move {
            if api.tunnel_exists(account_id, &tunnel_id).await {
                return CleanupReport::default();
            }
            let action = CleanupAction::DnsRecord {
                id: record.id.clone(),
                name: record.name.clone(),
            };
            let mut r = CleanupReport::default();
            if dry_run {
                r.record(action, true);
                return r;
            }
            match api.delete_dns_record(zone_id, &record.id).await {
                Ok(()) => r.record(action, false),
                Err(err) => r
                    .errors
                    .push(format!("deleting orphaned dns {}: {err}", record.id)),
            }
            r
        })
        .buffer_unordered(CLEANUP_CONCURRENCY);
    while let Some(part) = streams.next().await {
        merge_deduped(&mut report, part, &mut seen_records);
    }

    Ok(report)
}

/// Folds one worker's outcome in, skipping records already handled.
///
/// A record is reachable from both passes: the dead-tunnel walk finds it by
/// hostname, and the orphan scan finds it because its target tunnel no
/// longer resolves. Reporting it twice inflates the count, and in the
/// non-dry-run case means a second `DELETE` for an id that is already gone.
fn merge_deduped(
    into: &mut CleanupReport,
    from: CleanupReport,
    seen: &mut std::collections::HashSet<String>,
) {
    let keep = |action: CleanupAction, seen: &mut std::collections::HashSet<String>| {
        match &action {
            CleanupAction::DnsRecord { id, .. } if seen.contains(id) => None,
            CleanupAction::DnsRecord { id, .. } => {
                seen.insert(id.clone());
                Some(action)
            }
            // A tunnel id can legitimately appear once per dead status, but
            // the statuses are disjoint, so no dedup is needed.
            CleanupAction::Tunnel { .. } => Some(action),
        }
    };

    let CleanupReport {
        removed,
        would_remove,
        errors,
    } = from;
    into.removed
        .extend(removed.into_iter().filter_map(|a| keep(a, seen)));
    into.would_remove
        .extend(would_remove.into_iter().filter_map(|a| keep(a, seen)));
    into.errors.extend(errors);
}

/// How many resources are reclaimed at once.
///
/// Well below Cloudflare's per-token burst allowance so cleanup on a large
/// zone does not itself trip a rate limit.
const CLEANUP_CONCURRENCY: usize = 8;

/// The tunnel id a CNAME record points at, if it points at one.
///
/// Returns `None` for any record that is not a tunnel-backed CNAME —
/// `_dmarc`, `_acme-challenge`, a user's own mail record — which cleanup
/// must never touch.
fn tunnel_target(record: &DnsRecord) -> Option<(DnsRecord, String)> {
    let id = record.content.strip_suffix(TUNNEL_CNAME_SUFFIX)?;
    Some((record.clone(), id.to_string()))
}

/// Reclaims one dead tunnel and its DNS record.
///
/// A failure on one resource never aborts the rest: a single record stuck
/// behind a permissions problem should not leave a zone half-cleaned.
async fn reclaim_tunnel(
    api: &CloudflareApi,
    account_id: &str,
    zone_id: &str,
    domain: &str,
    tunnel: Tunnel,
    dry_run: bool,
) -> CleanupReport {
    let mut report = CleanupReport::default();
    let full_name = format!("{}.{}", tunnel.name, domain);

    // The DNS record for a stale tunnel may or may not still exist, and a
    // missing one is the normal case. A *lookup failure* is not "absent"
    // though — reading it as one (via `if let Ok(...)`) silently skipped
    // the record on any network blip.
    match api.find_dns_record(zone_id, &full_name).await {
        Ok(Some(record)) => {
            let action = CleanupAction::DnsRecord {
                id: record.id.clone(),
                name: record.name.clone(),
            };
            if dry_run {
                report.record(action, true);
            } else if let Err(err) = api.delete_dns_record(zone_id, &record.id).await {
                report
                    .errors
                    .push(format!("deleting dns {}: {err}", record.id));
            } else {
                report.record(action, false);
            }
        }
        Ok(None) => {}
        Err(err) => report
            .errors
            .push(format!("looking up dns {full_name}: {err}")),
    }

    let action = CleanupAction::Tunnel {
        id: tunnel.id.clone(),
        name: tunnel.name.clone(),
    };
    if dry_run {
        report.record(action, true);
        return report;
    }

    let _ = api.cleanup_connections(account_id, &tunnel.id).await;
    match api.delete_tunnel(account_id, &tunnel.id).await {
        Ok(()) => report.record(action, false),
        Err(err) => report
            .errors
            .push(format!("deleting tunnel {}: {err}", tunnel.id)),
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

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
    fn merging_dedupes_records_but_not_tunnels() {
        let record = CleanupAction::DnsRecord {
            id: "r1".into(),
            name: "a.example.com".into(),
        };
        let mut into = CleanupReport::default();
        let mut seen = std::collections::HashSet::new();

        // Same record arriving twice — from the dead-tunnel pass and the
        // orphan pass — is counted once.
        for _ in 0..2 {
            let mut from = CleanupReport::default();
            from.removed.push(record.clone());
            merge_deduped(&mut into, from, &mut seen);
        }
        assert_eq!(into.removed.len(), 1, "{into:?}");

        // A tunnel with the same id is a different resource and is kept.
        let tunnel = CleanupAction::Tunnel {
            id: "r1".into(),
            name: "a".into(),
        };
        let mut from = CleanupReport::default();
        from.removed.push(tunnel);
        merge_deduped(&mut into, from, &mut seen);
        assert_eq!(into.removed.len(), 2, "{into:?}");

        // Errors are never dropped — a failed deletion must stay visible.
        let mut failing = CleanupReport::default();
        failing.errors.push("boom".into());
        merge_deduped(&mut into, failing, &mut seen);
        assert_eq!(into.errors, vec!["boom"]);
    }

    #[test]
    fn only_cfargotunnel_cnames_are_candidates() {
        let mk = |content: &str| DnsRecord {
            id: "r".into(),
            name: "n.example.com".into(),
            content: content.into(),
        };
        let (record, id) = tunnel_target(&mk("abc.cfargotunnel.com")).expect("a tunnel CNAME");
        assert_eq!(id, "abc");
        assert_eq!(record.content, "abc.cfargotunnel.com");
        // Everything a real zone also contains must be left alone.
        for content in [
            "example.com",
            "abc.cfargotunnel.com.evil.net",
            "_dmarc.example.com",
            "abc.cfargotunnel.como",
        ] {
            assert!(tunnel_target(&mk(content)).is_none(), "{content}");
        }
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
    fn concurrency_is_bounded_but_useful() {
        // Unbounded fan-out against a per-token rate limit would turn a
        // large cleanup into a rate-limit failure; a width of 1 would make
        // the concurrency pointless.
        const _: () = assert!(CLEANUP_CONCURRENCY > 1 && CLEANUP_CONCURRENCY <= 16);
    }

    #[test]
    fn report_serializes_as_camel_case_for_json_consumers() {
        let mut report = CleanupReport::default();
        report.removed.push(CleanupAction::Tunnel {
            id: "t1".into(),
            name: "app".into(),
        });
        let json: Value = serde_json::from_str(&serde_json::to_string(&report).unwrap()).unwrap();
        assert!(json["wouldRemove"].is_array());
        assert!(json["errors"].is_array());
        assert_eq!(json["removed"][0]["kind"], "tunnel");
        assert_eq!(json["removed"][0]["id"], "t1");
    }
}
