//! Console output helpers (colored, minimal, no extra dependencies).

use colored::Colorize;

use crate::cloudflare::Zone;
use crate::config::Config;

/// Prints the small startup banner.
pub fn banner() {
    println!(
        "{} {}",
        "⚡ cfp".bright_magenta().bold(),
        "— serverless tunnels via Cloudflare Edge".dimmed()
    );
}

/// Prints the tunnel success block after the URL is live.
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
    println!(
        "  {}  Ctrl+C to stop and clean up",
        "Hint".dimmed()
    );
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

/// Prints the onboarding guide shown when no API token is configured.
pub fn no_token_guide() {
    eprintln!(
        "{}",
        "No Cloudflare API token configured.".red().bold()
    );
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

/// Prints the current credential status (`cfp key` with no arguments).
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

/// Masks a token, keeping only its last 4 characters.
fn mask_token(token: &str) -> String {
    if token.len() <= 4 {
        return "****".to_string();
    }
    format!("****{}", &token[token.len() - 4..])
}

/// Prints the list of accessible zones (`cfp domains`).
pub fn zones(zones: &[Zone], current_domain: Option<&str>) {
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

/// Prints the outcome of the cleanup command.
pub fn cleanup_summary(removed: usize) {
    if removed == 0 {
        println!("{}", "Nothing to clean up — all good.".green());
    } else {
        success(&format!("Removed {removed} orphaned resource(s)."));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_masking_keeps_tail() {
        assert_eq!(mask_token("abcdef123456"), "****3456");
        assert_eq!(mask_token("abc"), "****");
    }
}
