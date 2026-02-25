//! CLI configuration via clap.

use clap::Parser;

#[derive(Parser, Debug, Clone)]
#[command(name = "orderbook-aggregator")]
#[command(about = "Aggregates order books from multiple exchanges and streams merged top-of-book via gRPC")]
pub struct Config {
    /// Trading pair symbol (e.g., ethbtc)
    #[arg(short, long, default_value = "ethbtc")]
    pub symbol: String,

    /// gRPC server port
    #[arg(short, long, default_value = "50051")]
    pub port: u16,
}
