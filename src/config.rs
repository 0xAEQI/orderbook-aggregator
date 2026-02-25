//! CLI configuration via clap.

use clap::Parser;

fn validate_symbol(s: &str) -> Result<String, String> {
    if s.is_empty() {
        return Err("symbol cannot be empty".to_string());
    }
    if !s.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(format!("symbol must be alphanumeric, got: {s}"));
    }
    Ok(s.to_lowercase())
}

#[derive(Parser, Debug, Clone)]
#[command(name = "orderbook-aggregator")]
#[command(
    about = "Aggregates order books from multiple exchanges and streams merged top-of-book via gRPC"
)]
pub struct Config {
    /// Trading pair symbol (e.g., ethbtc)
    #[arg(short, long, default_value = "ethbtc", value_parser = validate_symbol)]
    pub symbol: String,

    /// gRPC server port
    #[arg(short, long, default_value = "50051")]
    pub port: u16,

    /// Metrics/health HTTP port
    #[arg(short = 'm', long, default_value = "9090")]
    pub metrics_port: u16,
}
