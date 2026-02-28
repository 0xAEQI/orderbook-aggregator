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
    #[arg(short, long, default_value = "ethbtc", value_parser = validate_symbol, env = "ORDERBOOK_SYMBOL")]
    pub symbol: String,

    /// gRPC server port
    #[arg(short, long, default_value = "50051", env = "ORDERBOOK_PORT")]
    pub port: u16,

    /// Metrics/health HTTP port
    #[arg(
        short = 'm',
        long,
        default_value = "9090",
        env = "ORDERBOOK_METRICS_PORT"
    )]
    pub metrics_port: u16,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_symbol_normalizes_to_lowercase() {
        for (input, expected) in [("ETHBTC", "ethbtc"), ("EthBtc", "ethbtc"), ("abc123", "abc123")] {
            assert_eq!(validate_symbol(input).unwrap(), expected, "input: {input}");
        }
    }

    #[test]
    fn validate_symbol_rejects_invalid() {
        for (input, label) in [
            ("", "empty"),
            ("eth-btc", "hyphen"),
            ("eth/btc", "slash"),
            ("eth btc", "space"),
            ("eth.btc", "dot"),
        ] {
            assert!(validate_symbol(input).is_err(), "expected Err for {label}: {input:?}");
        }
    }
}
