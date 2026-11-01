//! cfp CLI entry point — argument dispatch and async runtime setup.

mod cli;
mod config;
mod logging;
mod ui;

use std::cell::OnceCell;

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
    // One context per invocation: the config store, the parsed config, and
    // a lazily-built API client. Previously each handler repeated the
    // same "open store → load → maybe bail on a missing token" sequence,
    // and `run_tunnel_command` did not even share the token check the
    // other three used — so the same failure produced two different
    // messages depending on which command hit it.
    let ctx = Ctx::load()?;
    match cli.command {
        Some(Command::Key {
            token,
            token_file,
            clear,
        }) => handle_key(&ctx, token, token_file, clear).await,
        Some(Command::Domain { domain }) => handle_domain(&ctx, domain).await,
        Some(Command::Domains) => handle_domains(&ctx).await,
        Some(Command::Cleanup(args)) => handle_cleanup(&ctx, args).await,
        None => run_tunnel_command(&ctx, cli.to_tunnel_args()?).await,
    }
}

/// Everything a command needs from disk and the environment.
struct Ctx {
    store: ConfigStore,
    config: Config,
    /// Cached so a handler that makes several API calls reuses one client
    /// (and therefore one connection pool).
    api: OnceCell<CloudflareApi>,
}

impl Ctx {
    /// Opens the config store and reads the current config.
    fn load() -> Result<Self> {
        let store = ConfigStore::default()?;
        let config = store.load();
        Ok(Self {
            store,
            config,
            api: OnceCell::new(),
        })
    }

    /// The effective token, with the standard "run `cfp key` first" help.
    fn token(&self) -> Result<String> {
        self.config.effective_token().ok_or_else(|| {
            anyhow!("no Cloudflare API token configured — run `cfp key <token>` first")
        })
    }

    /// An API client for the effective token.
    ///
    /// The same message for every command: previously three handlers said
    /// "no token configured" and a fourth said "no Cloudflare API token
    /// configured", so the advice a user saw depended on the command.
    fn api(&self) -> Result<&CloudflareApi> {
        if let Some(api) = self.api.get() {
            return Ok(api);
        }
        let api = CloudflareApi::new(self.token()?).map_err(anyhow::Error::msg)?;
        let _ = self.api.set(api);
        self.api
            .get()
            .ok_or_else(|| anyhow!("could not initialise the Cloudflare client"))
    }

    /// The account/zone/domain trio, for commands that operate on one zone.
    fn zone(&self) -> Result<config::TunnelContext> {
        self.config.require_tunnel_context()
    }
}

async fn run_tunnel_command(ctx: &Ctx, args: TunnelArgs) -> Result<()> {
    if ctx.config.effective_token().is_none() {
        // Starting a tunnel is the one command where the missing token is
        // worth a setup walkthrough rather than a one-liner.
        ui::no_token_guide();
        bail!("no Cloudflare API token configured");
    }
    let zone = ctx.zone()?;
    let token = ctx.token()?;

    let mut builder = TunnelBuilder::new()
        .token(token)
        .account(zone.account_id)
        .zone(zone.zone_id)
        .domain(zone.domain)
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
        args.protocol,
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

async fn handle_key(
    ctx: &Ctx,
    token: Option<String>,
    token_file: Option<String>,
    clear: bool,
) -> Result<()> {
    if clear {
        let cleared = Config::new_empty();
        ctx.store.save(&cleared)?;
        ui::success("All credentials cleared.");
        ui::key_status(&cleared);
        return Ok(());
    }

    // Three ways to supply the token, in order of preference. Reading it
    // from stdin or a file keeps it out of `ps` output and shell history.
    let token = match (token.as_deref(), token_file.as_deref()) {
        (Some(_), Some(_)) => {
            bail!("pass the token either as an argument or via --token-file, not both")
        }
        (Some("-"), None) => read_token_from_stdin()?,
        (Some(value), None) => {
            eprintln!(
                "warning: a token passed as an argument is visible in `ps` output — \
                 prefer `cfp key -` or `cfp key --token-file <path>`"
            );
            value.to_string()
        }
        (None, Some(path)) => read_token_from_file(path)?,
        (None, None) => {
            // Bare `cfp key` shows the current status, so a missing token
            // is the documented outcome here rather than an error.
            ui::key_status(&ctx.config);
            return Ok(());
        }
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
        .map_err(ui::render_sdk_error)
        .context("verifying token — check the value, your network, and the token's permissions")?;
    ui::success(&format!("Token verified (status: {}).", status.status));

    let zones = api
        .list_zones()
        .await
        .map_err(ui::render_sdk_error)
        .with_context(|| {
            "listing accessible zones — make sure the token has Zone:Read on at least one zone"
        })?;

    if zones.is_empty() {
        bail!("token is valid but has access to no zones — add a Zone:Read resource to the token");
    }

    let mut config = ctx.config.clone();
    save_token_and_zone(&ctx.store, &mut config, trimmed, &zones[0])?;

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

async fn handle_domain(ctx: &Ctx, domain: String) -> Result<()> {
    let zone = ctx
        .api()?
        .find_zone(&domain)
        .await
        .map_err(ui::render_sdk_error)
        .with_context(|| format!("looking up zone \"{domain}\""))?
        .ok_or_else(|| {
            anyhow!("zone \"{domain}\" is not accessible with the saved token — run `cfp domains` to list what you can reach")
        })?;

    let mut config = ctx.config.clone();
    config.apply_zone(&zone)?;
    ctx.store.save(&config)?;
    ui::success(&format!("Switched to {domain}."));
    Ok(())
}

async fn handle_domains(ctx: &Ctx) -> Result<()> {
    let zones = ctx
        .api()?
        .list_zones()
        .await
        .map_err(ui::render_sdk_error)?;
    ui::zones(&zones, ctx.config.domain.as_deref());
    Ok(())
}

async fn handle_cleanup(ctx: &Ctx, args: CleanupArgs) -> Result<()> {
    let zone = ctx.zone()?;
    let api = ctx.api()?;

    let report = if args.dry_run {
        cloudpipe_sdk::preview_cleanup(api, &zone.account_id, &zone.zone_id, &zone.domain).await
    } else {
        cloudpipe_sdk::cleanup_with_report(api, &zone.account_id, &zone.zone_id, &zone.domain).await
    }
    .map_err(ui::render_sdk_error)?;

    ui::cleanup_summary(&report, args.dry_run, args.json);
    Ok(())
}

/// Saves the token and resolves the zone to expose tunnels under.
///
/// The token is written *before* the zone is resolved. A zone whose
/// account id Cloudflare omitted used to abort the whole command with
/// nothing written to disk, so re-running `cfp key` produced the
/// identical failure and the user could never get past it.
fn save_token_and_zone(
    store: &ConfigStore,
    config: &mut Config,
    token: &str,
    zone: &cloudpipe_sdk::Zone,
) -> Result<()> {
    config.token = Some(token.to_string());
    store.save(config)?;

    config.apply_zone(zone).map_err(|err| {
        anyhow!("{err:#}\n\n  The token was saved — pick a zone with `cfp domain <name>`.")
    })?;
    store.save(config)?;
    Ok(())
}

/// Reads a token from stdin, one line.
///
/// This is the safest way to hand a secret to the process: it never
/// reaches the process table, the shell history, or a crash report.
fn read_token_from_stdin() -> Result<String> {
    use std::io::BufRead as _;
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .context("reading the token from stdin")?;
    Ok(line)
}

/// Reads a token from a file.
fn read_token_from_file(path: &str) -> Result<String> {
    std::fs::read_to_string(path).with_context(|| format!("reading the token from {path}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cloudpipe_sdk::Zone;

    #[test]
    fn a_failed_zone_resolution_still_keeps_the_token() {
        // `apply_zone` fails when the zone carries no account id. The
        // token must survive that failure — losing it means the user
        // re-runs `cfp key` and hits the identical error.
        let dir = tempfile::tempdir().expect("temp dir");
        let store = ConfigStore::with_dir(dir.path().to_path_buf());
        let mut config = Config::default();

        let zone = Zone {
            id: "zone-1".into(),
            name: "example.com".into(),
            account: None,
        };
        let err = save_token_and_zone(&store, &mut config, "cf_test", &zone)
            .expect_err("the zone resolution must fail");
        assert!(format!("{err:#}").contains("account"), "{err:#}");

        // The token survived, and the error says so.
        let saved = store.load();
        assert_eq!(saved.token.as_deref(), Some("cf_test"));
        assert!(format!("{err:#}").contains("token was saved"), "{err:#}");
    }

    #[test]
    fn a_successful_resolution_saves_both_the_token_and_the_zone() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = ConfigStore::with_dir(dir.path().to_path_buf());
        let mut config = Config::default();

        let zone = Zone {
            id: "zone-1".into(),
            name: "example.com".into(),
            account: Some(cloudpipe_sdk::ZoneAccount {
                id: "account-1".into(),
            }),
        };
        save_token_and_zone(&store, &mut config, "cf_test", &zone).expect("the zone resolves");

        let saved = store.load();
        assert_eq!(saved.token.as_deref(), Some("cf_test"));
        assert_eq!(saved.domain.as_deref(), Some("example.com"));
        assert_eq!(saved.zone_id.as_deref(), Some("zone-1"));
        assert_eq!(saved.account_id.as_deref(), Some("account-1"));
    }
}
