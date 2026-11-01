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

    /// Disable auto-restart of `cloudflared` when it exits on its own.
    ///
    /// By default `cfp` respawns `cloudflared` on the same public URL if
    /// it crashes, so a long-running exposure survives transient edge
    /// drops without manual intervention. Pass this flag to make the
    /// process exit the first time the child goes down.
    #[arg(long)]
    pub no_restart: bool,

    /// Skip TLS verification when the origin is `https://` with a
    /// self-signed certificate.
    ///
    /// Almost every local dev server uses a self-signed certificate, and
    /// `cloudflared` validates the origin certificate, so exposing one over
    /// `https` fails without this.
    #[arg(long)]
    pub insecure: bool,

    /// Send this `Host` header to the origin instead of the public
    /// hostname. Needed when the local service routes on its own host.
    #[arg(long, value_name = "HOST")]
    pub origin_host_header: Option<String>,

    /// Origin TCP connect timeout, in seconds.
    #[arg(long, value_name = "SECONDS")]
    pub origin_connect_timeout: Option<u64>,

    /// Print the SDK's internal trace to stderr.
    ///
    /// Equivalent to `CFP_LOG=debug`. Use it to see the retry/backoff
    /// decisions the supervisor makes when a tunnel is flapping.
    #[arg(long, short = 'v', global = true)]
    pub verbose: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Save, show or clear the Cloudflare API token.
    Key {
        /// The token itself. Prefer `-` (read it from stdin) or
        /// `--token-file`: a token passed as an argument is visible to
        /// every local user through `ps` and `/proc/<pid>/cmdline`.
        token: Option<String>,

        /// Read the token from this file instead of the command line.
        #[arg(long, value_name = "PATH")]
        token_file: Option<String>,

        /// Clear the saved credentials.
        #[arg(long)]
        clear: bool,
    },
    Domain {
        domain: String,
    },
    Domains,
    Cleanup(CleanupArgs),
}

/// Options for `cfp cleanup`.
#[derive(clap::Args, Debug, Clone, Default)]
pub struct CleanupArgs {
    /// List what would be removed without deleting anything.
    #[arg(long)]
    pub dry_run: bool,

    /// Emit a machine-readable JSON report instead of human output.
    #[arg(long)]
    pub json: bool,
}

/// Normalized tunnel command arguments.
#[derive(Debug, Clone)]
pub struct TunnelArgs {
    pub protocol: Protocol,
    pub port: u16,
    pub subdomain: Option<String>,
    pub auto_restart: bool,
    /// Skip origin TLS verification.
    pub insecure: bool,
    /// Override the `Host` header sent to the origin.
    pub origin_host_header: Option<String>,
    /// Origin TCP connect timeout, in seconds.
    pub origin_connect_timeout: Option<u64>,
}

impl Cli {
    pub fn to_tunnel_args(&self) -> Result<TunnelArgs> {
        if self.command.is_some() {
            bail!("subcommand handled separately");
        }

        let mut protocol = Protocol::Http;
        let mut port: Option<u16> = None;

        if let Some(first) = &self.first {
            // The protocol token is matched case-insensitively, the same
            // way `Protocol::parse` (and the Node binding) does it.
            match Protocol::parse(first) {
                Ok(parsed) => protocol = parsed,
                Err(_) => match first.parse::<u16>() {
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

        if let Some(timeout) = self.origin_connect_timeout {
            if timeout == 0 || timeout > 300 {
                bail!("--origin-connect-timeout must be between 1 and 300 seconds");
            }
        }

        Ok(TunnelArgs {
            protocol,
            port: port.unwrap_or(crate::DEFAULT_PORT),
            subdomain: self.subdomain.clone(),
            auto_restart: !self.no_restart,
            insecure: self.insecure,
            origin_host_header: self.origin_host_header.clone(),
            origin_connect_timeout: self.origin_connect_timeout,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

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
    fn protocol_token_is_case_insensitive() {
        // The SDK and the Node binding both accept "HTTPS"; the CLI used
        // to reject it, which made the three front ends disagree.
        let cli = parse(&["HTTPS", "8443"]);
        let args = cli.to_tunnel_args().unwrap();
        assert_eq!(args.protocol, Protocol::Https);
        assert_eq!(args.port, 8443);
    }

    #[test]
    fn an_unknown_token_reports_the_supported_protocols() {
        let err = parse(&["rdp", "22"])
            .to_tunnel_args()
            .unwrap_err()
            .to_string();
        assert!(err.contains("http"), "{err}");
        assert!(err.contains("ssh"), "{err}");
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
            Command::Key {
                token,
                token_file,
                clear,
            } => {
                assert_eq!(token.as_deref(), Some("some-token"));
                assert_eq!(token_file, None);
                assert!(!clear);
            }
            _ => panic!("expected Key command"),
        }
    }

    #[test]
    fn key_accepts_a_token_file() {
        let cli = parse(&["key", "--token-file", "/tmp/tok"]);
        match cli.command.unwrap() {
            Command::Key {
                token, token_file, ..
            } => {
                assert_eq!(token, None);
                assert_eq!(token_file.as_deref(), Some("/tmp/tok"));
            }
            _ => panic!("expected Key command"),
        }
    }

    #[test]
    fn cleanup_flags_default_to_off() {
        let cli = parse(&["cleanup"]);
        match cli.command.unwrap() {
            Command::Cleanup(args) => {
                assert!(!args.dry_run);
                assert!(!args.json);
            }
            _ => panic!("expected Cleanup command"),
        }
    }

    #[test]
    fn cleanup_flags_parse() {
        let cli = parse(&["cleanup", "--dry-run", "--json"]);
        match cli.command.unwrap() {
            Command::Cleanup(args) => {
                assert!(args.dry_run);
                assert!(args.json);
            }
            _ => panic!("expected Cleanup command"),
        }
    }

    #[test]
    fn origin_tuning_flags_parse() {
        let cli = parse(&["https", "8443", "--insecure"]);
        let args = cli.to_tunnel_args().unwrap();
        assert!(args.insecure);
        assert_eq!(args.protocol, Protocol::Https);

        let cli = parse(&["8080", "--origin-host-header", "localhost:8080"]);
        let args = cli.to_tunnel_args().unwrap();
        assert_eq!(args.origin_host_header.as_deref(), Some("localhost:8080"));

        let cli = parse(&["8080", "--origin-connect-timeout", "15"]);
        let args = cli.to_tunnel_args().unwrap();
        assert_eq!(args.origin_connect_timeout, Some(15));
    }

    #[test]
    fn absurd_origin_connect_timeouts_are_rejected() {
        assert!(parse(&["8080", "--origin-connect-timeout", "0"])
            .to_tunnel_args()
            .is_err());
        assert!(parse(&["8080", "--origin-connect-timeout", "99999"])
            .to_tunnel_args()
            .is_err());
    }

    #[test]
    fn insecure_is_off_by_default() {
        assert!(!parse(&["8080"]).to_tunnel_args().unwrap().insecure);
    }

    #[test]
    fn verbose_flag_parses() {
        assert!(parse(&["8080", "-v"]).verbose);
        assert!(parse(&["8080", "--verbose"]).verbose);
        assert!(!parse(&["8080"]).verbose);
    }

    #[test]
    fn version_flag_is_advertised_in_help() {
        // The README used to promise `cfp -v`; clap only provides
        // `-V`/`--version`. Keep this test so docs and binary cannot drift
        // apart silently again.
        let help = Cli::command().render_long_help().to_string();
        assert!(help.contains("--version"), "help must mention --version");
    }
}
