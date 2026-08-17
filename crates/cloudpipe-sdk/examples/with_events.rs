//! Subscribe to lifecycle events to render your own UI.
//!
//! Run with the same env vars as `quickstart`.

use cloudpipe_sdk::{Event, LogLevel, Protocol, ShutdownReason, TunnelBuilder};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let token = std::env::var("CLOUDFLARE_API_TOKEN")?;
    let domain = std::env::var("CFP_DOMAIN")?;

    let mut handle = TunnelBuilder::new()
        .token(token)
        .domain(domain)
        .protocol(Protocol::Http)
        .port(8080)
        .on_event(render_event)
        .start()
        .await?;

    println!("[main] live at {}", handle.url());
    // Wait for Ctrl+C, then stop the tunnel cleanly so Cloudflare-side
    // resources (tunnel, DNS record) are released.
    let _ = tokio::signal::ctrl_c().await;
    handle.stop().await?;
    Ok(())
}

fn render_event(event: Event) {
    match event {
        Event::Banner => println!("⚡ cloudpipe-sdk"),
        Event::ResolvingConflicts => println!("resolving conflicts..."),
        Event::CreatingTunnel { name } => println!("creating tunnel {name}"),
        Event::IngressConfigured { protocol, port } => {
            println!("ingress ready ({protocol} on :{port})")
        }
        Event::DnsCreated { full_name } => println!("DNS record {full_name} created"),
        Event::CloudflaredStarted => println!("cloudflared started"),
        Event::EdgeConnected { conn_index, total } => {
            println!("edge connection {conn_index} established ({total} total)")
        }
        Event::CloudflaredLog {
            level: LogLevel::Error,
            line,
        } => {
            eprintln!("[cloudflared] {line}")
        }
        Event::CloudflaredLog { .. } => {}
        Event::ShuttingDown { reason } => println!("shutting down: {}", describe(reason)),
        Event::Cleaned => println!("✓ all resources cleaned up"),
        _ => {}
    }
}

fn describe(reason: ShutdownReason) -> &'static str {
    match reason {
        ShutdownReason::UserRequested => "user requested",
        ShutdownReason::Timeout => "timeout",
        ShutdownReason::ChildExited => "cloudflared exited",
        ShutdownReason::Error(_) => "internal error",
        _ => "other",
    }
}
