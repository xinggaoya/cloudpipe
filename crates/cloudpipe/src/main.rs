//! cfp CLI entry point — argument dispatch and async runtime setup.

mod cli;
mod config;
mod ui;

use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;
use colored::Colorize;

use cli::{Cli, Command, TunnelArgs};
use cloudpipe_sdk::{CloudflareApi, TunnelBuilder};
use config::{Config, ConfigStore};

/// Default local port when neither the ngrok-style nor legacy-style argument
/// is provided.
pub const DEFAULT_PORT: u16 = 8080;

static EXIT_REQUESTED: AtomicBool = AtomicBool::new(false);

fn main() {
    let cli = Cli::parse();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");
    let result = runtime.block_on(run(cli));
    if let Err(error) = result {
        ui::error(&format!("{error:#}"));
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Some(Command::Key { token, clear }) => handle_key(token, clear).await,
        Some(Command::Domain { domain }) => handle_domain(domain).await,
        Some(Command::Domains) => handle_domains().await,
        Some(Command::Cleanup) => handle_cleanup().await,
        None => run_tunnel_command(cli.to_tunnel_args()?).await,
    }
}

async fn run_tunnel_command(args: TunnelArgs) -> Result<()> {
    install_ctrlc_handler()?;

    let store = ConfigStore::default()?;
    let config = store.load();
    let Some(token) = config.effective_token() else {
        ui::no_token_guide();
        bail!("no Cloudflare API token configured");
    };
    let account_id = config
        .account_id
        .clone()
        .ok_or_else(|| anyhow!("account not configured — run `cfp key <token>` first"))?;
    let zone_id = config
        .zone_id
        .clone()
        .ok_or_else(|| anyhow!("zone not configured — run `cfp key <token>` first"))?;
    let domain = config
        .domain
        .clone()
        .ok_or_else(|| anyhow!("domain not configured — run `cfp key <token>` first"))?;

    let mut handle = TunnelBuilder::new()
        .token(token)
        .account(account_id)
        .zone(zone_id)
        .domain(domain)
        .protocol(args.protocol)
        .port(args.port)
        .subdomain(args.subdomain.unwrap_or_default())
        .on_event(ui::render_event)
        .start()
        .await
        .map_err(anyhow::Error::msg)?;

    ui::tunnel_live(
        handle.url(),
        args.port,
        &args.protocol.to_string(),
        handle.subdomain(),
    );

    // Wait for either Ctrl+C or the tunnel to exit on its own.
    // `biased` polls the Ctrl+C branch first so that an immediate Ctrl+C
    // is observed even before `wait()` is armed, and so that `wait()`
    // never holds a borrow of the handle while we want to call `stop()`.
    tokio::select! {
        biased;
        _ = wait_for_ctrlc() => {
            EXIT_REQUESTED.store(true, Ordering::SeqCst);
            if let Err(err) = handle.stop().await {
                ui::error(&format!("cleanup failed: {err:#}"));
            }
        }
        _ = handle.wait() => {}
    }
    Ok(())
}

async fn wait_for_ctrlc() {
    loop {
        if EXIT_REQUESTED.load(Ordering::SeqCst) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

fn install_ctrlc_handler() -> Result<()> {
    ctrlc::set_handler(|| {
        EXIT_REQUESTED.store(true, Ordering::SeqCst);
    })
    .map_err(|e| anyhow!("cannot install Ctrl+C handler: {e}"))
}

async fn handle_key(token: Option<String>, clear: bool) -> Result<()> {
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
    let api = CloudflareApi::new(trimmed.to_string()).map_err(anyhow::Error::msg)?;
    let status = api
        .verify_token()
        .await
        .map_err(anyhow::Error::msg)
        .context("verifying token (check the value, network, or permissions)")?;
    ui::success(&format!("Token verified (status: {}).", status.status));

    let zones = api.list_zones().await.map_err(anyhow::Error::msg).context(
        "listing accessible zones — make sure the token has Zone:Read on at least one zone",
    )?;

    if zones.is_empty() {
        bail!("token is valid but has access to no zones — add a Zone:Read resource to the token");
    }

    apply_zone(&mut config, &zones[0]);
    config.token = Some(trimmed.to_string());
    store.save(&config)?;
    if zones.len() == 1 {
        ui::success(&format!(
            "Saved. Tunnels will be exposed under {}.",
            config.domain.clone().unwrap()
        ));
        ui::key_status(&config);
    } else {
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

fn apply_zone(config: &mut Config, zone: &cloudpipe_sdk::Zone) {
    config.account_id = Some(zone.account.id.clone());
    config.zone_id = Some(zone.id.clone());
    config.domain = Some(zone.name.clone());
}

async fn handle_domain(domain: String) -> Result<()> {
    let store = ConfigStore::default()?;
    let mut config = store.load();
    let token = config
        .effective_token()
        .ok_or_else(|| anyhow!("no token configured — run `cfp key <token>` first"))?;

    let api = CloudflareApi::new(token).map_err(anyhow::Error::msg)?;
    let zone = api
        .find_zone(&domain)
        .await
        .map_err(anyhow::Error::msg)
        .with_context(|| format!("looking up zone \"{domain}\""))?
        .ok_or_else(|| anyhow!("zone \"{domain}\" is not accessible with the saved token"))?;

    apply_zone(&mut config, &zone);
    store.save(&config)?;
    ui::success(&format!("Switched to {domain}."));
    Ok(())
}

async fn handle_domains() -> Result<()> {
    let store = ConfigStore::default()?;
    let config = store.load();
    let token = config
        .effective_token()
        .ok_or_else(|| anyhow!("no token configured — run `cfp key <token>` first"))?;

    let api = CloudflareApi::new(token).map_err(anyhow::Error::msg)?;
    let zones = api.list_zones().await.map_err(anyhow::Error::msg)?;
    ui::zones(&zones, config.domain.as_deref());
    Ok(())
}

async fn handle_cleanup() -> Result<()> {
    let store = ConfigStore::default()?;
    let config = store.load();
    let token = config
        .effective_token()
        .ok_or_else(|| anyhow!("no token configured — run `cfp key <token>` first"))?;
    let account_id = config
        .account_id
        .clone()
        .ok_or_else(|| anyhow!("account not configured — run `cfp key <token>` first"))?;
    let zone_id = config
        .zone_id
        .clone()
        .ok_or_else(|| anyhow!("zone not configured — run `cfp key <token>` first"))?;
    let domain = config
        .domain
        .clone()
        .ok_or_else(|| anyhow!("domain not configured — run `cfp key <token>` first"))?;

    let api = CloudflareApi::new(token).map_err(anyhow::Error::msg)?;
    let removed = cloudpipe_sdk::cleanup(&api, &account_id, &zone_id, &domain)
        .await
        .map_err(anyhow::Error::msg)?;
    ui::cleanup_summary(removed);
    Ok(())
}
