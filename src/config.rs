//! Persistent configuration stored in `~/.cfp/config.json`.
//!
//! Holds the user's Cloudflare credentials (API token, account, zone) and the
//! base domain used to build tunnel hostnames. The config directory is created
//! with `0700` and the file with `0600` permissions on Unix, since it contains
//! a secret token.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// Environment variable that overrides the token from the config file.
pub const TOKEN_ENV_VAR: &str = "CLOUDFLARE_API_TOKEN";

/// User configuration persisted to disk.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct Config {
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
    /// Whether a token is available (from env var first, then config file).
    pub fn effective_token(&self) -> Option<String> {
        if let Ok(token) = std::env::var(TOKEN_ENV_VAR) {
            let token = token.trim().to_string();
            if !token.is_empty() {
                return Some(token);
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
}

/// Handles reading/writing the config file at a fixed location.
pub struct ConfigStore {
    dir: PathBuf,
    file: PathBuf,
}

impl ConfigStore {
    /// Store rooted at `~/.cfp`.
    pub fn default() -> Result<Self> {
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

    /// Loads the config. Missing or corrupt files yield the default config.
    pub fn load(&self) -> Config {
        let data = match fs::read_to_string(&self.file) {
            Ok(data) => data,
            Err(_) => return Config::default(),
        };
        serde_json::from_str(&data).unwrap_or_default()
    }

    /// Saves the config, creating the directory with restrictive permissions.
    pub fn save(&self, config: &Config) -> Result<()> {
        fs::create_dir_all(&self.dir)
            .with_context(|| format!("cannot create config dir {}", self.dir.display()))?;
        let data = serde_json::to_string_pretty(config)?;
        fs::write(&self.file, data)
            .with_context(|| format!("cannot write config file {}", self.file.display()))?;
        self.restrict_permissions();
        Ok(())
    }

    /// Sets `0700` on the directory and `0600` on the file (Unix only).
    fn restrict_permissions(&self) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&self.dir, fs::Permissions::from_mode(0o700));
            let _ = fs::set_permissions(&self.file, fs::Permissions::from_mode(0o600));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store(name: &str) -> ConfigStore {
        let dir = std::env::temp_dir().join(format!(
            "cfp-test-{}-{}",
            name,
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        ConfigStore::with_dir(dir)
    }

    #[test]
    fn load_missing_file_returns_default() {
        let store = temp_store("missing");
        assert_eq!(store.load(), Config::default());
    }

    #[test]
    fn load_corrupt_file_returns_default() {
        let store = temp_store("corrupt");
        fs::create_dir_all(&store.dir).unwrap();
        fs::write(&store.file, "{not json").unwrap();
        assert_eq!(store.load(), Config::default());
    }

    #[test]
    fn save_and_load_roundtrip() {
        let store = temp_store("roundtrip");
        let config = Config {
            token: Some("secret-token".into()),
            account_id: Some("acc123".into()),
            zone_id: Some("zone456".into()),
            domain: Some("example.com".into()),
        };
        store.save(&config).unwrap();
        assert_eq!(store.load(), config);
    }

    #[cfg(unix)]
    #[test]
    fn save_restricts_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let store = temp_store("perms");
        store.save(&Config::default()).unwrap();
        let file_mode = fs::metadata(&store.file).unwrap().permissions().mode();
        let dir_mode = fs::metadata(&store.dir).unwrap().permissions().mode();
        assert_eq!(file_mode & 0o777, 0o600);
        assert_eq!(dir_mode & 0o777, 0o700);
    }

    #[test]
    fn env_token_takes_precedence() {
        let store = temp_store("env-token");
        let mut config = Config {
            token: Some("file-token".into()),
            ..Default::default()
        };
        store.save(&config).unwrap();

        let loaded = store.load();
        assert_eq!(loaded.effective_token().as_deref(), Some("file-token"));

        std::env::set_var(TOKEN_ENV_VAR, "env-token");
        let loaded = store.load();
        assert_eq!(loaded.effective_token().as_deref(), Some("env-token"));
        std::env::remove_var(TOKEN_ENV_VAR);

        config.token = None;
        assert_eq!(config.effective_token(), None);
    }

    #[test]
    fn is_ready_checks_all_fields() {
        let ready = Config {
            token: Some("t".into()),
            account_id: Some("a".into()),
            zone_id: Some("z".into()),
            domain: Some("d.com".into()),
        };
        assert!(ready.is_ready());

        let incomplete = Config {
            token: Some("t".into()),
            ..Default::default()
        };
        assert!(!incomplete.is_ready());
    }
}
