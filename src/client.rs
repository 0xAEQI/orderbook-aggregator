//! Demo gRPC client for the order book aggregator.
//!
//! Connects to the `BookSummary` stream and renders a live-updating terminal
//! display of the merged top-10 order book with spread.
//!
//! ```bash
//! cargo run --release --bin client                         # localhost:50051
//! cargo run --release --bin client -- http://server:50051  # custom address
//! ```

use tonic::Request;

#[allow(clippy::pedantic)] // Generated code.
pub mod proto {
    tonic::include_proto!("orderbook");
}

use proto::orderbook_aggregator_client::OrderbookAggregatorClient;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "http://localhost:50051".to_string());

    println!("connecting to {addr}...");

    let mut client = OrderbookAggregatorClient::connect(addr).await?;
    let mut stream = client
        .book_summary(Request::new(proto::Empty {}))
        .await?
        .into_inner();

    println!("streaming order book summaries (ctrl+c to stop)\n");

    while let Some(summary) = stream.message().await? {
        print!("\x1b[2J\x1b[H"); // Clear screen.

        println!("=== Order Book Summary === spread: {:.8}\n", summary.spread);

        println!("{:<10} {:>14} {:>14}", "EXCHANGE", "PRICE", "AMOUNT");
        println!("{}", "-".repeat(40));
        println!("ASKS (lowest first):");
        for ask in summary.asks.iter().rev() {
            println!(
                "{:<10} {:>14.8} {:>14.8}",
                ask.exchange, ask.price, ask.amount
            );
        }

        println!("{}", "-".repeat(40));
        println!("BIDS (highest first):");
        for bid in &summary.bids {
            println!(
                "{:<10} {:>14.8} {:>14.8}",
                bid.exchange, bid.price, bid.amount
            );
        }
        println!("{}", "-".repeat(40));
    }

    Ok(())
}
