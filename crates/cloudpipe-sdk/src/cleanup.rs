//! One-shot cleanup of dead tunnels and orphaned DNS records.
//!
//! Useful both from the CLI (`cfp cleanup`) and from programs that want to
//! reconcile stale resources left behind by a previous crash.

use crate::client::CloudflareApi;
use crate::error::Result;

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
    let mut removed = 0usize;

    for status in ["down", "inactive", "degraded"] {
        let tunnels = match api.list_tunnels(account_id, status).await {
            Ok(t) => t,
            Err(err) => {
                tracing::warn!("skipping status {status}: {err}");
                continue;
            }
        };
        for tunnel in tunnels {
            let full_name = format!("{}.{domain}", tunnel.name);
            if let Ok(Some(record)) = api.find_dns_record(zone_id, &full_name).await {
                let _ = api.delete_dns_record(zone_id, &record.id).await;
                removed += 1;
            }
            api.cleanup_connections(account_id, &tunnel.id).await?;
            api.delete_tunnel(account_id, &tunnel.id).await?;
            removed += 1;
        }
    }

    // Orphaned CNAME records pointing at tunnels that no longer exist.
    let cnames = api.list_dns_records(zone_id, "CNAME").await?;
    for record in cnames {
        let Some(tunnel_id) = record.content.strip_suffix(".cfargotunnel.com") else {
            continue;
        };
        if !api.tunnel_exists(account_id, tunnel_id).await {
            let _ = api.delete_dns_record(zone_id, &record.id).await;
            removed += 1;
        }
    }

    Ok(removed)
}
