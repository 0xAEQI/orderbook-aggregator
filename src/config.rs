//! CLI configuration parsed from command-line arguments via [`clap`].

use clap::Parser;

/// Validate that the trading pair symbol is non-empty and alphanumeric,
/// then normalize to lowercase (exchanges expect lowercase symbols).
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_symbol_normalizes_to_lowercase() {
        assert_eq!(validate_symbol("ETHBTC").unwrap(), "ethbtc");
        assert_eq!(validate_symbol("EthBtc").unwrap(), "ethbtc");
    }

    #[test]
    fn validate_symbol_rejects_empty() {
        assert!(validate_symbol("").is_err());
    }

    #[test]
    fn validate_symbol_rejects_special_chars() {
        assert!(validate_symbol("eth-btc").is_err());
        assert!(validate_symbol("eth/btc").is_err());
        assert!(validate_symbol("eth btc").is_err());
    }

    #[test]
    fn validate_symbol_accepts_alphanumeric() {
        assert!(validate_symbol("ethbtc").is_ok());
        assert!(validate_symbol("btcusdt").is_ok());
        assert!(validate_symbol("abc123").is_ok());
    }
}
