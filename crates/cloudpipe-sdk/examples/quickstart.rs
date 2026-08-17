//! Minimal quickstart — start a tunnel to `localhost:8080` and print its URL.
//!
//! Run with:
//!
//! ```text
//! CLOUDFLARE_API_TOKEN=... \
//! CFP_DOMAIN=example.com \
//! cargo run --example quickstart
//! ```
//!
//! Account ID and zone ID are looked up automatically by the SDK.

use cloudpipe_sdk::{Protocol, TunnelBuilder};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let token = std::env::var("CLOUDFLARE_API_TOKEN")?;
    let domain = std::env::var("CFP_DOMAIN")?;

    let handle = TunnelBuilder::new()
        .token(token)
        .domain(domain)
        .protocol(Protocol::Http)
        .port(8080)
        .subdomain("quickstart")
        .start()
        .await?;

    println!("live at {}", handle.url());
    handle.wait().await;
    Ok(())
}
