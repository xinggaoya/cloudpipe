//! cfp CLI entry point — argument dispatch and async runtime setup.

mod cli;
mod config;
mod credentials;
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
    let ctx = Ctx::load().await?;
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
    /// Overrides the Cloudflare API base URL. Only the tests set it, and
    /// they point it at an in-process mock.
    api_base: Option<String>,
    /// Where the token may be stored.
    credential_store: credentials::Store,
    /// The token, resolved from the environment, the OS credential store
    /// or the config file — in that order.
    token: Option<String>,
    /// The same resolution, with the token's *source*.
    resolved: credentials::Resolved,
}

impl Ctx {
    /// Opens the config store, reads the current config and resolves the
    /// token.
    async fn load() -> Result<Self> {
        let store = ConfigStore::default()?;
        let mut ctx = Self::with_store(store)?;
        ctx.credential_store = credentials::Store::Auto;

        // A token that is still sitting in the config file is moved into
        // the OS credential store, so the file stops holding a secret in
        // plaintext.
        if migrate_legacy_token(&ctx.store, &mut ctx.config, ctx.credential_store).await {
            ui::note(
                "Moved your API token from the config file into the OS credential store.\n  \
                 The config file no longer contains the token.",
            );
        }

        let resolved = credentials::resolve_token(&ctx.config, ctx.credential_store).await;
        ctx.token = resolved.token.clone();
        ctx.resolved = resolved;
        Ok(ctx)
    }

    /// A context rooted at a specific store.
    ///
    /// The credential store is *not* consulted here: the tests use this
    /// constructor, and a test run must never write to the developer's
    /// real credential vault.
    fn with_store(store: ConfigStore) -> Result<Self> {
        let config = store.load();
        Ok(Self {
            store,
            config,
            api: OnceCell::new(),
            api_base: None,
            credential_store: credentials::Store::FileOnly,
            token: None,
            resolved: credentials::Resolved::empty(),
        })
    }

    /// The effective token, with the standard "run `cfp key` first" help.
    fn token(&self) -> Result<String> {
        self.token
            .clone()
            .or_else(|| self.config.effective_token())
            .ok_or_else(|| anyhow!("no Cloudflare API token configured — run `cfp key -` first"))
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
        let api = self.api_for_token(&self.token()?)?;
        let _ = self.api.set(api);
        self.api
            .get()
            .ok_or_else(|| anyhow!("could not initialise the Cloudflare client"))
    }

    /// A client for one specific token.
    ///
    /// `cfp key` verifies the token it is about to save, which is not
    /// necessarily the one already in the config, so it cannot reuse the
    /// cached client.
    fn api_for_token(&self, token: &str) -> Result<CloudflareApi> {
        match &self.api_base {
            Some(base) => CloudflareApi::with_base(token, base.clone()).map_err(anyhow::Error::msg),
            None => CloudflareApi::new(token).map_err(anyhow::Error::msg),
        }
    }

    /// The account/zone/domain trio, for commands that operate on one zone.
    fn zone(&self) -> Result<config::TunnelContext> {
        self.config.require_tunnel_context()
    }
}

async fn run_tunnel_command(ctx: &Ctx, args: TunnelArgs) -> Result<()> {
    if ctx.token().is_err() {
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
        _ = handle.wait() => {
            // A tunnel that never came up must not exit 0. The session's
            // outcome is read from the handle — not from the event stream,
            // which would race the dispatch task.
            if ui::is_failed_outcome(handle.outcome()) {
                bail!("the tunnel could not be kept alive — see the error above");
            }
        }
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
        // The token may live in the credential store rather than in the
        // file — clearing the file alone would leave it behind.
        if ctx.credential_store.allows_system() {
            if let Err(err) = credentials::delete_token() {
                ui::note(&format!(
                    "Could not clear the OS credential store ({err}). \
                     The config file has been cleared."
                ));
            }
        }
        ui::success("All credentials cleared.");
        ui::key_status(&cleared, None);
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
            ui::key_status(&ctx.config, Some(&ctx.resolved));
            return Ok(());
        }
    };

    let trimmed = token.trim();
    if trimmed.is_empty() {
        bail!("empty token — pass a non-empty value");
    }

    ui::step("Verifying token with Cloudflare...");
    let api = ctx.api_for_token(trimmed)?;
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
    let saved = credentials::save_token(trimmed, ctx.credential_store);
    save_token_and_zone(&ctx.store, &mut config, trimmed, &zones[0], saved)?;

    if zones.len() == 1 {
        ui::success(&format!(
            "Saved to {}. Tunnels will be exposed under {}.",
            saved.describe(),
            config.domain.clone().unwrap()
        ));
        ui::key_status(&config, Some(&ctx.resolved));
    } else {
        ui::heading("Multiple zones are accessible:");
        ui::zones(&zones, config.domain.as_deref());
        ui::success(&format!(
            "Saved to {} (default domain: {}). Switch with `cfp domain <name>`.",
            saved.describe(),
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
    saved: credentials::Saved,
) -> Result<()> {
    // The token goes into the OS credential store when the machine has
    // one, so it is not left lying in a plaintext file. The file keeps a
    // copy only when the store cannot be read back — a store that
    // silently loses the secret is worse than a file that holds it.
    config.token = match saved {
        credentials::Saved::System => None,
        credentials::Saved::File | credentials::Saved::SystemWithFileCopy => {
            Some(token.to_string())
        }
    };
    store.save(config)?;

    config.apply_zone(zone).map_err(|err| {
        anyhow!("{err:#}\n\n  The token was saved — pick a zone with `cfp domain <name>`.")
    })?;
    store.save(config)?;
    Ok(())
}

/// Moves a token that is still in the config file into the OS credential
/// store.
///
/// Returns `true` when the config file no longer holds the token.
///
/// The move is verified before the file is rewritten: the token is
/// written to the store, read back, and only then removed from the
/// file. A store that refuses the secret — or accepts it and cannot
/// read it back — leaves the file untouched, so the user never loses
/// their token.
async fn migrate_legacy_token(
    store: &ConfigStore,
    config: &mut Config,
    credential_store: credentials::Store,
) -> bool {
    if !credential_store.allows_system() {
        return false;
    }
    let Some(file_token) = config.file_token() else {
        return false;
    };
    if !credentials::store_available() {
        return false;
    }

    let stored = credentials::read_token_within(credentials::STORE_BUDGET).await;
    let moved = match stored {
        // The credential store already holds a token: it is where `cfp
        // key` writes now, so it is the one the user chose most
        // recently. The file copy is redundant — and is exactly the
        // plaintext we are trying to get rid of.
        Some(_) => true,
        None => match credentials::write_token(&file_token) {
            Ok(()) => {
                credentials::read_token_within(credentials::STORE_BUDGET)
                    .await
                    .as_deref()
                    == Some(file_token.as_str())
            }
            Err(_) => false,
        },
    };
    if !moved {
        return false;
    }

    config.token = None;
    if let Err(err) = store.save(config) {
        ui::note(&format!(
            "Could not rewrite the config file ({err}); \
             the token is still in it as well as in the credential store."
        ));
        return false;
    }
    true
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
        let err = save_token_and_zone(
            &store,
            &mut config,
            "cf_test",
            &zone,
            credentials::Saved::File,
        )
        .expect_err("the zone resolution must fail");
        assert!(format!("{err:#}").contains("account"), "{err:#}");

        // The token survived, and the error says so.
        let saved = store.load();
        assert_eq!(saved.token.as_deref(), Some("cf_test"));
        assert!(format!("{err:#}").contains("token was saved"), "{err:#}");
    }

    /// A context pointed at a mock API, with its config file in a temp dir.
    async fn mock_ctx() -> (
        Ctx,
        tempfile::TempDir,
        std::sync::Arc<cloudpipe_test_support::MockState>,
    ) {
        let dir = tempfile::tempdir().expect("temp dir");
        let state = cloudpipe_test_support::MockState::new();
        let base = cloudpipe_test_support::spawn_mock_api(state.clone()).await;
        let store = ConfigStore::with_dir(dir.path().to_path_buf());
        let mut ctx = Ctx::with_store(store).expect("ctx");
        ctx.api_base = Some(base);
        // The zone-scoped commands need credentials in the config.
        ctx.config.token = Some("cf_test".to_string());
        ctx.config.domain = Some("example.com".to_string());
        ctx.config.zone_id = Some("zone-1".to_string());
        ctx.config.account_id = Some("acc-1".to_string());
        (ctx, dir, state)
    }

    #[tokio::test]
    async fn key_saves_the_token_even_when_the_zone_has_no_account() {
        let (ctx, _dir, state) = mock_ctx().await;
        // Cloudflare omits the account for some tokens, and `apply_zone`
        // then fails. The token must survive: re-running `cfp key` used
        // to produce the identical failure with nothing written.
        state
            .zones_without_account
            .store(true, std::sync::atomic::Ordering::SeqCst);

        let err = handle_key(&ctx, Some("cf_test".to_string()), None, false)
            .await
            .expect_err("the zone resolution must fail");
        assert!(format!("{err:#}").contains("account"), "{err:#}");

        let saved = ctx.store.load();
        assert_eq!(saved.token.as_deref(), Some("cf_test"));
    }

    #[tokio::test]
    async fn key_reports_an_empty_token() {
        let (ctx, _dir, _state) = mock_ctx().await;
        let err = handle_key(&ctx, Some("   ".to_string()), None, false)
            .await
            .expect_err("an empty token must be rejected");
        assert!(format!("{err:#}").contains("empty token"), "{err:#}");
    }

    #[tokio::test]
    async fn domain_reports_an_inaccessible_zone() {
        let (ctx, _dir, _state) = mock_ctx().await;
        // The mock only knows `example.com`.
        let err = handle_domain(&ctx, "nope.example.com".into())
            .await
            .expect_err("the zone is not accessible");
        assert!(format!("{err:#}").contains("not accessible"), "{err:#}");
    }

    #[tokio::test]
    async fn domain_switches_to_an_accessible_zone() {
        let (ctx, _dir, _state) = mock_ctx().await;
        handle_domain(&ctx, "example.com".into())
            .await
            .expect("the zone is accessible");

        let saved = ctx.store.load();
        assert_eq!(saved.domain.as_deref(), Some("example.com"));
        assert_eq!(saved.zone_id.as_deref(), Some("zone-1"));
        assert_eq!(saved.account_id.as_deref(), Some("acc-1"));
    }

    #[tokio::test]
    async fn cleanup_previews_without_deleting() {
        let (ctx, _dir, _state) = mock_ctx().await;
        handle_cleanup(
            &ctx,
            CleanupArgs {
                dry_run: true,
                json: true,
            },
        )
        .await
        .expect("the preview must succeed");
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
        save_token_and_zone(
            &store,
            &mut config,
            "cf_test",
            &zone,
            credentials::Saved::File,
        )
        .expect("the zone resolves");

        let saved = store.load();
        assert_eq!(saved.token.as_deref(), Some("cf_test"));
        assert_eq!(saved.domain.as_deref(), Some("example.com"));
        assert_eq!(saved.zone_id.as_deref(), Some("zone-1"));
        assert_eq!(saved.account_id.as_deref(), Some("account-1"));
    }
}
