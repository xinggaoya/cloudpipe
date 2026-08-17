//! CLI definition (clap derive) and argument normalization.

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};

use cloudpipe_sdk::Protocol;

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
    /// First positional: protocol (`http`/`https`/`tcp`/`udp`/`ssh`) or a port.
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
    Key {
        token: Option<String>,
        #[arg(long)]
        clear: bool,
    },
    Domain {
        domain: String,
    },
    Domains,
    Cleanup,
}

/// Normalized tunnel command arguments.
#[derive(Debug, Clone)]
pub struct TunnelArgs {
    pub protocol: Protocol,
    pub port: u16,
    pub subdomain: Option<String>,
}

impl Cli {
    pub fn to_tunnel_args(&self) -> Result<TunnelArgs> {
        if self.command.is_some() {
            bail!("subcommand handled separately");
        }

        let mut protocol_token = "http";
        let mut port: Option<u16> = None;

        if let Some(first) = &self.first {
            match first.as_str() {
                "http" | "https" | "tcp" | "udp" | "ssh" => protocol_token = first,
                _ => match first.parse::<u16>() {
                    Ok(value) => port = Some(value),
                    Err(_) => bail!(
                        "unexpected argument \"{first}\": expected a protocol \
                         (http, https, tcp, udp, ssh) or a port"
                    ),
                },
            }
        }

        if let Some(value) = self.port {
            port = Some(value);
        }

        Ok(TunnelArgs {
            protocol: Protocol::parse(protocol_token).map_err(anyhow::Error::msg)?,
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
        assert_eq!(args.protocol, Protocol::Https);
        assert_eq!(args.port, 8443);
        assert_eq!(args.subdomain, None);
    }

    #[test]
    fn legacy_style_bare_port() {
        let cli = parse(&["3000"]);
        let args = cli.to_tunnel_args().unwrap();
        assert_eq!(args.protocol, Protocol::Http);
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
}
