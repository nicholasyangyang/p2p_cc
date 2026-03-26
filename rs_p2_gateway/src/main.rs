use clap::Parser;
use tracing_subscriber::{EnvFilter, fmt};

mod error;
mod config;
use crate::config::Config;
mod keys;
mod relay;
mod broker;
mod app;
mod transport;

#[derive(Parser)]
#[command(version, about = "Nostr P2P Gateway — broker relay for NIP-17 DMs")]
struct Cli {
    #[arg(long, default_value = "7899")]
    port: u16,

    #[arg(long)]
    broker_url: Option<String>,

    #[arg(long, default_value = "all_key.json")]
    keys_file: String,

    #[arg(long)]
    relays: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let _ = dotenvy::dotenv();

    fmt()
        .with_env_filter(
            EnvFilter::try_from_env("RUST_LOG")
                .unwrap_or_else(|_| EnvFilter::from_default_env()),
        )
        .init();

    let relays: Vec<String> = cli
        .relays
        .map(|v| v.split(',').map(String::from).collect())
        .unwrap_or_else(|| Config::from_env().relays);

    let cfg = Config {
        port: cli.port,
        broker_url: cli.broker_url.unwrap_or_else(|| Config::from_env().broker_url),
        keys_file: cli.keys_file,
        relays,
    };

    app::run(cfg).await
}
