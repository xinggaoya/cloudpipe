//! Locates, downloads and spawns the `cloudflared` binary.
//!
//! `cloudflared` is Cloudflare's tunnel client — it maintains the actual QUIC
//! connection to the Cloudflare edge. It is looked up in `PATH` first, then
//! downloaded to `~/.cfp/bin/` from GitHub Releases.

use anyhow::{bail, Context, Result};
use std::fs;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

/// Base URL for cloudflared release assets.
const GITHUB_BASE_URL: &str = "https://github.com/cloudflare/cloudflared/releases/latest/download";

/// Maps (platform, arch) to the GitHub release asset name.
///
/// Pure function so it can be unit-tested on any platform.
pub fn download_asset_name(platform: &str, arch: &str) -> Result<&'static str> {
    let asset = match (platform, arch) {
        ("darwin", "x86_64") => "cloudflared-darwin-amd64.tgz",
        ("darwin", "aarch64") => "cloudflared-darwin-arm64.tgz",
        ("windows", "x86_64") => "cloudflared-windows-amd64.exe",
        ("windows", "x86") => "cloudflared-windows-386.exe",
        ("linux", "x86_64") => "cloudflared-linux-amd64",
        ("linux", "aarch64") => "cloudflared-linux-arm64",
        ("linux", "arm") => "cloudflared-linux-arm",
        _ => bail!("unsupported platform {platform}/{arch}"),
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

/// Directory that holds the downloaded binary: `~/.cfp/bin`.
fn install_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("cannot determine home directory")?;
    Ok(home.join(".cfp").join("bin"))
}

/// Searches `PATH` (and `PATHEXT` on Windows) for an existing cloudflared.
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
        fs::metadata(path)
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// Ensures a cloudflared binary is available, downloading it if needed.
///
/// Returns the path of the binary to use.
pub fn ensure_installed() -> Result<PathBuf> {
    if let Some(path) = find_in_path() {
        return Ok(path);
    }

    let dir = install_dir()?;
    let path = dir.join(binary_name());
    if is_executable(&path) {
        return Ok(path);
    }

    let asset = download_asset_name(std::env::consts::OS, std::env::consts::ARCH)?;
    let url = format!("{GITHUB_BASE_URL}/{asset}");
    eprintln!("Downloading cloudflared from {url} ...");
    download_and_install(&url, &dir, &path, asset.ends_with(".tgz"))?;
    Ok(path)
}

/// Downloads the asset and writes it to `path` (extracting `.tgz` archives).
fn download_and_install(
    url: &str,
    dir: &Path,
    path: &Path,
    is_tgz: bool,
) -> Result<()> {
    let response = reqwest::blocking::get(url)
        .with_context(|| format!("cannot download {url}"))?
        .error_for_status()
        .with_context(|| format!("download failed for {url}"))?;

    let bytes = response
        .bytes()
        .context("reading download body")?
        .to_vec();

    fs::create_dir_all(dir)
        .with_context(|| format!("cannot create bin dir {}", dir.display()))?;

    if is_tgz {
        extract_tgz(&bytes, dir)
            .with_context(|| format!("extracting archive from {url}"))?;
    } else {
        fs::write(path, &bytes)
            .with_context(|| format!("cannot write {}", path.display()))?;
    }

    make_executable(path);
    Ok(())
}

/// Extracts a `.tgz` archive containing the `cloudflared` binary.
fn extract_tgz(bytes: &[u8], dir: &Path) -> Result<()> {
    let decoder = flate2::read::GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(decoder);
    let entries = archive.entries().context("reading tar archive")?;
    for entry in entries {
        let mut entry = entry.context("reading tar entry")?;
        let entry_path = entry.path().context("tar entry path")?.into_owned();
        if entry_path.file_name().is_some_and(|n| n == "cloudflared") {
            let out_path = dir.join("cloudflared");
            let mut file = fs::File::create(&out_path)
                .with_context(|| format!("cannot create {}", out_path.display()))?;
            std::io::copy(&mut entry, &mut file).context("extracting cloudflared")?;
            make_executable(&out_path);
            return Ok(());
        }
    }
    bail!("cloudflared binary not found in archive")
}

fn make_executable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o755));
    }
}

/// Builds the `--url` value cloudflared connects to locally.
pub fn local_url(protocol: &str, port: u16) -> String {
    format!("{protocol}://localhost:{port}")
}

/// Spawns cloudflared to run the tunnel, piping stderr for status reporting.
pub fn spawn(path: &Path, tunnel_token: &str, local_target: &str) -> Result<Child> {
    Command::new(path)
        .args([
            "tunnel",
            "run",
            "--token",
            tunnel_token,
            "--url",
            local_target,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("cannot spawn {}", path.display()))
}

/// Classification of a cloudflared stderr line for UI display.
#[derive(Debug, PartialEq)]
pub enum LineKind {
    /// A new edge connection was registered.
    Connection,
    /// A real error worth showing the user.
    Error,
    /// Harmless noise (origin cert hints, retries, etc.).
    Ignore,
}

/// Classifies a single stderr line from cloudflared.
///
/// Pure function so it can be unit-tested. Ignore patterns follow the
/// common `cloudflared` log noise (origin cert hints, retries, etc.).
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

/// Streams a stderr pipe to `on_line`, one line at a time.
///
/// Intended to run on a separate thread; the returned handle can be joined on
/// shutdown. Callers pass the `ChildStderr` obtained via `child.stderr.take()`.
pub fn stream_stderr(
    stderr: std::process::ChildStderr,
    mut on_line: impl FnMut(String) + Send + 'static,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(stderr);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let trimmed = line.trim().to_string();
                    if !trimmed.is_empty() {
                        on_line(trimmed);
                    }
                }
            }
        }
    })
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
    fn local_url_builds_expected_value() {
        assert_eq!(local_url("http", 8080), "http://localhost:8080");
        assert_eq!(local_url("https", 8443), "https://localhost:8443");
    }

    #[test]
    fn line_classification() {
        assert_eq!(
            classify_line("2026-01-01 Registered tunnel connection connIndex=0"),
            LineKind::Connection
        );
        assert_eq!(
            classify_line("ERR Failed to dial to edge"),
            LineKind::Ignore, // network noise is ignored, not fatal
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
}
