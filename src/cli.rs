//! CLI definition (clap derive) and argument normalization.

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};

/// cfp — serverless localhost tunnels via Cloudflare Edge.
///
/// Bring your own Cloudflare API token: `cfp key <token>`, then run
/// `cfp http 8080` to expose localhost:8080 at a random subdomain.
#[derive(Parser, Debug)]
#[command(
    name = "cfp",
    version,
    about = "Serverless localhost tunnels via Cloudflare Edge (no backend needed)",
    disable_help_subcommand = true
)]
pub struct Cli {
    /// First positional: protocol (`http`/`https`) or a port number.
    #[arg(value_name = "PROTOCOL_OR_PORT")]
    pub first: Option<String>,

    /// Local port to expose (used when the protocol is given first).
    #[arg(value_name = "PORT")]
    pub port: Option<u16>,

    /// Subdomain to use, e.g. `-s myapp` → `myapp.example.com`.
    #[arg(short, long, value_name = "SUBDOMAIN")]
    pub subdomain: Option<String>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Save, show or clear the Cloudflare API token.
    ///
    /// `cfp key <CF_API_TOKEN>` saves the token and auto-discovers the
    /// account/domain. `cfp key` shows the current status.
    Key {
        /// Cloudflare API token (omitted to show status).
        token: Option<String>,
        /// Clear all saved credentials.
        #[arg(long)]
        clear: bool,
    },

    /// Switch the base domain (picks the matching zone).
    Domain {
        /// Domain name, e.g. `example.com`.
        domain: String,
    },

    /// List domains accessible with the current token.
    Domains,

    /// Remove dead tunnels, their DNS records and orphaned CNAMEs.
    Cleanup,
}

/// Normalized tunnel command arguments.
#[derive(Debug, Clone)]
pub struct TunnelArgs {
    pub protocol: String,
    pub port: u16,
    pub subdomain: Option<String>,
}

impl Cli {
    /// Converts the raw CLI into tunnel arguments.
    ///
    /// Accepts both `cfp http 8080` (ngrok style) and `cfp 8080`
    /// (legacy style). Pure function, easy to unit-test.
    pub fn to_tunnel_args(&self) -> Result<TunnelArgs> {
        if self.command.is_some() {
            bail!("subcommand handled separately");
        }

        let mut protocol = "http";
        let mut port: Option<u16> = None;

        if let Some(first) = &self.first {
            match first.as_str() {
                "http" | "https" => protocol = first,
                _ => match first.parse::<u16>() {
                    Ok(value) => port = Some(value),
                    Err(_) => bail!(
                        "unexpected argument \"{first}\": expected `http`, `https` or a port"
                    ),
                },
            }
        }

        if let Some(value) = self.port {
            port = Some(value);
        }

        Ok(TunnelArgs {
            protocol: protocol.to_string(),
            port: port.unwrap_or(crate::DEFAULT_PORT),
            subdomain: self.subdomain.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("cfp").chain(args.iter().copied())).unwrap()
    }

    #[test]
    fn ngrok_style_https_port() {
        let cli = parse(&["https", "8443"]);
        let args = cli.to_tunnel_args().unwrap();
        assert_eq!(args.protocol, "https");
        assert_eq!(args.port, 8443);
        assert_eq!(args.subdomain, None);
    }

    #[test]
    fn legacy_style_bare_port() {
        let cli = parse(&["3000"]);
        let args = cli.to_tunnel_args().unwrap();
        assert_eq!(args.protocol, "http");
        assert_eq!(args.port, 3000);
    }

    #[test]
    fn default_port_when_missing() {
        let cli = parse(&[]);
        let args = cli.to_tunnel_args().unwrap();
        assert_eq!(args.port, crate::DEFAULT_PORT);
    }

    #[test]
    fn subdomain_flag() {
        let cli = parse(&["http", "8080", "-s", "myapp"]);
        let args = cli.to_tunnel_args().unwrap();
        assert_eq!(args.subdomain.as_deref(), Some("myapp"));
    }

    #[test]
    fn unknown_first_argument_is_rejected() {
        let cli = parse(&["bogus"]);
        assert!(cli.to_tunnel_args().is_err());
    }

    #[test]
    fn key_subcommand_parses() {
        let cli = parse(&["key", "some-token"]);
        match cli.command.unwrap() {
            Command::Key { token, clear } => {
                assert_eq!(token.as_deref(), Some("some-token"));
                assert!(!clear);
            }
            _ => panic!("expected Key command"),
        }
    }

    #[test]
    fn domain_and_cleanup_subcommands_parse() {
        match parse(&["domain", "example.com"]).command.unwrap() {
            Command::Domain { domain } => assert_eq!(domain, "example.com"),
            _ => panic!("expected Domain command"),
        }
        assert!(matches!(parse(&["domains"]).command.unwrap(), Command::Domains));
        assert!(matches!(parse(&["cleanup"]).command.unwrap(), Command::Cleanup));
    }
}
