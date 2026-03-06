//! Simulated Bridge device — speaks postcard+COBS over TCP.
//!
//! Generates synthetic antenna patterns so you can test the full PC app
//! UI without any ESP32 hardware.
//!
//! Usage: cargo run --bin rx-sim [--listen 127.0.0.1:9876]

use clap::Parser;
use tokio::net::TcpListener;

#[derive(Parser)]
struct Args {
    /// TCP listen address.
    #[arg(long, default_value = "127.0.0.1:9876")]
    listen: String,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "rx_sim=debug".into()),
        )
        .init();

    let args = Args::parse();

    let listener = TcpListener::bind(&args.listen).await.unwrap();
    tracing::info!("rx-sim listening on {}", args.listen);

    loop {
        let (stream, addr) = listener.accept().await.unwrap();
        tracing::info!("Client connected from {}", addr);
        tokio::spawn(beambench_pc::sim::handle_connection(stream));
    }
}
