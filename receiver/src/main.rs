use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use m5mic_protocol::{DISCOVERY_PORT, WS_PORT};
use m5mic_receiver::{run, ReceiverConfig};

#[derive(Debug, Parser)]
struct Args {
    #[arg(long, default_value = "0.0.0.0")]
    listen: String,

    #[arg(long, default_value_t = WS_PORT)]
    ws_port: u16,

    #[arg(long, default_value_t = DISCOVERY_PORT)]
    discovery_port: u16,

    #[arg(long, default_value = "captures")]
    output_dir: PathBuf,

    #[arg(long)]
    no_recordings: bool,

    #[arg(long)]
    virtual_mic: bool,

    #[arg(long, default_value = "M5Mic Receiver")]
    instance: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "m5mic_receiver=info".to_string()),
        )
        .init();

    let args = Args::parse();
    let config = ReceiverConfig {
        listen: args.listen,
        ws_port: args.ws_port,
        discovery_port: args.discovery_port,
        output_dir: (!args.no_recordings).then_some(args.output_dir),
        instance: args.instance,
        virtual_mic: args.virtual_mic,
    };

    run(config, None).await
}
