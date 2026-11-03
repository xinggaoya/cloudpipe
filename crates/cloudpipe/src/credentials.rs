//! Where the Cloudflare API token is kept.
//!
//! # The order of preference
//!
//! 1. **The environment** — `CLOUDFLARE_API_TOKEN`. Nothing is written
//!    to disk at all, and any other tool on the machine can be pointed
//!    at the same value.
//! 2. **The OS credential store** — Windows Credential Manager, macOS
//!    Keychain, Linux Secret Service. The OS encrypts the secret at
//!    rest and binds it to the user's login, so it is not readable by
//!    another local user and it never sits in a plaintext file.
//! 3. **The config file** — `~/.cfp/config.json`, `0600` on Unix. The
//!    fallback for a machine with no credential store at all: a headless
//!    server, a container, a locked-down desktop.
//!
//! # Migrating
//!
//! A token that is still sitting in the config file is moved into the
//! credential store the first time `cfp` runs — but only after the
//! store has been read back and the value verified, so a store that
//! silently refuses to keep the secret never costs the user their
//! token.

use std::time::Duration;

use crate::config::Config;

/// The service name the credential is filed under.
pub const SERVICE: &str = "cfp";

/// The account name within the service: the Cloudflare API token.
pub const ACCOUNT: &str = "cloudflare-api-token";

/// How long the OS credential store gets to answer before `cfp` falls
/// back to the config file.
///
/// The Windows Credential Manager answers instantly. The Linux Secret
/// Service call is a D-Bus round trip that can block for a long time on
/// a machine with no session bus, and a CLI that hangs at startup is
/// worse than one that falls back to the file.
pub const STORE_BUDGET: Duration = Duration::from_secs(3);

/// Where a token came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Location {
    /// The `CLOUDFLARE_API_TOKEN` environment variable.
    Environment,
    /// The OS credential store.
    System,
    /// The config file.
    File,
}

/// Where the token is allowed to be stored.
///
/// The distinction matters for two reasons: a machine with no
/// credential store has to fall back to the file, and a test run must
/// never write to the developer's real vault.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Store {
    /// The OS credential store when the machine has one, the config file
    /// otherwise.
    Auto,
    /// The config file only — the OS credential store is never touched.
    FileOnly,
}

impl Store {
    /// Whether the OS credential store may be used.
    pub(crate) fn allows_system(self) -> bool {
        matches!(self, Store::Auto)
    }
}

impl Location {
    /// A human-readable name, for the CLI's status output.
    pub fn describe(self) -> &'static str {
        match self {
            Location::Environment => "the CLOUDFLARE_API_TOKEN environment variable",
            Location::System => "the OS credential store",
            Location::File => "the config file (~/.cfp/config.json)",
        }
    }
}

/// A resolved token and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// The token itself.
    pub token: Option<String>,
    /// Where it came from.
    pub location: Option<Location>,
}

impl Resolved {
    /// A resolution with no token.
    pub fn empty() -> Self {
        Self {
            token: None,
            location: None,
        }
    }
}

/// Resolves the token: the environment first, then the OS credential
/// store, then the config file.
pub async fn resolve_token(config: &Config, store: Store) -> Resolved {
    // The environment wins, and only the environment: a token in the
    // config file must not shadow it, and a blank variable is no token
    // at all rather than an empty one.
    let from_env = std::env::var(crate::config::TOKEN_ENV_VAR)
        .ok()
        .map(|token| token.trim().to_string())
        .filter(|token| !token.is_empty());
    if let Some(token) = from_env {
        return Resolved {
            token: Some(token),
            location: Some(Location::Environment),
        };
    }
    if store.allows_system() {
        if let Some(token) = read_token_within(STORE_BUDGET).await {
            return Resolved {
                token: Some(token),
                location: Some(Location::System),
            };
        }
    }
    match config.file_token() {
        Some(token) => Resolved {
            token: Some(token),
            location: Some(Location::File),
        },
        None => Resolved::empty(),
    }
}

/// Whether this machine has a credential store `cfp` can use.
pub fn store_available() -> bool {
    keyring::Entry::store_status().is_ok()
}

/// Reads the token from the OS credential store.
///
/// Blocking: the Windows Credential Manager answers instantly, but the
/// Linux Secret Service call is a D-Bus round trip. Callers on an async
/// runtime should use [`read_token_within`].
pub fn read_token() -> Option<String> {
    let entry = keyring::Entry::new(SERVICE, ACCOUNT).ok()?;
    match entry.get_password() {
        Ok(token) if !token.trim().is_empty() => Some(token),
        // An empty entry is no entry.
        Ok(_) => None,
        Err(_) => None,
    }
}

/// Reads the token from the OS credential store, giving up after
/// `budget`.
///
/// A store that cannot be reached is indistinguishable from a store that
/// holds nothing, as far as the caller is concerned: the config file is
/// the fallback either way.
pub async fn read_token_within(budget: Duration) -> Option<String> {
    match tokio::time::timeout(budget, tokio::task::spawn_blocking(read_token)).await {
        Ok(Ok(token)) => token,
        _ => None,
    }
}

/// Writes the token into the OS credential store.
pub fn write_token(token: &str) -> Result<(), String> {
    let entry = keyring::Entry::new(SERVICE, ACCOUNT).map_err(|err| err.to_string())?;
    entry.set_password(token).map_err(|err| err.to_string())
}

/// Removes the token from the OS credential store.
pub fn delete_token() -> Result<(), String> {
    let entry = keyring::Entry::new(SERVICE, ACCOUNT).map_err(|err| err.to_string())?;
    match entry.delete_credential() {
        Ok(()) => Ok(()),
        // "There was nothing to delete" is the state we wanted.
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(err) => Err(err.to_string()),
    }
}

/// Where a token was put.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Saved {
    /// The OS credential store, and the config file no longer holds the
    /// token.
    System,
    /// The config file — no usable credential store on this machine.
    File,
    /// The OS credential store, but it could not be read back, so a copy
    /// was left in the config file as well.
    SystemWithFileCopy,
}

impl Saved {
    /// A human-readable name for the CLI's confirmation.
    pub fn describe(self) -> &'static str {
        match self {
            Saved::System => "the OS credential store",
            Saved::File => "the config file (~/.cfp/config.json)",
            Saved::SystemWithFileCopy => {
                "the OS credential store (a copy was kept in the config file as a fallback)"
            }
        }
    }
}

/// Stores `token` in the best place available.
///
/// The credential store is preferred; the config file is the fallback.
/// When the store accepts the value but cannot read it back, the file
/// copy is kept — a store that quietly loses the secret is worse than a
/// file that holds it.
pub fn save_token(token: &str, store: Store) -> Saved {
    if !store.allows_system() {
        return Saved::File;
    }
    match write_token(token) {
        Ok(()) => match read_token() {
            Some(stored) if stored == token => Saved::System,
            _ => Saved::SystemWithFileCopy,
        },
        Err(_) => Saved::File,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_location_names_are_stable() {
        // The CLI prints these; a rename would be a user-visible change.
        assert_eq!(
            Location::Environment.describe(),
            "the CLOUDFLARE_API_TOKEN environment variable"
        );
        assert_eq!(Location::System.describe(), "the OS credential store");
        assert!(Location::File.describe().contains("config.json"));
    }

    #[test]
    fn the_service_and_account_names_are_stable() {
        // Changing these would orphan every credential already stored.
        assert_eq!(SERVICE, "cfp");
        assert_eq!(ACCOUNT, "cloudflare-api-token");
    }

    #[test]
    fn an_empty_resolution_has_no_location() {
        let resolved = Resolved::empty();
        assert!(resolved.token.is_none());
        assert!(resolved.location.is_none());
    }

    #[test]
    fn the_file_only_store_never_touches_the_os_store() {
        // The test suite runs with this policy: a test must not write to
        // the developer's real credential vault.
        assert!(!Store::FileOnly.allows_system());
        assert!(Store::Auto.allows_system());
        assert_eq!(save_token("secret", Store::FileOnly), Saved::File);
    }
}
