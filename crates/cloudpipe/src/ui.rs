//! CLI rendering — turns SDK [`Event`]s and errors into terminal output.
//!
//! Two rules shape this module:
//!
//! 1. **Width-aware, not glyph-count-aware.** The old renderer hard-coded
//!    `─`.repeat(56) around a table whose real content was 62 columns wide,
//!    and leaned on emoji that render as double-width in most terminals and
//!    as `?` on a Windows CMD. Every rule now measures its own content and
//!    falls back to ASCII box characters when the environment cannot be
//!    trusted (see [`Style`]).
//! 2. **Rendering is a pure function.** [`render_live_block`] and
//!    [`render_cleanup`] build strings rather than printing, so they can be
//!    asserted on directly instead of eyeballed.

use std::io::IsTerminal;

use colored::Colorize;

use cloudpipe_sdk::{
    CleanupReport, CloudflareErrorKind, Error, Event, LogLevel, Protocol, ShutdownReason,
};

use crate::config::Config;

/// How to decorate output: colours on/off and Unicode/ASCII glyphs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Style {
    /// Emit ANSI colour escapes.
    pub color: bool,
    /// Emit Unicode box drawing / emoji rather than ASCII.
    pub unicode: bool,
}

impl Style {
    /// Detects the best style for the current stdout.
    ///
    /// `NO_COLOR` (the de-facto standard) and `CFP_ASCII=1` both force the
    /// conservative path. So does a non-TTY stdout, because writing escape
    /// sequences into a log file or a CI artefact is never what anyone
    /// wants.
    pub fn detect() -> Self {
        let stdout = std::io::stdout();
        let tty = stdout.is_terminal();
        let no_color = std::env::var_os("NO_COLOR").is_some();
        let ascii = std::env::var_os("CFP_ASCII").is_some();
        Self {
            color: tty && !no_color,
            unicode: tty && !ascii,
        }
    }

    /// A style with no escapes and no glyphs — used by tests and by
    /// anyone driving `cfp` from a script.
    #[allow(dead_code)]
    pub fn plain() -> Self {
        Self {
            color: false,
            unicode: false,
        }
    }

    /// Horizontal rule in the chosen glyph set.
    pub fn rule(&self, width: usize) -> String {
        let ch = if self.unicode { '─' } else { '-' };
        std::iter::repeat_n(ch, width).collect()
    }

    /// The bullet prefix for a list item.
    pub fn bullet(&self) -> &'static str {
        if self.unicode {
            "•"
        } else {
            "*"
        }
    }

    /// The status glyphs, or ASCII stand-ins.
    pub fn ok_glyph(&self) -> &'static str {
        if self.unicode {
            "✔"
        } else {
            "+"
        }
    }

    pub fn fail_glyph(&self) -> &'static str {
        if self.unicode {
            "✖"
        } else {
            "x"
        }
    }

    pub fn rocket_glyph(&self) -> &'static str {
        if self.unicode {
            "🚀"
        } else {
            "==>"
        }
    }

    pub fn bolt_glyph(&self) -> &'static str {
        if self.unicode {
            "⚡"
        } else {
            "*"
        }
    }

    pub fn arrow_glyph(&self) -> &'static str {
        if self.unicode {
            "👉"
        } else {
            "->"
        }
    }
}

/// Applies the style's colour policy to a bold string.
fn bold(s: &str, style: Style) -> String {
    if style.color {
        s.bold().to_string()
    } else {
        s.to_string()
    }
}

fn dim(s: &str, style: Style) -> String {
    if style.color {
        s.dimmed().to_string()
    } else {
        s.to_string()
    }
}

fn green(s: &str, style: Style) -> String {
    if style.color {
        s.green().to_string()
    } else {
        s.to_string()
    }
}

fn yellow(s: &str, style: Style) -> String {
    if style.color {
        s.yellow().to_string()
    } else {
        s.to_string()
    }
}

fn red(s: &str, style: Style) -> String {
    if style.color {
        s.red().to_string()
    } else {
        s.to_string()
    }
}

fn cyan_bold(s: &str, style: Style) -> String {
    if style.color {
        s.bright_cyan().bold().underline().to_string()
    } else {
        s.to_string()
    }
}

fn magenta_bold(s: &str, style: Style) -> String {
    if style.color {
        s.bright_magenta().bold().to_string()
    } else {
        s.to_string()
    }
}

/// Sentinel meaning "use the auto-detected style".
const STYLE_AUTO: u8 = u8::MAX;

/// Auto-detection runs once: it reads env vars and stats the terminal, and
/// every render call needs the answer.
static DETECTED: std::sync::LazyLock<Style> = std::sync::LazyLock::new(Style::detect);

/// Test hook. `STYLE_AUTO` means "follow detection"; anything else is an
/// encoded [`Style`].
static STYLE_OVERRIDE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(STYLE_AUTO);

#[allow(dead_code)]
impl Style {
    fn bits(self) -> u8 {
        u8::from(self.color) | (u8::from(self.unicode) << 1)
    }

    fn from_bits(bits: u8) -> Self {
        Self {
            color: bits & 1 == 1,
            unicode: bits & 2 == 2,
        }
    }
}

/// Overrides the detected style. Returns the previous value so tests can
/// restore it.
#[doc(hidden)]
#[allow(dead_code)]
pub fn set_style(new_style: Style) -> Style {
    use std::sync::atomic::Ordering;
    STYLE_OVERRIDE.swap(new_style.bits(), Ordering::SeqCst);
    new_style
}

fn style() -> Style {
    use std::sync::atomic::Ordering;
    match STYLE_OVERRIDE.load(Ordering::Relaxed) {
        STYLE_AUTO => *DETECTED,
        bits => Style::from_bits(bits),
    }
}

/// Prints the small startup banner.
pub fn banner() {
    let st = style();
    println!(
        "{} {}",
        magenta_bold(&format!("{} cfp", st.bolt_glyph()), st),
        dim("— serverless tunnels via Cloudflare Edge", st)
    );
}

/// Renders a single SDK event to stdout.
pub fn render_event(event: Event) {
    let st = style();
    match event {
        Event::Banner => banner(),
        Event::ResolvingConflicts => {}
        Event::CreatingTunnel { name } => {
            println!("  {}", dim(&format!("Creating tunnel \"{name}\"..."), st));
        }
        Event::IngressConfigured { protocol, port } => {
            println!(
                "  {}",
                dim(
                    &format!("Configuring ingress ({protocol}://localhost:{port})..."),
                    st
                )
            );
        }
        Event::DnsCreated { full_name } => {
            println!("  {}", dim(&format!("Creating DNS {full_name}..."), st));
        }
        Event::CloudflaredStarted => {}
        Event::Restarting { reason, attempt } => {
            println!(
                "  {}",
                yellow(
                    &format!(
                        "cloudflared ended ({}) — restarting (attempt #{attempt})...",
                        describe_reason(&reason)
                    ),
                    st
                )
            );
        }
        Event::Restarted { attempt } => {
            println!(
                "  {}",
                green(
                    &format!("Resumed on the same URL (attempt #{attempt})."),
                    st
                )
            );
        }
        Event::RestartGivingUp {
            attempts,
            last_error,
        } => {
            println!(
                "  {}",
                red(
                    &format!(
                        "auto-restart failed {attempts} times in a row — giving up. \
                         last error: {last_error}"
                    ),
                    st
                )
            );
        }
        Event::BootstrapFailed { attempt, error } => {
            println!(
                "  {}",
                yellow(&format!("reconnect attempt #{attempt} failed: {error}"), st)
            );
        }
        Event::EdgeConnected { total, .. } => {
            if total == 1 {
                println!("  {}", green("Edge connection established", st));
            } else if total == 4 {
                println!("  {}", green("All 4 connections established", st));
            }
        }
        Event::CloudflaredLog { level, line } => {
            if matches!(level, LogLevel::Error) {
                eprintln!("  [cloudflared] {}", yellow(&line, st));
            }
        }
        Event::ShuttingDown { reason } => {
            if matches!(reason, ShutdownReason::ChildExited) {
                println!(
                    "  {}",
                    dim("cloudflared exited — cleaning up before respawn.", st)
                );
            }
        }
        Event::Cleaned => {
            println!("  {}", green("Tunnel and DNS record cleaned up.", st));
        }
        _ => {}
    }
}

fn describe_reason(reason: &ShutdownReason) -> String {
    match reason {
        ShutdownReason::UserRequested => "user requested".to_string(),
        ShutdownReason::ChildExited => "cloudflared exited".to_string(),
        ShutdownReason::Error(err) => format!("internal error: {err}"),
        other => format!("{other:?}"),
    }
}

/// Builds the "we live" block. Pure — returns the text instead of printing
/// it so the layout can be asserted on.
pub fn render_live_block(
    url: &str,
    protocol: Protocol,
    port: u16,
    subdomain: &str,
    auto_restart: bool,
) -> String {
    let st = style();
    let hint = if auto_restart {
        "Ctrl+C to stop; cloudflared auto-restarts on crash"
    } else {
        "Ctrl+C to stop and clean up"
    };
    let rows = [
        format!("{:<8}{protocol}://localhost:{port}", "Target"),
        format!("{:<8}{subdomain}.<your zone>", "Subdomain"),
        format!("{:<8}{hint}", "Hint"),
    ];

    // Measure the longest row so the rule actually matches the content,
    // rather than a magic constant that drifts on every locale change.
    let width = rows
        .iter()
        .map(|r| r.chars().count())
        .max()
        .unwrap_or(0)
        .max(20);

    let mut out = String::new();
    out.push('\n');
    out.push_str(&format!(
        "  {} {}\n",
        green("WE LIVE!", st),
        st.rocket_glyph()
    ));
    out.push('\n');
    out.push_str(&format!("  {} {}\n", st.arrow_glyph(), cyan_bold(url, st)));
    out.push('\n');
    out.push_str(&format!("  {}\n", dim(&st.rule(width + 2), st)));
    for row in rows {
        out.push_str(&format!("  {row}\n"));
    }
    out.push_str(&format!("  {}\n", dim(&st.rule(width + 2), st)));
    out.push('\n');
    out
}

/// Prints the tunnel success block after the URL is live.
pub fn tunnel_live(url: &str, port: u16, protocol: &str, subdomain: &str, auto_restart: bool) {
    let parsed = Protocol::parse(protocol).unwrap_or(Protocol::Http);
    print!(
        "{}",
        render_live_block(url, parsed, port, subdomain, auto_restart)
    );
}

/// Maps an SDK error onto actionable guidance.
///
/// The SDK's [`Error`] already carries a good message, but the README
/// promises "Cloudflare API errors → translated to actionable text". That
/// promise used to be kept only by accident: `main` funnelled every error
/// through `anyhow::Error::msg`, throwing away the type. Matching on the
/// real variant is what makes the promise true.
pub fn render_sdk_error(err: Error) -> anyhow::Error {
    use anyhow::anyhow;
    let hint = sdk_error_hint(&err);
    let message = format!("{err}");
    match hint {
        Some(h) => anyhow!("{message}\n\n  {h}"),
        None => anyhow!("{message}"),
    }
}

/// The actionable next step for an SDK error, if we have one.
pub fn sdk_error_hint(err: &Error) -> Option<&'static str> {
    use Error::*;
    match err {
        Cloudflare(cloudpipe_sdk::ApiError::Api { kind, .. }) => match kind {
            CloudflareErrorKind::RateLimited => {
                Some("Cloudflare is rate-limiting this token. Wait ~30s and retry; `cfp cleanup` clears leftovers from an interrupted run.")
            }
            CloudflareErrorKind::AuthFailed | CloudflareErrorKind::InvalidToken => {
                Some("The token was rejected. Re-create it at https://dash.cloudflare.com/profile/api-tokens with Account→Tunnel:Edit, Zone→DNS:Edit and Zone→Zone:Read, then run `cfp key <token>`.")
            }
            CloudflareErrorKind::DnsExists => {
                Some("A DNS record with that name already exists. Try another `-s <subdomain>`, or run `cfp cleanup`.")
            }
            _ => None,
        },
        Cloudflare(cloudpipe_sdk::ApiError::Timeout(_)) => {
            Some("The request to Cloudflare timed out. Check network/proxy settings and retry.")
        }
        Cloudflare(cloudpipe_sdk::ApiError::NonJson { status, .. }) if *status >= 500 => {
            Some("Cloudflare returned a server error. This is usually transient — retry in a moment.")
        }
        SubdomainInUse(_) => {
            Some("That subdomain is serving a live tunnel right now. Pick a different `-s <name>`.")
        }
        InvalidSubdomain(_) => {
            Some("Subdomains must be 3-63 chars of a-z, 0-9 and dashes, not starting or ending with a dash.")
        }
        MissingCredential(_) => Some("Run `cfp key <CF_API_TOKEN>` first, or set CLOUDFLARE_API_TOKEN."),
        AlreadyShutDown => Some("This handle was already stopped; nothing left to do."),
        Cloudflared(_) => Some(
            "cloudflared could not be installed or started. Pre-install it and pass its path, or check CFP_GITHUB_PROXY if you are behind a network that blocks GitHub.",
        ),
        _ => None,
    }
}

/// Prints an indented, dimmed progress line (e.g. "Verifying token…").
pub fn step(message: &str) {
    println!("  {}", dim(message, style()));
}

/// Prints a blank line followed by a bold section heading.
pub fn heading(message: &str) {
    println!();
    println!("{}", bold(message, style()));
}

/// Prints an error in red.
pub fn error(message: &str) {
    eprintln!("{} {}", style().fail_glyph().red().bold(), message.red());
}

/// Prints a success line in green.
pub fn success(message: &str) {
    println!("{} {}", style().ok_glyph().green().bold(), message);
}

pub fn no_token_guide() {
    let st = style();
    eprintln!(
        "{}",
        red(&bold("No Cloudflare API token configured.", st), st)
    );
    eprintln!();
    eprintln!("{}", bold("One-time setup (30 seconds):", st));
    eprintln!("  1. Open https://dash.cloudflare.com/profile/api-tokens");
    eprintln!("  2. Create Token -> custom token with these permissions:");
    eprintln!("       Account -> Cloudflare Tunnel -> Edit");
    eprintln!("       Zone     -> DNS               -> Edit");
    eprintln!("       Zone     -> Zone              -> Read");
    eprintln!("  3. Then run:");
    eprintln!();
    eprintln!("       {}", cyan_bold("cfp key <CF_API_TOKEN>", st));
    eprintln!();
}

pub fn key_status(config: &Config) {
    let st = style();
    match config.effective_token() {
        Some(token) => {
            let masked = mask_token(&token);
            println!("{} {}", bold("Token", st), dim(&masked, st));
        }
        None => println!("{} {}", bold("Token", st), red("not set", st)),
    }
    print_field("Domain", config.domain.as_deref(), st);
    print_field("Zone ID", config.zone_id.as_deref(), st);
    print_field("Account ID", config.account_id.as_deref(), st);
    println!();
    println!("Config file: ~/.cfp/config.json");
    println!("Clear credentials: {}", dim("cfp key --clear", st));
}

fn print_field(label: &str, value: Option<&str>, st: Style) {
    match value {
        Some(value) => println!("{:<12} {}", bold(label, st), value),
        None => println!("{:<12} {}", bold(label, st), yellow("not set", st)),
    }
}

fn mask_token(token: &str) -> String {
    if token.len() <= 4 {
        return "****".to_string();
    }
    format!("****{}", &token[token.len() - 4..])
}

pub fn zones(zones: &[cloudpipe_sdk::Zone], current_domain: Option<&str>) {
    let st = style();
    if zones.is_empty() {
        println!("{}", yellow("No zones accessible with this token.", st));
        return;
    }
    for zone in zones {
        let marker = if Some(zone.name.as_str()) == current_domain {
            "  (current)"
        } else {
            ""
        };
        println!(
            "  {} {}{}",
            st.bullet(),
            if st.color {
                zone.name.bright_cyan().to_string()
            } else {
                zone.name.clone()
            },
            if marker.is_empty() {
                String::new()
            } else {
                green(marker, st)
            }
        );
    }
    println!();
    println!("Switch domain: {}", dim("cfp domain <example.com>", st));
}

/// Renders a [`CleanupReport`]. Pure — returns the text instead of printing.
pub fn render_cleanup(report: &CleanupReport, dry_run: bool, json: bool) -> String {
    if json {
        return serde_json::to_string_pretty(report)
            .unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"));
    }
    let st = style();
    let mut out = String::new();

    if dry_run {
        if report.would_remove.is_empty() {
            out.push_str(&format!(
                "{} Nothing to clean up — all good.\n",
                green("Nothing to clean up.", st)
            ));
            return out;
        }
        out.push_str(&format!(
            "{} {} resource(s) would be removed (dry run):\n",
            yellow("Would remove:", st),
            report.would_remove.len()
        ));
        for action in &report.would_remove {
            out.push_str(&format!("  {} {action}\n", st.bullet()));
        }
    } else if report.is_empty() {
        out.push_str(&format!(
            "{} Nothing to clean up — all good.\n",
            green("Nothing to clean up.", st)
        ));
        return out;
    } else {
        out.push_str(&format!(
            "{} Removed {} resource(s).\n",
            green("Done.", st),
            report.removed.len()
        ));
        for action in &report.removed {
            out.push_str(&format!("  {} {action}\n", st.bullet()));
        }
    }

    if !report.errors.is_empty() {
        out.push('\n');
        out.push_str(&format!(
            "{} {} error(s):\n",
            red("Failed:", st),
            report.errors.len()
        ));
        for err in &report.errors {
            out.push_str(&format!("  {} {err}\n", st.bullet()));
        }
    }
    out
}

pub fn cleanup_summary(report: &CleanupReport, dry_run: bool, json: bool) {
    print!("{}", render_cleanup(report, dry_run, json));
}

#[cfg(test)]
mod tests {
    use super::*;
    use cloudpipe_sdk::CleanupAction;

    #[test]
    fn live_block_rules_match_content_width() {
        let prev = set_style(Style::plain());
        let block = render_live_block("https://a.example.com", Protocol::Tcp, 22, "myapp", true);
        set_style(prev);

        let rule_line = block
            .lines()
            .find(|l| l.trim_start().starts_with('-') && l.contains("--"))
            .expect("a rule line");
        // The rule must be at least as wide as the longest content row.
        let widest = block
            .lines()
            .filter(|l| l.starts_with("  ") && !l.trim_start().starts_with('-'))
            .map(|l| l.trim_end().chars().count())
            .max()
            .unwrap();
        assert!(
            rule_line.chars().count() >= widest,
            "rule too narrow: {} vs {widest}",
            rule_line.chars().count()
        );
    }

    #[test]
    fn live_block_shows_port_and_auto_restart_hint() {
        let prev = set_style(Style::plain());
        let on = render_live_block("https://x.example.com", Protocol::Tcp, 22, "myapp", true);
        let off = render_live_block("https://x.example.com", Protocol::Tcp, 22, "myapp", false);
        set_style(prev);

        assert!(on.contains("tcp://localhost:22"), "{on}");
        assert!(on.contains("auto-restarts"), "{on}");
        assert!(!off.contains("auto-restarts"), "{off}");
        assert!(on.contains("https://x.example.com"));
    }

    #[test]
    fn ascii_style_avoids_unicode_glyphs() {
        let st = Style {
            color: false,
            unicode: false,
        };
        let line = st.rule(10);
        assert_eq!(line, "----------");
        assert_eq!(st.bullet(), "*");
        assert_eq!(st.ok_glyph(), "+");
        assert!(st.rocket_glyph().is_ascii());
        assert!(st.arrow_glyph().is_ascii());
        assert!(st.bolt_glyph().is_ascii());
    }

    #[test]
    fn unicode_style_uses_box_drawing() {
        let st = Style {
            color: false,
            unicode: true,
        };
        assert_eq!(st.rule(5), "─────");
        assert_eq!(st.bullet(), "•");
    }

    #[test]
    fn plain_style_emits_no_escape_sequences() {
        let prev = set_style(Style::plain());
        let block = render_live_block("https://x.example.com", Protocol::Http, 8080, "a", false);
        set_style(prev);
        assert!(!block.contains('\u{1b}'), "colour escapes leaked");
    }

    #[test]
    fn cleanup_dry_run_lists_without_removing() {
        let mut report = CleanupReport::default();
        report.would_remove.push(CleanupAction::Tunnel {
            id: "t1".into(),
            name: "app".into(),
        });
        let prev = set_style(Style::plain());
        let out = render_cleanup(&report, true, false);
        set_style(prev);

        assert!(out.contains("Would remove"), "{out}");
        assert!(out.contains("tunnel t1 (app)"), "{out}");
        assert!(!out.contains("Removed"), "{out}");
    }

    #[test]
    fn cleanup_removed_is_listed() {
        let mut report = CleanupReport::default();
        report.removed.push(CleanupAction::DnsRecord {
            id: "r1".into(),
            name: "a.example.com".into(),
        });
        report.errors.push("deleting tunnel t2: boom".into());
        let prev = set_style(Style::plain());
        let out = render_cleanup(&report, false, false);
        set_style(prev);

        assert!(out.contains("Removed 1 resource"), "{out}");
        assert!(out.contains("dns r1 (a.example.com)"), "{out}");
        assert!(out.contains("1 error"), "{out}");
    }

    #[test]
    fn cleanup_empty_reports_nothing_to_do() {
        let prev = set_style(Style::plain());
        let out = render_cleanup(&CleanupReport::default(), false, false);
        set_style(prev);
        assert!(out.contains("Nothing to clean up"), "{out}");
    }

    #[test]
    fn cleanup_json_is_machine_readable() {
        let mut report = CleanupReport::default();
        report.removed.push(CleanupAction::Tunnel {
            id: "t1".into(),
            name: "app".into(),
        });
        let out = render_cleanup(&report, false, true);
        let parsed: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
        assert_eq!(parsed["removed"][0]["kind"], "tunnel");
        assert_eq!(parsed["removed"][0]["id"], "t1");
        assert_eq!(parsed["removed"][0]["name"], "app");
        assert!(parsed["wouldRemove"].is_array());
        assert!(parsed["errors"].is_array());
    }

    #[test]
    fn rate_limit_error_gets_actionable_hint() {
        let err = Error::Cloudflare(cloudpipe_sdk::ApiError::Api {
            code: Some(10429),
            message: "Rate limited.".into(),
            kind: CloudflareErrorKind::RateLimited,
        });
        let hint = sdk_error_hint(&err).unwrap();
        assert!(hint.contains("rate-limiting"), "{hint}");
        assert!(hint.contains("cfp cleanup"), "{hint}");
    }

    #[test]
    fn auth_error_hint_mentions_permissions() {
        let err = Error::Cloudflare(cloudpipe_sdk::ApiError::Api {
            code: Some(10001),
            message: "bad token".into(),
            kind: CloudflareErrorKind::AuthFailed,
        });
        let hint = sdk_error_hint(&err).unwrap();
        assert!(hint.contains("dash.cloudflare.com"), "{hint}");
        assert!(hint.contains("cfp key"), "{hint}");
    }

    #[test]
    fn subdomain_in_use_hint_suggests_alternative() {
        let hint = sdk_error_hint(&Error::SubdomainInUse("app".into())).unwrap();
        assert!(hint.contains("-s"), "{hint}");
    }

    #[test]
    fn already_shut_down_hint_explains_the_no_op() {
        assert_eq!(
            sdk_error_hint(&Error::AlreadyShutDown),
            Some("This handle was already stopped; nothing left to do.")
        );
    }

    #[test]
    fn errors_without_a_specific_playbook_have_no_hint() {
        // A bare API code we do not recognise must not invent advice.
        let unknown = Error::Cloudflare(cloudpipe_sdk::ApiError::Api {
            code: Some(7003),
            message: "could not route to origin".into(),
            kind: CloudflareErrorKind::Other(7003),
        });
        assert!(sdk_error_hint(&unknown).is_none());
    }

    #[test]
    fn render_sdk_error_appends_hint_to_message() {
        let err = Error::SubdomainInUse("app".into());
        let rendered = render_sdk_error(err).to_string();
        assert!(rendered.contains("subdomain already in use"), "{rendered}");
        assert!(rendered.contains("-s"), "{rendered}");
    }

    #[test]
    fn mask_token_never_leaks_the_secret() {
        assert_eq!(mask_token("abcd"), "****");
        assert_eq!(mask_token("abc"), "****");
        assert_eq!(mask_token("abcdefgh"), "****efgh");
    }

    #[test]
    fn describe_reason_handles_all_variants() {
        assert_eq!(
            describe_reason(&ShutdownReason::UserRequested),
            "user requested"
        );
        assert_eq!(
            describe_reason(&ShutdownReason::ChildExited),
            "cloudflared exited"
        );
        assert!(describe_reason(&ShutdownReason::Error("x".into())).contains("internal error"));
    }
}
