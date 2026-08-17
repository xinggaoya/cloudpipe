//! Locates, downloads and spawns the `cloudflared` binary.
//!
//! `cloudflared` is Cloudflare's tunnel client. The SDK first looks for it
//! on `PATH`, then falls back to downloading a release asset from GitHub
//! (with an optional mirror prefix).

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{anyhow, Context as _};
use flate2::read::GzDecoder;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::error::{InstallError, Result};

/// Base URL for `cloudflared` release assets.
const GITHUB_BASE_URL: &str = "https://github.com/cloudflare/cloudflared/releases/latest/download";

/// Default GitHub mirror prefix used to speed up downloads in regions where
/// `github.com` is slow or blocked. Override at runtime with the
/// `CFP_GITHUB_PROXY` env var; an empty value disables mirroring entirely.
const DEFAULT_GITHUB_PROXY: &str = "https://v4.gh-proxy.org/";

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
    let direct = format!("{GITHUB_BASE_URL}/{asset}");
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

/// Maps `(platform, arch)` to the GitHub release asset name.
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
        return Ok(path);
    }

    let dir = install_dir(config_dir)?;
    let path = dir.join(binary_name());
    if is_executable(&path) {
        return Ok(path);
    }

    let asset = download_asset_name(std::env::consts::OS, std::env::consts::ARCH)?;
    let is_tgz = asset.ends_with(".tgz");
    let urls = candidate_urls(asset, github_proxy);

    let mut last_err: Option<anyhow::Error> = None;
    for url in &urls {
        match download_and_install(url, &dir, &path, is_tgz).await {
            Ok(()) => return Ok(path),
            Err(err) => {
                if urls.len() > 1 {
                    eprintln!("Download failed ({url}): {err:#}; trying next source");
                }
                // Unwrap the SDK Error back into the inner anyhow::Error so we
                // can pick the deepest cause for the final InstallError.
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

async fn download_and_install(url: &str, dir: &Path, path: &Path, is_tgz: bool) -> Result<()> {
    let response = reqwest::get(url)
        .await
        .with_context(|| format!("cannot download {url}"))?
        .error_for_status()
        .with_context(|| format!("download failed for {url}"))?;

    let bytes = response.bytes().await.context("reading download body")?;

    fs::create_dir_all(dir)
        .await
        .with_context(|| format!("cannot create bin dir {}", dir.display()))?;

    if is_tgz {
        // tar/flate2 are sync APIs — run on a blocking thread.
        let bytes_vec = bytes.to_vec();
        let dir_owned = dir.to_path_buf();
        let path_owned = path.to_path_buf();
        tokio::task::spawn_blocking(move || extract_tgz(&bytes_vec, &dir_owned, &path_owned))
            .await
            .context("extracting archive")??;
    } else {
        let mut file = fs::File::create(path)
            .await
            .with_context(|| format!("cannot create {}", path.display()))?;
        file.write_all(&bytes)
            .await
            .with_context(|| format!("cannot write {}", path.display()))?;
    }

    make_executable(path);
    Ok(())
}

fn extract_tgz(bytes: &[u8], dir: &Path, path: &Path) -> Result<()> {
    let decoder = GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(decoder);
    let entries = archive.entries().context("reading tar archive")?;
    for entry in entries {
        let mut entry = entry.context("reading tar entry")?;
        let entry_path = entry.path().context("tar entry path")?.into_owned();
        if entry_path.file_name().is_some_and(|n| n == "cloudflared") {
            let mut file = std::fs::File::create(path)
                .with_context(|| format!("cannot create {}", path.display()))?;
            std::io::copy(&mut entry, &mut file).context("extracting cloudflared")?;
            make_executable(path);
            // Ensure parent dir exists — `path` may live inside `dir`.
            let _ = dir;
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
pub async fn spawn(path: &Path, tunnel_token: &str) -> Result<tokio::process::Child> {
    Command::new(path)
        .args(["tunnel", "run", "--token", tunnel_token])
        .env("NO_AUTOUPDATE", "true")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| InstallError::Spawn(e).into())
}

#[cfg(test)]
mod tests {
    use super::*;

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
            "https://gh-proxy.org/https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-linux-amd64"
        );
        assert_eq!(
            urls[1],
            "https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-linux-amd64"
        );
    }

    #[test]
    fn candidate_urls_disabled_when_proxy_empty() {
        let urls = candidate_urls("cloudflared-linux-amd64", Some(""));
        assert_eq!(urls.len(), 1);
        assert_eq!(
            urls[0],
            "https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-linux-amd64"
        );
    }

    #[test]
    fn candidate_urls_strips_trailing_slashes_from_proxy() {
        let urls = candidate_urls("cloudflared-linux-amd64", Some("https://gh-proxy.org///"));
        assert_eq!(
            urls[0],
            "https://gh-proxy.org/https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-linux-amd64"
        );
    }
}
