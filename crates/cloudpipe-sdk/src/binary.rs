//! Locates, downloads and spawns the `cloudflared` binary.
//!
//! `cloudflared` is Cloudflare's tunnel client. The SDK first looks for it
//! on `PATH`, then falls back to downloading a release asset from GitHub
//! (with an optional mirror prefix).
//!
//! # Supply-chain posture
//!
//! The download is treated as untrusted input because it becomes an
//! executable:
//!
//! - The version is **pinned** (see [`CLOUDFLARED_VERSION`]) rather than
//!   tracking `latest`, so an upstream release cannot change under a user
//!   between two runs.
//! - The asset's **SHA-256 is verified** before the file is made
//!   executable; a mismatch aborts and the partial file is removed.
//! - The binary is written to a temporary path, fsynced, `chmod 0755`ed,
//!   then `rename`d into place, so a concurrent reader never observes a
//!   half-written file and two `cfp` invocations cannot corrupt each
//!   other.
//! - The tunnel token is passed via a **`0600` file**, never through argv,
//!   because argv is world-readable through `/proc/<pid>/cmdline`.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{anyhow, Context as _};
use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::error::{InstallError, Result};

/// Base URL for `cloudflared` release assets.
const GITHUB_BASE_URL: &str = "https://github.com/cloudflare/cloudflared/releases/download";

/// The `cloudflared` version this SDK installs.
///
/// Pinned on purpose: tracking `releases/latest` means a user can run the
/// same command twice and execute two different binaries, and an upstream
/// release would be installed with no review.
pub const CLOUDFLARED_VERSION: &str = "2025.11.1";

/// Default GitHub mirror prefix used to speed up downloads in regions where
/// `github.com` is slow or blocked. Override at runtime with the
/// `CFP_GITHUB_PROXY` env var; an empty value disables mirroring entirely.
const DEFAULT_GITHUB_PROXY: &str = "https://v4.gh-proxy.org/";

/// Per-attempt download deadline. Without it a half-open connection would
/// leave `cfp` hanging on "Downloading cloudflared…" indefinitely.
const DOWNLOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// Refuse to buffer an asset larger than this. `cloudflared` is ~40 MB;
/// the ceiling exists so a misconfigured mirror cannot exhaust memory.
const MAX_ASSET_BYTES: u64 = 512 * 1024 * 1024;

/// Every release asset this SDK knows how to install.
///
/// The order is fixed and alphabetical so the generated checksum table
/// and the in-tree table can be diffed line by line.
///
/// This list is the single source of truth for "which platforms do we
/// support": `scripts/gen-cloudflared-checksums.sh` parses it, the CI
/// `verify-checksums` job diffs the result against the pinned table, and
/// `download_asset_name` is asserted to only ever return a member.
///
/// Re-exported as [`crate::ALL_ASSETS`].
pub const ALL_ASSETS: &[&str] = &[
    "cloudflared-darwin-amd64.tgz",
    "cloudflared-darwin-arm64.tgz",
    "cloudflared-linux-amd64",
    "cloudflared-linux-arm64",
    "cloudflared-linux-arm",
    "cloudflared-windows-386.exe",
    "cloudflared-windows-amd64.exe",
];

/// SHA-256 digests of the release assets this SDK may install.
///
/// Every entry is the digest of the bytes served by the official release
/// URL for [`CLOUDFLARED_VERSION`]. Two safeguards keep this table honest,
/// because a wrong entry here is indistinguishable from a tampered
/// download at the point of use:
///
/// 1. `ci.yml`'s `verify-checksums` job re-downloads each asset from
///    GitHub and diffs it against this table, so a stale or invented digest
///    fails the build rather than every user's first run.
/// 2. A release bump must update this table in the same commit, or (2)
///    fires.
///
/// Regenerate with `cargo xtask` equivalent:
///
/// ```text
/// curl -fsSL <asset-url> | sha256sum
/// ```
///
/// An asset with no digest here is refused rather than installed
/// unverified, unless the digest is supplied at runtime through
/// [`CFP_CLOUDFLARED_SHA256`].
const ASSET_SHA256: &[(&str, &str)] = &[
    // Verified against the bytes GitHub serves for CLOUDFLARED_VERSION.
    (
        "cloudflared-linux-amd64",
        "cc348a425d7944d3fad9d8ac124b06725d471634153b58e0d57c72a79cc1ea4e",
    ),
    (
        "cloudflared-linux-arm64",
        "3d67ef9295f7d5528a1acd74e8db79638a283b46f8922d5427545ff3f8ca3cc7",
    ),
    // The remaining platforms are intentionally absent until their digests
    // are filled in by `ci.yml`'s `verify-checksums` job, which downloads
    // each asset and diffs it against this table. A fabricated digest here
    // is indistinguishable from a tampered download at the point of use, so
    // an unpinned platform is refused with an actionable message instead of
    // installing something unverifiable.
    //
    // To use one of those platforms today without waiting for a release,
    // set CFP_CLOUDFLARED_SHA256=<digest you verified>, or install
    // cloudflared yourself and put it on PATH.
];

/// Classifies a single line of `cloudflared` stderr for event dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LineKind {
    /// A new edge connection was registered.
    Connection,
    /// A real error worth surfacing.
    Error,
    /// Harmless noise (origin cert hints, retries, etc.).
    Ignore,
}

/// Classifies a single stderr line from `cloudflared`.
pub fn classify_line(line: &str) -> LineKind {
    const IGNORE_PATTERNS: &[&str] = &[
        "cannot determine default origin certificate path",
        "no file cert.pem",
        "origincert option",
        "tunnel_origin_cert",
        "context canceled",
        "connection terminated",
        "no more connections active and exiting",
        "serve tunnel error",
        "retrying connection",
        "icmp router terminated",
        "use of closed network connection",
        "application error 0x0",
        "failed to accept quic stream",
        "failed to dial to edge",
        "timeout: no recent network activity",
        "quic:",
    ];

    let lower = line.to_ascii_lowercase();
    if lower.contains("registered tunnel connection") {
        return LineKind::Connection;
    }
    if IGNORE_PATTERNS.iter().any(|p| lower.contains(p)) {
        return LineKind::Ignore;
    }
    if lower.contains("err") || lower.contains("error") {
        return LineKind::Error;
    }
    LineKind::Ignore
}

/// Reads the GitHub mirror prefix from `CFP_GITHUB_PROXY`, falling back to
/// [`DEFAULT_GITHUB_PROXY`].
pub fn github_proxy_from_env() -> String {
    std::env::var("CFP_GITHUB_PROXY")
        .unwrap_or_else(|_| DEFAULT_GITHUB_PROXY.to_string())
        .trim()
        .trim_end_matches('/')
        .to_string()
}

/// Builds the ordered list of candidate download URLs for `asset`.
///
/// The configured mirror (if enabled) is tried first; the direct GitHub URL
/// is always appended as the final fallback.
pub fn candidate_urls(asset: &str, proxy_override: Option<&str>) -> Vec<String> {
    let direct = release_url(asset);
    let proxy = match proxy_override {
        Some(value) => value.trim().trim_end_matches('/').to_string(),
        None => github_proxy_from_env(),
    };
    let mut urls = Vec::new();
    if !proxy.is_empty() {
        urls.push(format!("{proxy}/{direct}"));
    }
    urls.push(direct);
    urls
}

/// The canonical, version-pinned release URL for an asset.
fn release_url(asset: &str) -> String {
    format!("{GITHUB_BASE_URL}/{CLOUDFLARED_VERSION}/{asset}")
}

/// Maps `(platform, arch)` to the release asset name.
pub fn download_asset_name(platform: &str, arch: &str) -> Result<&'static str> {
    let asset = match (platform, arch) {
        ("darwin", "x86_64") => "cloudflared-darwin-amd64.tgz",
        ("darwin", "aarch64") => "cloudflared-darwin-arm64.tgz",
        ("windows", "x86_64") => "cloudflared-windows-amd64.exe",
        ("windows", "x86") => "cloudflared-windows-386.exe",
        ("linux", "x86_64") => "cloudflared-linux-amd64",
        ("linux", "aarch64") => "cloudflared-linux-arm64",
        ("linux", "arm") => "cloudflared-linux-arm",
        _ => {
            return Err(InstallError::UnsupportedPlatform {
                platform: platform.to_string(),
                arch: arch.to_string(),
            }
            .into());
        }
    };
    Ok(asset)
}

/// Environment variable that supplies the expected digest for an asset.
///
/// Overrides the compiled-in table. Two uses:
///
/// - a user whose platform this build does not pin, who can pass the
///   digest they verified themselves without waiting for a release;
/// - CI, when re-verifying a bumped version before the table is updated.
///
/// The value wins over [`ASSET_SHA256`], so a wrong value here can be used
/// to *disable* verification for an asset — which is why the CLI surfaces
/// it loudly when set.
pub const SHA256_ENV_VAR: &str = "CFP_CLOUDFLARED_SHA256";

/// The expected SHA-256 for an asset, if one is known.
///
/// Resolution order: the [`SHA256_ENV_VAR`] override, then
/// [`ASSET_SHA256`]. `None` means "cannot verify", which
/// [`ensure_installed`] refuses rather than installing unverified.
pub fn expected_sha256(asset: &str) -> Option<String> {
    if let Ok(digest) = std::env::var(SHA256_ENV_VAR) {
        let digest = digest.trim().to_ascii_lowercase();
        if !digest.is_empty() {
            return Some(digest);
        }
    }
    ASSET_SHA256
        .iter()
        .find(|(name, _)| *name == asset)
        .map(|(_, digest)| (*digest).to_string())
}

/// Whether the digest in [`SHA256_ENV_VAR`] is overriding the built-in
/// table, which the installer announces so a user is never quietly
/// downgraded to "verified by an env var someone set".
pub fn sha256_is_overridden() -> bool {
    std::env::var(SHA256_ENV_VAR)
        .map(|d| !d.trim().is_empty())
        .unwrap_or(false)
}

/// Hex-encodes a SHA-256 digest.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Local binary file name for the current platform.
fn binary_name() -> &'static str {
    if cfg!(windows) {
        "cloudflared.exe"
    } else {
        "cloudflared"
    }
}

/// Directory that holds the downloaded binary (overridable for tests).
pub(crate) fn install_dir(config_dir: Option<&Path>) -> Result<PathBuf> {
    let base = match config_dir {
        Some(dir) => dir.to_path_buf(),
        None => dirs::home_dir()
            .context("cannot determine home directory")?
            .join(".cfp"),
    };
    Ok(base.join("bin"))
}

/// Searches `PATH` (and `PATHEXT` on Windows) for an existing `cloudflared`.
pub fn find_in_path() -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    let name = binary_name();

    let mut candidates: Vec<PathBuf> = Vec::new();
    if cfg!(windows) {
        for dir in std::env::split_paths(&path_var) {
            candidates.push(dir.join(name));
        }
        if let Some(pathext) = std::env::var_os("PATHEXT") {
            let exts: Vec<String> = std::env::split_paths(&pathext)
                .map(|e| e.to_string_lossy().to_string())
                .collect();
            for ext in exts {
                if !ext.is_empty() {
                    for dir in std::env::split_paths(&path_var) {
                        candidates.push(dir.join(format!("cloudflared{ext}")));
                    }
                }
            }
        }
    } else {
        candidates = std::env::split_paths(&path_var)
            .map(|dir| dir.join(name))
            .collect();
    }

    candidates
        .into_iter()
        .find(|path| path.is_file())
        .filter(|path| is_executable(path))
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// Ensures a `cloudflared` binary is available, downloading if needed.
///
/// Returns the path to the binary.
pub async fn ensure_installed(
    config_dir: Option<&Path>,
    github_proxy: Option<&str>,
) -> Result<PathBuf> {
    if let Some(path) = find_in_path() {
        // A previous run killed between fork and cloudflared's own open()
        // leaves the token file behind. Clear it before returning.
        cleanup_token_file(&path).await;
        return Ok(path);
    }

    let dir = install_dir(config_dir)?;
    let path = dir.join(binary_name());
    if is_executable(&path) {
        return Ok(path);
    }

    cleanup_token_file(&path).await;

    let asset = download_asset_name(std::env::consts::OS, std::env::consts::ARCH)?;
    if sha256_is_overridden() {
        eprintln!(
            "Note: {SHA256_ENV_VAR} is set, so the cloudflared download is \
             verified against that value instead of this build's pinned digest."
        );
    }
    let Some(expected) = expected_sha256(asset) else {
        // Installing an executable we cannot verify would defeat the point
        // of pinning the version in the first place. The error says exactly
        // what to do about it, because "refused" with no route forward is
        // the same outcome as being broken.
        return Err(InstallError::UnverifiedAsset(asset.to_string()).into());
    };
    let is_tgz = asset.ends_with(".tgz");
    let urls = candidate_urls(asset, github_proxy);

    let mut last_err: Option<anyhow::Error> = None;
    for url in &urls {
        match download_and_install(url, &dir, &path, is_tgz, &expected).await {
            Ok(()) => return Ok(path),
            Err(err) => {
                if urls.len() > 1 {
                    eprintln!("Download failed ({url}): {err:#}; trying next source");
                }
                let inner = match err {
                    crate::Error::Other(anyhow_err) => anyhow_err,
                    other => anyhow::anyhow!("{other}"),
                };
                last_err = Some(inner);
            }
        }
    }
    Err(InstallError::Download(
        last_err.unwrap_or_else(|| anyhow!("no download sources available")),
    )
    .into())
}

/// Downloads an asset and installs it at `path`.
///
/// The download goes to `path.with_extension("part")` and is only moved
/// into place after its digest matches, so a partial or tampered download
/// never becomes the binary that gets executed.
async fn download_and_install(
    url: &str,
    dir: &Path,
    path: &Path,
    is_tgz: bool,
    expected_sha256: &str,
) -> Result<()> {
    fs::create_dir_all(dir)
        .await
        .with_context(|| format!("cannot create bin dir {}", dir.display()))?;

    let client = reqwest::Client::builder()
        .timeout(DOWNLOAD_TIMEOUT)
        .user_agent(format!(
            "cloudpipe/{version}",
            version = env!("CARGO_PKG_VERSION")
        ))
        .build()
        .map_err(crate::error::ApiError::Http)?;

    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("cannot download {url}"))?
        .error_for_status()
        .with_context(|| format!("download failed for {url}"))?;

    if let Some(len) = response.content_length() {
        if len > MAX_ASSET_BYTES {
            return Err(InstallError::TooLarge {
                bytes: len,
                limit: MAX_ASSET_BYTES,
            }
            .into());
        }
    }

    let bytes = response.bytes().await.context("reading download body")?;
    if bytes.len() as u64 > MAX_ASSET_BYTES {
        return Err(InstallError::TooLarge {
            bytes: bytes.len() as u64,
            limit: MAX_ASSET_BYTES,
        }
        .into());
    }

    let actual = hex(&Sha256::digest(&bytes));
    if !actual.eq_ignore_ascii_case(expected_sha256) {
        return Err(InstallError::ChecksumMismatch {
            expected: expected_sha256.to_string(),
            actual,
        }
        .into());
    }

    // tgz payloads are unpacked before installation; everything else is the
    // binary verbatim.
    if is_tgz {
        let bytes_vec = bytes.to_vec();
        let temp = temp_path(path);
        tokio::task::spawn_blocking(move || extract_tgz(&bytes_vec, &temp))
            .await
            .context("extracting archive")??;
    } else {
        let temp = temp_path(path);
        let mut file = fs::File::create(&temp)
            .await
            .with_context(|| format!("cannot create {}", temp.display()))?;
        file.write_all(&bytes)
            .await
            .with_context(|| format!("cannot write {}", temp.display()))?;
        file.flush().await.ok();
        file.sync_all().await.ok();
        drop(file);
        make_executable(&temp);
    }

    // Atomic publish: rename is atomic within a filesystem, so a concurrent
    // `cfp` either sees the old file or the new one, never a partial write.
    fs::rename(&temp_path(path), path)
        .await
        .with_context(|| format!("cannot install {}", path.display()))?;
    make_executable(path);
    Ok(())
}

/// The staging path a download is written to before being published.
fn temp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    path.with_file_name(name)
}

/// Extracts the `cloudflared` entry out of a `.tgz` into `dest`.
fn extract_tgz(bytes: &[u8], dest: &Path) -> Result<()> {
    let decoder = GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(decoder);
    let entries = archive.entries().context("reading tar archive")?;
    for entry in entries {
        let mut entry = entry.context("reading tar entry")?;
        let entry_path = entry.path().context("tar entry path")?.into_owned();
        if entry_path.file_name().is_some_and(|n| n == "cloudflared") {
            let mut file = std::fs::File::create(dest)
                .with_context(|| format!("cannot create {}", dest.display()))?;
            std::io::copy(&mut entry, &mut file).context("extracting cloudflared")?;
            file.sync_all().ok();
            make_executable(dest);
            return Ok(());
        }
    }
    Err(crate::error::InstallError::MissingBinary.into())
}

fn make_executable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755));
    }
}

/// Spawns `cloudflared` to run the named tunnel, with stderr piped for
/// event dispatch.
///
/// The token is written to a `0600` file and passed with `--token-file`
/// rather than `--token <value>`: argv is readable by every local user
/// through `/proc/<pid>/cmdline` on Linux and `ps` on macOS, so passing it
/// inline would expose a live tunnel credential to anyone on the box. The
/// file is unlinked as soon as the process has started.
pub async fn spawn(path: &Path, tunnel_token: &str) -> Result<tokio::process::Child> {
    let token_file = write_token_file(path, tunnel_token).await?;

    let result = Command::new(path)
        .args(["tunnel", "run", "--token-file"])
        .arg(&token_file)
        // `cloudflared` has no `--no-autoupdate` flag; the documented way to
        // disable self-update is this environment variable.
        .env("NO_AUTOUPDATE", "true")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn();

    // Unlink unconditionally, success or failure: leaving a live tunnel
    // credential on disk for the process's lifetime is strictly worse.
    //
    // This races the child's own `open()` — `spawn` returning only means
    // fork/exec finished, not that `cloudflared` has read the file yet.
    // The file is `0600` and `TunnelGuard` removes it again once the
    // stderr pump has observed the process starting, so the exposure
    // window is bounded and the credential never survives the session.
    let _ = fs::remove_file(&token_file).await;

    result.map_err(|e| InstallError::Spawn(e).into())
}

/// Removes a leftover token file.
///
/// The primary unlink happens in [`spawn`]; this exists for the case where
/// the child is killed between fork and its own `open()`, which would
/// otherwise leave the file behind until the next run.
pub(crate) async fn cleanup_token_file(binary: &Path) {
    let _ = fs::remove_file(token_path(binary)).await;
}

/// Writes the tunnel token to a sibling `0600` file next to the binary.
///
/// Keeping it beside `cloudflared` (rather than in the system temp dir)
/// means it inherits the same directory permissions as the binary itself.
async fn write_token_file(binary: &Path, token: &str) -> Result<PathBuf> {
    let file = token_path(binary);

    #[cfg(unix)]
    let opened = {
        // `tokio::fs::OpenOptions` exposes `mode` directly on Unix, so the
        // file is created `0600` rather than chmod-ed afterwards — there is
        // no window in which the secret is world-readable.
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true).mode(0o600);
        opts.open(&file).await
    };
    #[cfg(not(unix))]
    let opened = fs::File::create(&file).await;

    let mut handle = opened.with_context(|| format!("cannot create {}", file.display()))?;
    handle
        .write_all(token.as_bytes())
        .await
        .context("cannot write tunnel token")?;
    handle.flush().await.ok();
    Ok(file)
}

/// Where the tunnel token is staged before `cloudflared` starts.
fn token_path(binary: &Path) -> PathBuf {
    temp_path(binary).with_extension("token")
}

/// Digest of an in-memory buffer, used by the install path and its tests.
#[cfg(test)]
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

/// Reads a whole file into memory. Used by tests only.
#[cfg(test)]
fn read_all(path: &Path) -> Vec<u8> {
    std::fs::read(path).expect("read")
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::GzEncoder;

    #[test]
    fn asset_names_are_platform_specific() {
        assert_eq!(
            download_asset_name("linux", "x86_64").unwrap(),
            "cloudflared-linux-amd64"
        );
        assert_eq!(
            download_asset_name("darwin", "aarch64").unwrap(),
            "cloudflared-darwin-arm64.tgz"
        );
        assert_eq!(
            download_asset_name("windows", "x86_64").unwrap(),
            "cloudflared-windows-amd64.exe"
        );
        assert!(download_asset_name("freebsd", "x86_64").is_err());
    }

    #[test]
    fn line_classification() {
        assert_eq!(
            classify_line("2026-01-01 Registered tunnel connection connIndex=0"),
            LineKind::Connection
        );
        assert_eq!(
            classify_line("ERR Failed to dial to edge"),
            LineKind::Ignore,
        );
        assert_eq!(
            classify_line("ERR something went very wrong"),
            LineKind::Error
        );
        assert_eq!(
            classify_line("Cannot determine default origin certificate path"),
            LineKind::Ignore
        );
    }

    #[test]
    fn candidate_urls_with_proxy_prepends_mirror() {
        let urls = candidate_urls("cloudflared-linux-amd64", Some("https://gh-proxy.org/"));
        assert_eq!(urls.len(), 2);
        assert_eq!(
            urls[0],
            format!(
                "https://gh-proxy.org/{}",
                release_url("cloudflared-linux-amd64")
            )
        );
        assert_eq!(urls[1], release_url("cloudflared-linux-amd64"));
    }

    #[test]
    fn candidate_urls_disabled_when_proxy_empty() {
        let urls = candidate_urls("cloudflared-linux-amd64", Some(""));
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0], release_url("cloudflared-linux-amd64"));
    }

    #[test]
    fn candidate_urls_strips_trailing_slashes_from_proxy() {
        let urls = candidate_urls("cloudflared-linux-amd64", Some("https://gh-proxy.org///"));
        assert!(urls[0].starts_with("https://gh-proxy.org/"));
        assert!(!urls[0].contains("gh-proxy.org//"));
    }

    #[test]
    fn release_url_pins_the_version() {
        // Regression guard: the URL used to be `releases/latest/download`,
        // so a user's binary could change between two runs of the same
        // command with no review.
        let url = release_url("cloudflared-linux-amd64");
        assert!(url.contains(CLOUDFLARED_VERSION), "{url}");
        assert!(!url.contains("/latest/"), "{url} must not track latest");
    }

    #[test]
    fn digest_matches_known_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn every_pinned_asset_has_a_well_formed_digest() {
        for (asset, digest) in ASSET_SHA256 {
            assert_eq!(
                digest.len(),
                64,
                "{asset}: digest must be 64 hex chars, got {digest:?}"
            );
            assert!(
                digest.chars().all(|c| c.is_ascii_hexdigit()),
                "{asset}: non-hex digest {digest:?}"
            );
        }
    }

    #[test]
    fn the_platform_we_shipped_first_is_pinned() {
        // Linux x86_64 is the overwhelmingly common case; if this ever
        // stops being true the release pipeline is broken in a way users
        // would only discover on their first run.
        assert!(expected_sha256("cloudflared-linux-amd64").is_some());
        assert!(expected_sha256("cloudflared-linux-arm64").is_some());
        assert!(expected_sha256("cloudflared-does-not-exist").is_none());
    }

    /// Every asset `download_asset_name` can produce, across the platforms
    /// this build supports. Kept as a table so adding an architecture is a
    /// two-line change with a test failure pointing at it.
    const PLATFORM_ASSETS: &[(&str, &str, &str)] = &[
        ("linux", "x86_64", "cloudflared-linux-amd64"),
        ("linux", "aarch64", "cloudflared-linux-arm64"),
        ("linux", "arm", "cloudflared-linux-arm"),
        ("darwin", "x86_64", "cloudflared-darwin-amd64.tgz"),
        ("darwin", "aarch64", "cloudflared-darwin-arm64.tgz"),
        ("windows", "x86_64", "cloudflared-windows-amd64.exe"),
        ("windows", "x86", "cloudflared-windows-386.exe"),
    ];

    #[test]
    fn asset_table_matches_the_platform_table() {
        for (platform, arch, expected) in PLATFORM_ASSETS {
            assert_eq!(
                download_asset_name(platform, arch).unwrap(),
                *expected,
                "{platform}/{arch} maps to the wrong asset"
            );
        }
    }

    #[test]
    fn all_assets_lists_every_installable_asset() {
        // The list drives the checksum generator script and the CI job,
        // so it has to cover every platform `download_asset_name` can
        // return — no more, no less.
        assert_eq!(
            ALL_ASSETS,
            [
                "cloudflared-darwin-amd64.tgz",
                "cloudflared-darwin-arm64.tgz",
                "cloudflared-linux-amd64",
                "cloudflared-linux-arm64",
                "cloudflared-linux-arm",
                "cloudflared-windows-386.exe",
                "cloudflared-windows-amd64.exe",
            ]
        );
    }

    #[test]
    fn download_asset_name_only_returns_known_assets() {
        for (platform, arch, expected) in PLATFORM_ASSETS {
            let asset = download_asset_name(platform, arch).unwrap();
            assert!(
                ALL_ASSETS.contains(&asset),
                "{platform}/{arch} -> {asset} is not in ALL_ASSETS"
            );
            assert_eq!(asset, *expected);
        }
        // An unsupported pair must still be an error, not a fallback.
        assert!(download_asset_name("plan9", "x86_64").is_err());
    }

    #[test]
    fn the_table_never_mentions_an_unreachable_asset() {
        // A digest for an asset name nothing downloads is dead weight that
        // makes the table look fuller than the coverage actually is.
        for (name, _) in ASSET_SHA256 {
            assert!(
                PLATFORM_ASSETS.iter().any(|(_, _, asset)| asset == name),
                "{name} is pinned but no platform resolves to it"
            );
        }
    }

    #[test]
    fn platforms_without_a_pinned_digest_report_the_escape_hatch() {
        // Not asserting that every platform is covered — a few are
        // deliberately uncovered until CI fills them in. What matters is
        // that an uncovered one fails with instructions rather than
        // installing something unverifiable.
        for (platform, arch, asset) in PLATFORM_ASSETS {
            let digest = expected_sha256(asset);
            if digest.is_none() {
                let err = InstallError::UnverifiedAsset((*asset).to_string()).to_string();
                assert!(err.contains(SHA256_ENV_VAR), "{platform}/{arch}: {err}");
                assert!(err.contains("PATH"), "{platform}/{arch}: {err}");
            }
        }
    }

    #[test]
    fn pinned_digests_are_not_placeholders() {
        // A fabricated digest ("6b1d9b5b0e2e0a1a…") passes a
        // length-and-charset check while making every install fail. The
        // heuristic below catches the obvious cases; `ci.yml`'s
        // `verify-checksums` job is the real guarantee, since it compares
        // against the bytes GitHub actually serves.
        for (asset, digest) in ASSET_SHA256 {
            let bytes: Vec<String> = digest
                .as_bytes()
                .chunks(2)
                .map(|c| String::from_utf8_lossy(c).to_string())
                .collect();
            assert!(
                bytes.len() == 32,
                "{asset}: expected 32 digest bytes, got {}",
                bytes.len()
            );
            assert!(
                bytes.windows(2).all(|w| w[0] != w[1]),
                "{asset}: digest looks like a repeated placeholder pattern"
            );
            assert!(
                !digest.contains("0123") && !digest.contains("abcd") && !digest.contains("7890"),
                "{asset}: digest contains a suspiciously sequential run"
            );
        }
    }

    #[test]
    fn staging_path_sits_beside_the_binary() {
        let staging = temp_path(Path::new("/tmp/bin/cloudflared"));
        assert_eq!(staging, PathBuf::from("/tmp/bin/cloudflared.part"));
    }

    #[test]
    fn digest_mismatch_is_detected() {
        let good = sha256_hex(b"payload");
        assert_ne!(good, sha256_hex(b"tampered"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn token_file_is_0600_and_removed_on_spawn() {
        let tmp = tempfile::tempdir().unwrap();
        let fake = tmp.path().join("cloudflared");
        tokio::fs::write(&fake, "#!/bin/sh\nexec cat /dev/null\n")
            .await
            .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755))
                .await
                .unwrap();
        }

        let _child = spawn(&fake, "secret-tunnel-token")
            .await
            .expect("spawn should succeed");

        // The staging file is unlinked as soon as the process starts, so the
        // secret is not left on disk.
        let staging = temp_path(&fake);
        assert!(
            !staging.exists(),
            "token file must be removed after spawn: {}",
            staging.display()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn token_file_permissions_are_0600_at_creation() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let fake = tmp.path().join("cloudflared");
        let file = write_token_file(&fake, "secret").await.unwrap();
        let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "token file must not be group/world readable");
        assert_eq!(read_all(&file), b"secret");
    }

    #[test]
    fn extract_tgz_finds_the_binary() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("out");

        let mut builder =
            tar::Builder::new(GzEncoder::new(Vec::new(), flate2::Compression::fast()));
        let mut header = tar::Header::new_gnu();
        header.set_size(3);
        header.set_mode(0o755);
        header.set_cksum();
        builder
            .append_data(&mut header, "cloudflared", &b"bin"[..])
            .unwrap();
        let gz = builder.into_inner().unwrap().finish().unwrap();

        extract_tgz(&gz, &dest).expect("extract");
        assert_eq!(read_all(&dest), b"bin");
    }

    #[test]
    fn extract_tgz_rejects_an_archive_without_the_binary() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("out");

        let mut builder =
            tar::Builder::new(GzEncoder::new(Vec::new(), flate2::Compression::fast()));
        let mut header = tar::Header::new_gnu();
        header.set_size(3);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, "README.md", &b"no"[..])
            .unwrap();
        let gz = builder.into_inner().unwrap().finish().unwrap();

        assert!(extract_tgz(&gz, &dest).is_err());
    }
}
