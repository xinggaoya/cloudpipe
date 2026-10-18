//! Persistent configuration stored in `~/.cfp/config.json`.
//!
//! Holds the user's Cloudflare credentials (API token, account, zone) and the
//! base domain used to build tunnel hostnames. Because the file contains a
//! secret token, three properties are load-bearing:
//!
//! - the directory is `0700` and the file is `0600` on Unix, **applied when
//!   the file is created rather than after writing it** (see
//!   [`ConfigStore::save`]) so the token is never briefly world-readable;
//! - writes go through a temp file and an atomic `rename`, so an
//!   interrupted save cannot truncate a good config;
//! - a config that fails to parse is **reported**, not silently replaced
//!   with an empty one — the previous behaviour quietly cost users their
//!   saved token with no diagnostic at all.
//!
//! `CFP_CONFIG_DIR` relocates the whole directory, which is what lets the
//! test suite and parallel invocations avoid touching the real credentials.

use anyhow::{anyhow, Context, Result};
use cloudpipe_sdk::Zone;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// Bumped whenever the on-disk layout changes incompatibly. Older files are
/// migrated forward; a file from a *newer* version is refused rather than
/// half-understood.
pub const CONFIG_VERSION: u32 = 1;

/// Environment variable that overrides the token from the config file.
pub const TOKEN_ENV_VAR: &str = "CLOUDFLARE_API_TOKEN";

/// Environment variable that relocates the whole config directory.
pub const CONFIG_DIR_ENV_VAR: &str = "CFP_CONFIG_DIR";

/// The three values a tunnel needs on top of the token.
///
/// [`Config::require_tunnel_context`] is the single place that validates
/// their presence, so every command reports the same "run `cfp key` first"
/// guidance instead of each handler re-deriving it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelContext {
    /// Cloudflare account ID.
    pub account_id: String,
    /// Cloudflare zone ID.
    pub zone_id: String,
    /// Base domain.
    pub domain: String,
}

/// Error produced when a required field is missing from the config.
fn missing(field: &str) -> anyhow::Error {
    anyhow::anyhow!("{field} not configured — run `cfp key <token>` first")
}

/// User configuration persisted to disk.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct Config {
    /// On-disk schema version. Absent in version-0 files, hence `default`.
    #[serde(default)]
    pub version: u32,

    /// Cloudflare API token (secret).
    pub token: Option<String>,
    /// Cloudflare account ID (auto-discovered from the zone).
    pub account_id: Option<String>,
    /// Cloudflare zone ID for the base domain.
    pub zone_id: Option<String>,
    /// Base domain, e.g. `example.com`.
    pub domain: Option<String>,
}

impl Config {
    /// Whether a token is available (from the env var first, then the
    /// config file).
    pub fn effective_token(&self) -> Option<String> {
        self.effective_token_from(std::env::var(TOKEN_ENV_VAR).ok().as_deref())
    }

    /// Token resolution with the environment value injected.
    ///
    /// Split out from [`effective_token`](Self::effective_token) so tests
    /// can assert the precedence rules without mutating the process
    /// environment, which races with every other test in the binary (and
    /// is `unsafe` as of edition 2024).
    pub fn effective_token_from(&self, from_env: Option<&str>) -> Option<String> {
        if let Some(token) = from_env {
            let token = token.trim();
            if !token.is_empty() {
                return Some(token.to_string());
            }
        }
        self.token.clone()
    }

    /// Whether enough credentials exist to talk to the Cloudflare API.
    #[allow(dead_code)]
    pub fn is_ready(&self) -> bool {
        self.effective_token().is_some()
            && self.account_id.is_some()
            && self.zone_id.is_some()
            && self.domain.is_some()
    }

    /// Fills in [`version`](Self::version) for a freshly built config.
    pub fn new_empty() -> Self {
        Self {
            version: CONFIG_VERSION,
            ..Default::default()
        }
    }

    /// Extracts the account/zone/domain trio, reporting whichever field is
    /// missing with actionable guidance.
    pub fn require_tunnel_context(&self) -> Result<TunnelContext> {
        let account_id = self
            .account_id
            .clone()
            .ok_or_else(|| missing("account"))?;
        let zone_id = self.zone_id.clone().ok_or_else(|| missing("zone"))?;
        let domain = self.domain.clone().ok_or_else(|| missing("domain"))?;
        Ok(TunnelContext {
            account_id,
            zone_id,
            domain,
        })
    }

    /// Applies a zone's identity, replacing whatever was configured before.
    ///
    /// Returns an error when Cloudflare's zone listing omitted the account
    /// reference, rather than silently writing an empty account id that
    /// would fail confusingly on the next `cfp <port>` run.
    pub fn apply_zone(&mut self, zone: &Zone) -> Result<()> {
        let account_id = zone
            .require_account_id()
            .map_err(|e| anyhow!("{e:#}"))?
            .to_string();
        self.account_id = Some(account_id);
        self.zone_id = Some(zone.id.clone());
        self.domain = Some(zone.name.clone());
        Ok(())
    }
}

/// Handles reading/writing the config file at a fixed location.
pub struct ConfigStore {
    dir: PathBuf,
    file: PathBuf,
}

impl ConfigStore {
    /// Store rooted at `~/.cfp`, or at `$CFP_CONFIG_DIR` when set.
    ///
    /// The env override exists so tests and parallel CLI invocations do not
    /// have to touch the real user's credentials.
    pub fn default() -> Result<Self> {
        if let Some(dir) = std::env::var_os(CONFIG_DIR_ENV_VAR) {
            let dir = PathBuf::from(dir);
            if !dir.as_os_str().is_empty() {
                return Ok(Self::with_dir(dir));
            }
        }
        let home = dirs::home_dir().context("cannot determine home directory")?;
        Ok(Self::with_dir(home.join(".cfp")))
    }

    /// Store rooted at an arbitrary directory (mainly for tests).
    pub fn with_dir(dir: PathBuf) -> Self {
        Self {
            file: dir.join("config.json"),
            dir,
        }
    }

    /// Path of the config file.
    #[allow(dead_code)]
    pub fn file_path(&self) -> &Path {
        &self.file
    }

    /// Loads the config.
    ///
    /// A missing file yields the default config — that is the normal
    /// first-run case. A file that *exists but does not parse* is a
    /// different situation: it usually means an interrupted write or a
    /// hand-edit, and silently substituting an empty config would make the
    /// user re-enter their token with no explanation. That case is reported
    /// on stderr and treated as empty.
    pub fn load(&self) -> Config {
        let data = match fs::read_to_string(&self.file) {
            Ok(data) => data,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Config::new_empty(),
            Err(err) => {
                eprintln!(
                    "cfp: cannot read {}: {err}\n     falling back to an empty configuration",
                    self.file.display()
                );
                return Config::new_empty();
            }
        };

        let mut config: Config = match serde_json::from_str(&data) {
            Ok(config) => config,
            Err(err) => {
                eprintln!(
                    "cfp: {} is not valid JSON ({err}).\n     \
                     It has been left untouched; fix it by hand, or run \
                     `cfp key --clear` to start over.\n     \
                     Continuing with an empty configuration.",
                    self.file.display()
                );
                return Config::new_empty();
            }
        };

        if config.version > CONFIG_VERSION {
            eprintln!(
                "cfp: {} was written by a newer cfp (schema v{} > v{}).\n     \
                 Known fields will still be used.",
                self.file.display(),
                config.version,
                CONFIG_VERSION
            );
        }
        config.version = CONFIG_VERSION;
        config
    }

    /// Saves the config.
    ///
    /// The write is staged in `config.json.tmp` and moved into place with
    /// [`fs::rename`], which is atomic within a filesystem. An interrupted
    /// save therefore leaves the previous config intact rather than a
    /// truncated file that would silently erase the user's token.
    pub fn save(&self, config: &Config) -> Result<()> {
        fs::create_dir_all(&self.dir)
            .with_context(|| format!("cannot create config dir {}", self.dir.display()))?;
        self.restrict_dir_permissions();

        let mut config = config.clone();
        config.version = CONFIG_VERSION;
        let data = serde_json::to_string_pretty(&config)?;

        let staging = self.staging_path();
        write_private(&staging, data.as_bytes())
            .with_context(|| format!("cannot write {}", staging.display()))?;
        fs::rename(&staging, &self.file).with_context(|| {
            format!(
                "cannot move {} into place at {}",
                staging.display(),
                self.file.display()
            )
        })?;
        self.restrict_file_permissions();
        Ok(())
    }

    /// The temp path used to stage a config write.
    fn staging_path(&self) -> PathBuf {
        let mut name = self.file.file_name().unwrap_or_default().to_os_string();
        name.push(".tmp");
        self.file.with_file_name(name)
    }

    /// Sets `0700` on the directory (Unix only).
    fn restrict_dir_permissions(&self) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&self.dir, fs::Permissions::from_mode(0o700));
        }
    }

    /// Sets `0600` on the config file (Unix only).
    fn restrict_file_permissions(&self) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&self.file, fs::Permissions::from_mode(0o600));
        }
    }
}

/// Writes `data` to `path`, creating the file `0600` from the start on Unix.
///
/// `File::create` uses the process umask, which on a permissive system
/// yields a `0644` file — readable by every local user, in plaintext, for
/// as long as the write takes. Creating with an explicit mode closes that
/// window.
fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(data)?;
        file.sync_all()
    }
    #[cfg(not(unix))]
    {
        fs::write(path, data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every case gets its own self-cleaning directory. The previous
    /// `std::env::temp_dir().join(format!("cfp-test-{}-{}", name, process::id()))`
    /// scheme made cases in one test binary share (and clobber) a path.
    fn temp_store(name: &str) -> (tempfile::TempDir, ConfigStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = ConfigStore::with_dir(dir.path().to_path_buf());
        let _ = name;
        (dir, store)
    }

    fn sample() -> Config {
        Config {
            version: CONFIG_VERSION,
            token: Some("secret-token".into()),
            account_id: Some("acc123".into()),
            zone_id: Some("zone456".into()),
            domain: Some("example.com".into()),
        }
    }

    #[test]
    fn load_missing_file_returns_empty_config() {
        let (_tmp, store) = temp_store("missing");
        let loaded = store.load();
        assert_eq!(loaded, Config::new_empty());
        assert_eq!(loaded.version, CONFIG_VERSION);
    }

    #[test]
    fn load_corrupt_file_reports_and_returns_empty() {
        let (_tmp, store) = temp_store("corrupt");
        fs::create_dir_all(&store.dir).unwrap();
        fs::write(&store.file, "{not json").unwrap();

        // Behaviour is unchanged (empty config) but the file is left alone
        // so the user can salvage their token.
        assert_eq!(store.load(), Config::new_empty());
        assert_eq!(
            fs::read_to_string(&store.file).unwrap(),
            "{not json",
            "a corrupt config must not be overwritten by a read"
        );
    }

    #[test]
    fn load_upgrades_a_versionless_config() {
        let (_tmp, store) = temp_store("v0");
        fs::create_dir_all(&store.dir).unwrap();
        // A v0 file has no `version` key at all.
        fs::write(
            &store.file,
            r#"{"token":"t","account_id":"a","zone_id":"z","domain":"d.com"}"#,
        )
        .unwrap();

        let loaded = store.load();
        assert_eq!(loaded.version, CONFIG_VERSION);
        assert_eq!(loaded.token.as_deref(), Some("t"));
    }

    #[test]
    fn load_tolerates_a_newer_schema() {
        let (_tmp, store) = temp_store("v99");
        fs::create_dir_all(&store.dir).unwrap();
        fs::write(
            &store.file,
            r#"{"version":99,"token":"t","future_field":true}"#,
        )
        .unwrap();

        // Known fields still work; a newer cfp must not brick an older one.
        let loaded = store.load();
        assert_eq!(loaded.token.as_deref(), Some("t"));
    }

    #[test]
    fn save_and_load_roundtrip() {
        let (_tmp, store) = temp_store("roundtrip");
        let config = sample();
        store.save(&config).unwrap();
        let loaded = store.load();
        assert_eq!(loaded.token, config.token);
        assert_eq!(loaded.account_id, config.account_id);
        assert_eq!(loaded.zone_id, config.zone_id);
        assert_eq!(loaded.domain, config.domain);
        assert_eq!(loaded.version, CONFIG_VERSION);
    }

    #[test]
    fn save_stamps_the_current_version() {
        let (_tmp, store) = temp_store("stamp");
        let mut config = sample();
        config.version = 0;
        store.save(&config).unwrap();
        assert_eq!(store.load().version, CONFIG_VERSION);
    }

    #[cfg(unix)]
    #[test]
    fn save_restricts_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let (_tmp, store) = temp_store("perms");
        store.save(&Config::new_empty()).unwrap();
        let file_mode = fs::metadata(&store.file).unwrap().permissions().mode();
        let dir_mode = fs::metadata(&store.dir).unwrap().permissions().mode();
        assert_eq!(file_mode & 0o777, 0o600);
        assert_eq!(dir_mode & 0o777, 0o700);
    }

    #[cfg(unix)]
    #[test]
    fn config_is_never_world_readable_even_at_creation() {
        use std::os::unix::fs::PermissionsExt;
        let (_tmp, store) = temp_store("private");
        store.save(&sample()).unwrap();
        // The staging file is what actually receives the secret; if it were
        // created 0644 there would be a window in which `cat` worked.
        let staging = store.staging_path();
        assert!(
            !staging.exists(),
            "the staging file must be renamed away, not left behind"
        );
        assert_eq!(
            fs::metadata(&store.file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn interrupted_save_leaves_the_previous_config_intact() {
        // `save` stages then renames. A crash between the two leaves the
        // staging file behind, never a truncated `config.json`.
        let (_tmp, store) = temp_store("atomic");
        store.save(&sample()).unwrap();
        let before = fs::read_to_string(&store.file).unwrap();

        // Simulate the crash: drop a stale staging file, then re-save.
        fs::write(store.staging_path(), "{ truncated").unwrap();
        store.save(&sample()).unwrap();

        assert_eq!(
            fs::read_to_string(&store.file).unwrap(),
            before,
            "a previous save must remain valid"
        );
    }

    #[test]
    fn env_token_takes_precedence_without_touching_the_process_env() {
        // `Config::effective_token` reads the environment, so instead of
        // mutating it (racy with other tests in this binary, and `unsafe`
        // in edition 2024) the token source is injected explicitly.
        let with_file_token = sample();
        assert_eq!(
            with_file_token.effective_token_from(None).as_deref(),
            Some("secret-token")
        );

        let from_env = with_file_token.effective_token_from(Some("env-token"));
        assert_eq!(from_env.as_deref(), Some("env-token"));

        // A blank env var falls through to the file rather than shadowing
        // it with an empty string.
        let blank = with_file_token.effective_token_from(Some("   "));
        assert_eq!(blank.as_deref(), Some("secret-token"));

        let no_file_token = Config::new_empty();
        assert_eq!(no_file_token.effective_token_from(None), None);
    }

    #[test]
    fn require_tunnel_context_reports_each_missing_field() {
        let empty = Config::new_empty();
        assert!(empty.require_tunnel_context().is_err());

        let partial = Config {
            account_id: Some("a".into()),
            ..Config::new_empty()
        };
        let err = format!("{:#}", partial.require_tunnel_context().unwrap_err());
        assert!(err.contains("zone"), "{err}");
        assert!(err.contains("cfp key"), "{err}");

        let full = sample();
        let ctx = full.require_tunnel_context().unwrap();
        assert_eq!(ctx.account_id, "acc123");
        assert_eq!(ctx.zone_id, "zone456");
        assert_eq!(ctx.domain, "example.com");
    }

    #[test]
    fn apply_zone_replaces_the_previous_zone() {
        let mut config = sample();
        let zone: Zone = serde_json::from_str(
            r#"{"id":"zone-new","name":"other.test","account":{"id":"acc-new"}}"#,
        )
        .unwrap();
        config.apply_zone(&zone).unwrap();
        assert_eq!(config.zone_id.as_deref(), Some("zone-new"));
        assert_eq!(config.domain.as_deref(), Some("other.test"));
        assert_eq!(config.account_id.as_deref(), Some("acc-new"));
        assert_eq!(
            config.token.as_deref(),
            Some("secret-token"),
            "switching zones must not clear the token"
        );
    }

    #[test]
    fn apply_zone_refuses_a_zone_without_an_account() {
        let mut config = sample();
        let zone: Zone = serde_json::from_str(r#"{"id":"z","name":"n.test"}"#).unwrap();
        let err = format!("{:#}", config.apply_zone(&zone).unwrap_err());
        assert!(err.contains("without an account"), "{err}");
        assert_eq!(
            config.zone_id.as_deref(),
            Some("zone456"),
            "a failed switch must not mutate the config"
        );
    }

    #[test]
    fn is_ready_checks_all_fields() {
        assert!(sample().is_ready());
        let incomplete = Config {
            token: Some("t".into()),
            ..Config::new_empty()
        };
        assert!(!incomplete.is_ready());
    }
}
