//! Order Book Aggregator
//!
//! Connects to Binance and Bitstamp WebSocket feeds, merges their order books,
//! and streams the top-10 bid/ask levels with spread via gRPC.

mod config;
mod error;
mod exchange;
mod merger;
mod metrics;
mod server;
mod types;

use std::sync::Arc;

use clap::Parser;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;
use tonic::transport::Server;
use tracing::info;

use config::Config;
use exchange::{Exchange, binance::Binance, bitstamp::Bitstamp};
use metrics::Metrics;
use server::{OrderbookService, proto::orderbook_aggregator_server::OrderbookAggregatorServer};
use types::Summary;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config = Config::parse();
    info!(
        symbol = %config.symbol,
        grpc_port = config.port,
        metrics_port = config.metrics_port,
        "starting orderbook aggregator"
    );

    // Bind gRPC listener eagerly — fail fast if port is taken, before spawning
    // exchange connections or background tasks.
    let grpc_listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", config.port)).await?;
    let grpc_addr = grpc_listener.local_addr()?;
    info!(%grpc_addr, "gRPC server listening");

    let cancel = CancellationToken::new();

    // Register metrics — adding a new exchange is a one-line change here.
    let metrics = Arc::new(Metrics::register(&["binance", "bitstamp"]));

    // mpsc channel for exchange → merger (move semantics, no broadcast clone overhead).
    let (book_tx, book_rx) = mpsc::channel::<types::OrderBook>(64);

    // Watch channel for merger → gRPC server (latest-value semantics).
    let (summary_tx, summary_rx) = watch::channel(Summary::default());

    // Spawn exchange WebSocket tasks.
    // Each adapter gets its own Arc<ExchangeMetrics> — no field lookup on the hot path.
    let binance = Binance {
        metrics: metrics.exchange("binance"),
    };
    let bitstamp = Bitstamp {
        metrics: metrics.exchange("bitstamp"),
    };

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

    // Drop the original sender so channel closes when exchanges stop.
    drop(book_tx);

    // Spawn merger task.
    let merger_handle = {
        let cancel = cancel.clone();
        let metrics = metrics.clone();
        tokio::spawn(async move {
            merger::run(book_rx, summary_tx, metrics, cancel).await;
        })
    };

    // Spawn metrics/health HTTP server.
    let http_handle = {
        let cancel = cancel.clone();
        let metrics = metrics.clone();
        tokio::spawn(async move {
            metrics::serve_http(config.metrics_port, metrics, cancel).await;
        })
    };

    // gRPC server on the pre-bound listener.
    let service = OrderbookService::new(summary_rx);
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(grpc_listener);

    let server_cancel = cancel.clone();
    let server = Server::builder()
        .add_service(OrderbookAggregatorServer::new(service))
        .serve_with_incoming_shutdown(incoming, async move {
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
    let _ = tokio::join!(binance_handle, bitstamp_handle, merger_handle, http_handle);

    info!("shutdown complete");
    Ok(())
}

#[cfg(test)]
#[allow(clippy::float_cmp)] // Exact f64 literals from test inputs — no arithmetic rounding.
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use tokio::sync::{mpsc, watch};
    use tokio_util::sync::CancellationToken;
    use tonic::Request;

    use crate::merger;
    use crate::metrics::Metrics;
    use crate::server::proto::orderbook_aggregator_client::OrderbookAggregatorClient;
    use crate::server::proto::orderbook_aggregator_server::OrderbookAggregatorServer;
    use crate::server::{proto, OrderbookService};
    use crate::types::{Level, OrderBook, Summary};

    /// End-to-end: mock exchange data → merger → gRPC server → gRPC client.
    #[tokio::test]
    async fn grpc_streams_merged_summary() {
        let (book_tx, book_rx) = mpsc::channel(16);
        let (summary_tx, summary_rx) = watch::channel(Summary::default());
        let cancel = CancellationToken::new();
        let metrics = Arc::new(Metrics::register(&["test_a", "test_b"]));

        // Merger task.
        tokio::spawn({
            let cancel = cancel.clone();
            let metrics = metrics.clone();
            async move { merger::run(book_rx, summary_tx, metrics, cancel).await }
        });

        // gRPC server on ephemeral port.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn({
            let cancel = cancel.clone();
            async move {
                let service = OrderbookService::new(summary_rx);
                tonic::transport::Server::builder()
                    .add_service(OrderbookAggregatorServer::new(service))
                    .serve_with_incoming_shutdown(
                        tokio_stream::wrappers::TcpListenerStream::new(listener),
                        async move { cancel.cancelled().await },
                    )
                    .await
                    .unwrap();
            }
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        // Connect gRPC client and subscribe.
        let mut client = OrderbookAggregatorClient::connect(format!("http://{addr}"))
            .await
            .unwrap();
        let mut stream = client
            .book_summary(Request::new(proto::Empty {}))
            .await
            .unwrap()
            .into_inner();

        // Push order books from two exchanges.
        book_tx
            .send(OrderBook {
                exchange: "test_a",
                bids: [Level {
                    exchange: "test_a",
                    price: 100.0,
                    amount: 5.0,
                }]
                .into_iter()
                .collect(),
                asks: [Level {
                    exchange: "test_a",
                    price: 101.0,
                    amount: 3.0,
                }]
                .into_iter()
                .collect(),
                received_at: Instant::now(),
            })
            .await
            .unwrap();

        book_tx
            .send(OrderBook {
                exchange: "test_b",
                bids: [Level {
                    exchange: "test_b",
                    price: 100.5,
                    amount: 2.0,
                }]
                .into_iter()
                .collect(),
                asks: [Level {
                    exchange: "test_b",
                    price: 100.8,
                    amount: 4.0,
                }]
                .into_iter()
                .collect(),
                received_at: Instant::now(),
            })
            .await
            .unwrap();

        // Read until we get the fully-merged summary (both exchanges present).
        let summary = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(s) = stream.message().await.unwrap()
                    && s.bids.len() == 2
                    && s.asks.len() == 2
                {
                    return s;
                }
            }
        })
        .await
        .expect("timed out waiting for merged summary");

        // Best bid: test_b at 100.5 (higher than test_a at 100.0).
        assert_eq!(summary.bids[0].exchange, "test_b");
        assert_eq!(summary.bids[0].price, 100.5);

        // Best ask: test_b at 100.8 (lower than test_a at 101.0).
        assert_eq!(summary.asks[0].exchange, "test_b");
        assert_eq!(summary.asks[0].price, 100.8);

        // Spread = best_ask - best_bid = 100.8 - 100.5 = 0.3.
        assert!((summary.spread - 0.3).abs() < 1e-10);

        // Both exchanges contribute one level per side.
        assert_eq!(summary.bids.len(), 2);
        assert_eq!(summary.asks.len(), 2);

        cancel.cancel();
    }
}
