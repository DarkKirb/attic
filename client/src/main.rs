mod api;
mod cache;
mod cli;
mod command;
mod compression;
mod config;
mod nix_config;
mod nix_netrc;
mod push;
mod version;

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    init_logging()?;
    cli::run().await
}

fn init_logging() -> Result<()> {
    tracing_subscriber::fmt::init();
    Ok(())
}
