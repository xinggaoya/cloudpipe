//! CLI rendering — turns SDK [`Event`]s into colored terminal output.

use colored::Colorize;

use cloudpipe_sdk::{Event, LogLevel, ShutdownReason};

use crate::config::Config;

/// Prints the small startup banner.
pub fn banner() {
    println!(
        "{} {}",
        "⚡ cfp".bright_magenta().bold(),
        "— serverless tunnels via Cloudflare Edge".dimmed()
    );
}

/// Renders a single SDK event to stdout.
pub fn render_event(event: Event) {
    match event {
        Event::Banner => banner(),
        Event::ResolvingConflicts => {}
        Event::CreatingTunnel { name } => {
            println!("  {}", format!("Creating tunnel \"{name}\"...").dimmed());
        }
        Event::IngressConfigured { protocol, port } => {
            println!(
                "  {}",
                format!("Configuring ingress ({})...", protocol).dimmed()
            );
            let _ = port; // matched in the tunnel-live block
        }
        Event::DnsCreated { full_name } => {
            println!("  {}", format!("Creating DNS {full_name}...").dimmed());
        }
        Event::CloudflaredStarted => {}
        Event::EdgeConnected { total, .. } => {
            if total == 1 {
                println!("  {}", "Edge connection established".green());
            } else if total == 4 {
                println!("  {}", "All 4 connections established".green());
            }
        }
        Event::CloudflaredLog { level, line } => {
            if matches!(level, LogLevel::Error) {
                eprintln!("  [cloudflared] {}", line.yellow());
            }
        }
        Event::ShuttingDown { reason } => match reason {
            ShutdownReason::UserRequested => {}
            ShutdownReason::Timeout => {
                println!("  {}", "4h age limit reached — cleaning up.".yellow());
            }
            ShutdownReason::ChildExited => {
                println!("  {}", "cloudflared exited — cleaning up.".yellow());
            }
            _ => {}
        },
        Event::Cleaned => {
            println!("  {}", "Cleaning up tunnel and DNS record...".dimmed());
        }
        _ => {}
    }
}

/// Prints the tunnel success block after the URL is live. Called by `main`
/// once `handle.url()` is known.
pub fn tunnel_live(url: &str, port: u16, protocol: &str, subdomain: &str) {
    println!();
    println!("  {}  🚀", "WE LIVE!".green().bold());
    println!();
    println!("  👉  {}", url.bright_cyan().bold().underline());
    println!();
    println!("  {}", "─".repeat(56).dimmed());
    println!(
        "  {}  {}:{} ({}), subdomain {}",
        "Target".dimmed(),
        protocol,
        port,
        "localhost".dimmed(),
        subdomain
    );
    println!("  {}  Ctrl+C to stop and clean up", "Hint".dimmed());
    println!("  {}", "─".repeat(56).dimmed());
    println!();
}

/// Prints an error in red.
pub fn error(message: &str) {
    eprintln!("{} {}", "✖".red().bold(), message.red());
}

/// Prints a success line in green.
pub fn success(message: &str) {
    println!("{} {}", "✔".green().bold(), message);
}

pub fn no_token_guide() {
    eprintln!("{}", "No Cloudflare API token configured.".red().bold());
    eprintln!();
    eprintln!("{}", "One-time setup (30 seconds):".bold());
    eprintln!("  1. Open https://dash.cloudflare.com/profile/api-tokens");
    eprintln!("  2. Create Token → custom token with these permissions:");
    eprintln!("       Account → Cloudflare Tunnel → Edit");
    eprintln!("       Zone     → DNS               → Edit");
    eprintln!("       Zone     → Zone              → Read");
    eprintln!("  3. Then run:");
    eprintln!();
    eprintln!("       {}", "cfp key <CF_API_TOKEN>".bright_cyan());
    eprintln!();
}

pub fn key_status(config: &Config) {
    match config.effective_token() {
        Some(token) => {
            let masked = mask_token(&token);
            println!("{} {}", "Token".bold(), masked.dimmed());
        }
        None => println!("{} {}", "Token".bold(), "not set".red()),
    }
    print_field("Domain", config.domain.as_deref());
    print_field("Zone ID", config.zone_id.as_deref());
    print_field("Account ID", config.account_id.as_deref());
    println!();
    println!("Config file: ~/.cfp/config.json");
    println!("Clear credentials: {}", "cfp key --clear".dimmed());
}

fn print_field(label: &str, value: Option<&str>) {
    match value {
        Some(value) => println!("{:<12} {}", label.to_string().bold(), value),
        None => println!("{:<12} {}", label.to_string().bold(), "not set".yellow()),
    }
}

fn mask_token(token: &str) -> String {
    if token.len() <= 4 {
        return "****".to_string();
    }
    format!("****{}", &token[token.len() - 4..])
}

pub fn zones(zones: &[cloudpipe_sdk::Zone], current_domain: Option<&str>) {
    if zones.is_empty() {
        println!("{}", "No zones accessible with this token.".yellow());
        return;
    }
    for zone in zones {
        let marker = if Some(zone.name.as_str()) == current_domain {
            "  (current)".green()
        } else {
            "".normal()
        };
        println!("  • {}{}", zone.name.bright_cyan(), marker);
    }
    println!();
    println!("Switch domain: {}", "cfp domain <example.com>".dimmed());
}

pub fn cleanup_summary(removed: usize) {
    if removed == 0 {
        println!("{}", "Nothing to clean up — all good.".green());
    } else {
        success(&format!("Removed {removed} orphaned resource(s)."));
    }
}
