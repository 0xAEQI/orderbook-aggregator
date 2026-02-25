//! Order Book Aggregator
//!
//! Connects to Binance and Bitstamp WebSocket feeds, merges their order books,
//! and streams the top-10 bid/ask levels with spread via gRPC.

mod config;
mod error;
mod exchange;
mod merger;
mod server;
mod types;

use clap::Parser;
use tokio::sync::{broadcast, watch};
use tokio_util::sync::CancellationToken;
use tonic::transport::Server;
use tracing::info;

use config::Config;
use exchange::{binance::Binance, bitstamp::Bitstamp, Exchange};
use server::{
    proto::orderbook_aggregator_server::OrderbookAggregatorServer, OrderbookService,
};
use types::Summary;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config = Config::parse();
    info!(symbol = %config.symbol, port = config.port, "starting orderbook aggregator");

    let cancel = CancellationToken::new();

    // Broadcast channel for exchange → merger communication.
    // Buffer of 64: large enough to absorb burst, small enough to keep memory bounded.
    let (book_tx, book_rx) = broadcast::channel::<types::OrderBook>(64);

    // Watch channel for merger → gRPC server communication (latest-value semantics).
    let (summary_tx, summary_rx) = watch::channel(Summary::default());

    // Spawn exchange WebSocket tasks.
    let binance = Binance;
    let bitstamp = Bitstamp;

    let binance_handle = {
        let symbol = config.symbol.clone();
        let tx = book_tx.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            if let Err(e) = binance.connect(symbol, tx, cancel).await {
                tracing::error!(exchange = "binance", error = %e, "fatal error");
            }
        })
    };

    let bitstamp_handle = {
        let symbol = config.symbol.clone();
        let tx = book_tx.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            if let Err(e) = bitstamp.connect(symbol, tx, cancel).await {
                tracing::error!(exchange = "bitstamp", error = %e, "fatal error");
            }
        })
    };

    // Drop the original sender so the broadcast channel closes when exchanges stop.
    drop(book_tx);

    // Spawn merger task.
    let merger_handle = {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            merger::run(book_rx, summary_tx, cancel).await;
        })
    };

    // Start gRPC server.
    let addr = format!("0.0.0.0:{}", config.port).parse()?;
    let service = OrderbookService::new(summary_rx);

    info!(%addr, "gRPC server listening");

    let server_cancel = cancel.clone();
    let server = Server::builder()
        .add_service(OrderbookAggregatorServer::new(service))
        .serve_with_shutdown(addr, async move {
            server_cancel.cancelled().await;
        });

    // Ctrl+C handler.
    let shutdown_cancel = cancel.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to listen for ctrl+c");
        info!("received ctrl+c, shutting down");
        shutdown_cancel.cancel();
    });

    // Run server (blocks until shutdown).
    server.await?;

    // Wait for all tasks to finish.
    let _ = tokio::join!(binance_handle, bitstamp_handle, merger_handle);

    info!("shutdown complete");
    Ok(())
}
