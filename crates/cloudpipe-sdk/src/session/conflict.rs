//! Deciding what to do about a subdomain that is already in use.
//!
//! A healthy tunnel serving the name is a hard error — silently stealing it
//! would take down someone else's URL. Anything else (a `down` tunnel, an
//! orphaned CNAME) is reclaimed.

use tracing::warn;

use crate::client::CloudflareApi;
use crate::error::{Error, Result};

/// Resolves subdomain conflicts before creating anything. Stale tunnels
/// (`down`/`inactive`/`degraded`) and orphaned DNS records are removed.
pub(crate) async fn resolve_conflicts(
    api: &CloudflareApi,
    account_id: &str,
    zone_id: &str,
    subdomain: &str,
    full_name: &str,
) -> Result<()> {
    // Every lookup below propagates its error. The previous version wrote
    // `if let Ok(existing) = ...`, which made a network blip or an auth
    // failure look exactly like "no such tunnel" — the session would then
    // charge ahead and fail later with a misleading `SubdomainInUse`.
    if let Some(existing) = api
        .find_tunnel_by_name(account_id, subdomain)
        .await?
        .into_iter()
        .next()
    {
        if existing.status == "healthy" {
            return Err(Error::SubdomainInUse(subdomain.to_string()));
        }
        warn!(
            "reclaiming stale tunnel \"{subdomain}\" ({})",
            existing.status
        );
        // The DNS record for a stale tunnel may or may not still exist;
        // a missing one is the normal case, not an error.
        if let Some(record) = api.find_dns_record(zone_id, full_name).await? {
            api.delete_dns_record(zone_id, &record.id).await?;
        }
        api.cleanup_connections(account_id, &existing.id).await?;
        api.delete_tunnel(account_id, &existing.id).await?;
    }

    if let Some(record) = api.find_dns_record(zone_id, full_name).await? {
        warn!("removing orphaned DNS record for {full_name}");
        api.delete_dns_record(zone_id, &record.id).await?;
    }
    Ok(())
}
