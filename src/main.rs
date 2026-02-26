use std::sync::Arc;

use clap::Parser;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;
use tonic::transport::Server;
use tracing::info;

use orderbook_aggregator::config::Config;
use orderbook_aggregator::exchange::{Exchange, binance::Binance, bitstamp::Bitstamp};
use orderbook_aggregator::metrics::{self, Metrics};
use orderbook_aggregator::server::{
    OrderbookService, proto::orderbook_aggregator_server::OrderbookAggregatorServer,
};
use orderbook_aggregator::types::{self, Summary};
use orderbook_aggregator::merger;

/// Capacity of the mpsc channel between exchange adapters and the merger.
const BOOK_CHANNEL_CAPACITY: usize = 64;

fn spawn_exchange(
    exchange: impl Exchange,
    name: &'static str,
    symbol: String,
    tx: mpsc::Sender<types::OrderBook>,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(e) = exchange.connect(symbol, tx, cancel).await {
            tracing::error!(exchange = name, error = %e, "fatal error");
        }
    })
}

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
    let (book_tx, book_rx) = mpsc::channel::<types::OrderBook>(BOOK_CHANNEL_CAPACITY);

    // Watch channel for merger → gRPC server (latest-value semantics).
    let (summary_tx, summary_rx) = watch::channel(Summary::default());

    let binance_handle = spawn_exchange(
        Binance { metrics: metrics.exchange("binance") },
        "binance",
        config.symbol.clone(),
        book_tx.clone(),
        cancel.clone(),
    );
    let bitstamp_handle = spawn_exchange(
        Bitstamp { metrics: metrics.exchange("bitstamp") },
        "bitstamp",
        config.symbol.clone(),
        book_tx.clone(),
        cancel.clone(),
    );

    // Drop the original sender so channel closes when exchanges stop.
    drop(book_tx);

    // Spawn merger task.
    let merger_handle = {
        let metrics = metrics.clone();
        tokio::spawn(async move {
            merger::run(book_rx, summary_tx, metrics).await;
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

    // Shutdown signal handler (SIGINT + SIGTERM).
    let shutdown_cancel = cancel.clone();
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            let mut sigterm = tokio::signal::unix::signal(
                tokio::signal::unix::SignalKind::terminate(),
            )
            .expect("failed to register SIGTERM handler");
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {},
                _ = sigterm.recv() => {},
            }
        }
        #[cfg(not(unix))]
        tokio::signal::ctrl_c()
            .await
            .expect("failed to listen for ctrl+c");
        info!("received shutdown signal, draining");
        shutdown_cancel.cancel();
    });

    // Run server (blocks until shutdown).
    server.await?;

    // Wait for all tasks to finish.
    let _ = tokio::join!(binance_handle, bitstamp_handle, merger_handle, http_handle);

    info!("shutdown complete");
    Ok(())
}
