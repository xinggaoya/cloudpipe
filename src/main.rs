//! cfp CLI entry point — argument dispatch and global error handling.

mod cli;
mod cloudflare;
mod cloudflared;
mod config;
mod tunnel;
mod ui;

use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;
use colored::Colorize;

use cli::{Cli, Command, TunnelArgs};
use cloudflare::CloudflareApi;
use config::{Config, ConfigStore};

/// Default local port when neither the ngrok-style nor legacy-style argument
/// is provided.
pub const DEFAULT_PORT: u16 = 8080;

fn main() {
    let cli = Cli::parse();
    let result = run(cli);
    if let Err(error) = result {
        ui::error(&error.to_string());
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Some(Command::Key { token, clear }) => handle_key(token, clear),
        Some(Command::Domain { domain }) => handle_domain(domain),
        Some(Command::Domains) => handle_domains(),
        Some(Command::Cleanup) => tunnel::run_cleanup(),
        None => run_tunnel_command(cli.to_tunnel_args()?),
    }
}

/// Sets up the Ctrl+C handler — calls `tunnel::request_exit()` so the main
/// loop can perform a clean Cloudflare-side cleanup. Cheap to call even if
/// no tunnel will be opened.
fn install_ctrlc_handler() -> Result<()> {
    ctrlc::set_handler(tunnel::request_exit)
        .map_err(|e| anyhow!("cannot install Ctrl+C handler: {e}"))
}

/// Runs a tunnel from normalized `TunnelArgs`.
fn run_tunnel_command(args: TunnelArgs) -> Result<()> {
    install_ctrlc_handler()?;
    let opts = tunnel::TunnelOptions {
        protocol: args.protocol,
        port: args.port,
        subdomain: args.subdomain,
    };
    tunnel::run_tunnel(&opts)
}

/// Implements `cfp key [token] [--clear]`.
fn handle_key(token: Option<String>, clear: bool) -> Result<()> {
    let store = ConfigStore::default()?;
    let mut config = store.load();

    if clear {
        store.save(&Config::default())?;
        ui::success("All credentials cleared.");
        ui::key_status(&Config::default());
        return Ok(());
    }

    let Some(token) = token else {
        ui::key_status(&config);
        return Ok(());
    };

    let trimmed = token.trim();
    if trimmed.is_empty() {
        bail!("empty token — pass a non-empty value");
    }

    println!("  {}", "Verifying token with Cloudflare...".dimmed());
    let api = CloudflareApi::new(trimmed.to_string())
        .context("building API client")?;
    let status = api
        .verify_token()
        .context("verifying token (check the value, network, or permissions)")?;
    ui::success(&format!("Token verified (status: {}).", status.status));

    let zones = api
        .list_zones()
        .context("listing accessible zones — make sure the token has Zone:Read on at least one zone")?;

    if zones.is_empty() {
        bail!("token is valid but has access to no zones — add a Zone:Read resource to the token");
    }

    if zones.len() == 1 {
        apply_zone(&mut config, &zones[0]);
        config.token = Some(trimmed.to_string());
        store.save(&config)?;
        let domain = config.domain.clone().unwrap();
        ui::success(&format!("Saved. Tunnels will be exposed under {domain}."));
        ui::key_status(&config);
    } else {
        // Save token + first zone, let the user choose the rest.
        apply_zone(&mut config, &zones[0]);
        config.token = Some(trimmed.to_string());
        store.save(&config)?;
        println!();
        println!("{}", "Multiple zones are accessible:".bold());
        ui::zones(&zones, config.domain.as_deref());
        ui::success(&format!(
            "Saved (default domain: {}). Switch with `cfp domain <name>`.",
            config.domain.clone().unwrap()
        ));
    }
    Ok(())
}

fn apply_zone(config: &mut Config, zone: &cloudflare::Zone) {
    config.account_id = Some(zone.account.id.clone());
    config.zone_id = Some(zone.id.clone());
    config.domain = Some(zone.name.clone());
}

/// Implements `cfp domain <example.com>`.
fn handle_domain(domain: String) -> Result<()> {
    let store = ConfigStore::default()?;
    let mut config = store.load();
    let token = config
        .effective_token()
        .ok_or_else(|| anyhow!("no token configured — run `cfp key <token>` first"))?;

    let api = CloudflareApi::new(token).context("building API client")?;
    let zone = api
        .find_zone(&domain)
        .with_context(|| format!("looking up zone \"{domain}\""))?
        .ok_or_else(|| anyhow!("zone \"{domain}\" is not accessible with the saved token"))?;

    apply_zone(&mut config, &zone);
    store.save(&config)?;
    ui::success(&format!("Switched to {domain}."));
    Ok(())
}

/// Implements `cfp domains`.
fn handle_domains() -> Result<()> {
    let store = ConfigStore::default()?;
    let config = store.load();
    let token = config
        .effective_token()
        .ok_or_else(|| anyhow!("no token configured — run `cfp key <token>` first"))?;

    let api = CloudflareApi::new(token).context("building API client")?;
    let zones = api.list_zones().context("listing zones")?;
    ui::zones(&zones, config.domain.as_deref());
    Ok(())
}
