use std::sync::Arc;

use clap::Parser;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tonic::transport::Server;
use tracing::info;

use orderbook_aggregator::config::Config;
use orderbook_aggregator::exchange::binance::BinanceHandler;
use orderbook_aggregator::exchange::bitstamp::BitstampHandler;
use orderbook_aggregator::exchange::spawn_exchange;
use orderbook_aggregator::merger;
use orderbook_aggregator::metrics::{self, Metrics};
use orderbook_aggregator::server::{
    OrderbookService, proto::orderbook_aggregator_server::OrderbookAggregatorServer,
};
use orderbook_aggregator::types::Summary;

/// Capacity of each per-exchange SPSC ring buffer. Small by design -- for order
/// book data only the latest snapshot matters. A small ring ensures the merger
/// processes fresh data after any delay, instead of draining dozens of stale ones.
const RING_BUFFER_CAPACITY: usize = 4;

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

    // Bind gRPC listener eagerly -- fail fast if port is taken, before spawning
    // exchange connections or background tasks.
    let grpc_listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", config.port)).await?;
    let grpc_addr = grpc_listener.local_addr()?;
    info!(%grpc_addr, "gRPC server listening");

    let cancel = CancellationToken::new();

    // Register metrics -- adding a new exchange is a one-line change here.
    let metrics = Arc::new(Metrics::register(&["binance", "bitstamp"]));

    // SPSC ring buffers: one per exchange. Small ring -- only latest snapshot matters.
    // Only store(Release) / load(Acquire) -- no CAS, no contention.
    let (binance_prod, binance_cons) = rtrb::RingBuffer::new(RING_BUFFER_CAPACITY);
    let (bitstamp_prod, bitstamp_cons) = rtrb::RingBuffer::new(RING_BUFFER_CAPACITY);

    // Watch channel for merger → gRPC server (latest-value semantics).
    let (summary_tx, summary_rx) = watch::channel(Summary::default());

    // --- Dedicated OS threads ---
    // Each exchange gets its own OS thread with a single-threaded tokio runtime.
    // Isolates WS I/O from the main runtime -- no work-stealing scheduler jitter.

    let binance_thread = spawn_exchange(
        BinanceHandler::new(),
        config.symbol.clone(),
        binance_prod,
        metrics.exchange("binance"),
        cancel.clone(),
    )?;

    let bitstamp_thread = spawn_exchange(
        BitstampHandler::new(),
        config.symbol.clone(),
        bitstamp_prod,
        metrics.exchange("bitstamp"),
        cancel.clone(),
    )?;

    // Merger on a dedicated OS thread -- plain spin-poll loop, no tokio runtime.
    let merger_thread = {
        let metrics = metrics.clone();
        let cancel = cancel.clone();
        std::thread::Builder::new()
            .name("merger".into())
            .spawn(move || {
                merger::run_spsc(
                    vec![binance_cons, bitstamp_cons],
                    &summary_tx,
                    &metrics,
                    &cancel,
                );
            })?
    };

    info!("spawned dedicated OS threads: ws-binance, ws-bitstamp, merger");

    // Shutdown signal handler (SIGINT + SIGTERM).
    let shutdown_cancel = cancel.clone();
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            let mut sigterm =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
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

    // Run server (blocks until shutdown).
    server.await?;

    // Wait for HTTP to finish.
    let _ = http_handle.await;

    // Join OS threads (should already be exiting due to cancellation).
    if binance_thread.join().is_err() {
        tracing::warn!("binance thread panicked");
    }
    if bitstamp_thread.join().is_err() {
        tracing::warn!("bitstamp thread panicked");
    }
    if merger_thread.join().is_err() {
        tracing::warn!("merger thread panicked");
    }

    info!("shutdown complete");
    Ok(())
}
