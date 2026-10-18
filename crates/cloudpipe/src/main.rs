//! cfp CLI entry point — argument dispatch and async runtime setup.

mod cli;
mod config;
mod logging;
mod ui;

use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;

use cli::{CleanupArgs, Cli, Command, TunnelArgs};
use cloudpipe_sdk::{CloudflareApi, TunnelBuilder};
use config::{Config, ConfigStore};

/// Default local port when neither the ngrok-style nor legacy-style argument
/// is provided.
pub const DEFAULT_PORT: u16 = 8080;

fn main() {
    let cli = Cli::parse();
    logging::init(logging::LogSetting::resolve(cli.verbose));
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
        Some(Command::Cleanup(args)) => handle_cleanup(args).await,
        None => run_tunnel_command(cli.to_tunnel_args()?).await,
    }
}

async fn run_tunnel_command(args: TunnelArgs) -> Result<()> {
    let store = ConfigStore::default()?;
    let config = store.load();
    let Some(token) = config.effective_token() else {
        ui::no_token_guide();
        bail!("no Cloudflare API token configured");
    };
    let ctx = config.require_tunnel_context()?;

    let mut builder = TunnelBuilder::new()
        .token(token)
        .account(ctx.account_id)
        .zone(ctx.zone_id)
        .domain(ctx.domain)
        .protocol(args.protocol)
        .port(args.port)
        .subdomain(args.subdomain.unwrap_or_default())
        .auto_restart(args.auto_restart);

    if args.insecure {
        builder = builder.no_tls_verify(true);
    }
    if let Some(host) = args.origin_host_header {
        builder = builder.origin_host_header(host);
    }
    if let Some(timeout) = args.origin_connect_timeout {
        builder = builder.origin_connect_timeout(timeout);
    }

    let mut handle = builder
        .on_event(ui::render_event)
        .start()
        .await
        .map_err(ui::render_sdk_error)?;

    ui::tunnel_live(
        handle.url(),
        args.port,
        &args.protocol.to_string(),
        handle.subdomain(),
        args.auto_restart,
    );

    // Wait for either Ctrl+C or the tunnel to exit on its own.
    //
    // `biased` polls the Ctrl+C branch first so that an immediate Ctrl+C
    // is observed even before `wait()` is armed, and so that `wait()`
    // never holds a borrow of the handle while we want to call `stop()`.
    //
    // Ctrl+C comes straight from tokio's signal driver — no `AtomicBool`
    // plus 100 ms polling loop, so shutdown latency is bounded by how
    // fast the OS delivers SIGINT rather than by a sleep interval.
    tokio::select! {
        biased;
        _ = tokio::signal::ctrl_c() => {
            if let Err(err) = handle.stop().await {
                ui::error(&format!("cleanup failed: {err:#}"));
            }
        }
        _ = handle.wait() => {}
    }
    Ok(())
}

async fn handle_key(token: Option<String>, clear: bool) -> Result<()> {
    let store = ConfigStore::default()?;
    let mut config = store.load();

    if clear {
        let cleared = Config::new_empty();
        store.save(&cleared)?;
        ui::success("All credentials cleared.");
        ui::key_status(&cleared);
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

    ui::step("Verifying token with Cloudflare...");
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

    config.apply_zone(&zones[0])?;
    config.token = Some(trimmed.to_string());
    store.save(&config)?;
    if zones.len() == 1 {
        ui::success(&format!(
            "Saved. Tunnels will be exposed under {}.",
            config.domain.clone().unwrap()
        ));
        ui::key_status(&config);
    } else {
        ui::heading("Multiple zones are accessible:");
        ui::zones(&zones, config.domain.as_deref());
        ui::success(&format!(
            "Saved (default domain: {}). Switch with `cfp domain <name>`.",
            config.domain.clone().unwrap()
        ));
    }
    Ok(())
}

async fn handle_domain(domain: String) -> Result<()> {
    let store = ConfigStore::default()?;
    let mut config = store.load();
    let api = load_api(&config)?;
    let zone = api
        .find_zone(&domain)
        .await
        .map_err(anyhow::Error::msg)
        .with_context(|| format!("looking up zone \"{domain}\""))?
        .ok_or_else(|| anyhow!("zone \"{domain}\" is not accessible with the saved token"))?;

    config.apply_zone(&zone)?;
    store.save(&config)?;
    ui::success(&format!("Switched to {domain}."));
    Ok(())
}

async fn handle_domains() -> Result<()> {
    let store = ConfigStore::default()?;
    let config = store.load();
    let api = load_api(&config)?;
    let zones = api.list_zones().await.map_err(anyhow::Error::msg)?;
    ui::zones(&zones, config.domain.as_deref());
    Ok(())
}

async fn handle_cleanup(args: CleanupArgs) -> Result<()> {
    let store = ConfigStore::default()?;
    let config = store.load();
    let ctx = config.require_tunnel_context()?;
    let api = load_api(&config)?;

    let report = if args.dry_run {
        cloudpipe_sdk::preview_cleanup(&api, &ctx.account_id, &ctx.zone_id, &ctx.domain).await
    } else {
        cloudpipe_sdk::cleanup_with_report(&api, &ctx.account_id, &ctx.zone_id, &ctx.domain).await
    }
    .map_err(ui::render_sdk_error)?;

    ui::cleanup_summary(&report, args.dry_run, args.json);
    Ok(())
}

/// Builds an API client from the effective token, reporting the standard
/// "run `cfp key` first" guidance when none is configured.
fn load_api(config: &Config) -> Result<CloudflareApi> {
    let token = config
        .effective_token()
        .ok_or_else(|| anyhow!("no token configured — run `cfp key <token>` first"))?;
    CloudflareApi::new(token).map_err(anyhow::Error::msg)
}
