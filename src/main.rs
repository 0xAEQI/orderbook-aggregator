use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use tokio::sync::watch;
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;
use tonic::transport::Server;
use tracing::{info, warn};

use orderbook_aggregator::config::Config;
use orderbook_aggregator::error::Error;
use orderbook_aggregator::exchange::binance::BinanceHandler;
use orderbook_aggregator::exchange::bitstamp::BitstampHandler;
use orderbook_aggregator::exchange::wire_exchange;
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
async fn main() -> Result<(), Error> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config = Config::parse();
    info!(
        version = env!("CARGO_PKG_VERSION"),
        commit = env!("GIT_COMMIT"),
        symbol = %config.symbol,
        grpc_port = config.port,
        metrics_port = config.metrics_port,
        "starting orderbook aggregator"
    );

    // Bind gRPC listener eagerly -- fail fast if port is taken, before spawning
    // exchange connections or background tasks.
    let grpc_listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", config.port))
        .await
        .map_err(Error::Bind)?;
    let grpc_addr = grpc_listener.local_addr().map_err(Error::Bind)?;
    info!(%grpc_addr, "gRPC server listening");

    let cancel = CancellationToken::new();

    // --- Exchange wiring ---
    // To add a new exchange: add its name to EXCHANGES in lib.rs, implement
    // WsHandler, and add one wire_exchange call below.
    let metrics = Arc::new(Metrics::register(orderbook_aggregator::EXCHANGES));

    let num_exchanges = orderbook_aggregator::EXCHANGES.len();
    let mut consumers = Vec::with_capacity(num_exchanges);
    let mut threads: Vec<(&str, std::thread::JoinHandle<()>)> =
        Vec::with_capacity(num_exchanges + 1);

    // Each exchange gets its own SPSC ring + dedicated OS thread with a
    // single-threaded tokio runtime. Isolates WS I/O from the main runtime.
    let (c, t) = wire_exchange(
        BinanceHandler::new(),
        config.symbol.clone(),
        RING_BUFFER_CAPACITY,
        metrics.exchange("binance"),
        cancel.clone(),
    )
    .map_err(Error::Spawn)?;
    consumers.push(c);
    threads.push(("binance", t));

    let (c, t) = wire_exchange(
        BitstampHandler::new(),
        config.symbol.clone(),
        RING_BUFFER_CAPACITY,
        metrics.exchange("bitstamp"),
        cancel.clone(),
    )
    .map_err(Error::Spawn)?;
    consumers.push(c);
    threads.push(("bitstamp", t));

    // Watch channel for merger → gRPC server (latest-value semantics).
    let (summary_tx, summary_rx) = watch::channel(Summary::default());

    // Merger on a dedicated OS thread -- plain spin-poll loop, no tokio runtime.
    {
        let metrics = metrics.clone();
        let cancel = cancel.clone();
        let t = std::thread::Builder::new()
            .name("merger".into())
            .spawn(move || {
                merger::run_spsc(consumers, &summary_tx, &metrics, &cancel);
            })
            .map_err(Error::Spawn)?;
        threads.push(("merger", t));
    }

    info!(count = threads.len(), "spawned dedicated OS threads");

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

    // TCP_NODELAY on every accepted gRPC connection. Without it, Nagle's
    // algorithm batches small HTTP/2 frames — adding up to 40ms latency.
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(grpc_listener).map(|result| {
        result.inspect(|stream| {
            let _ = stream.set_nodelay(true);
        })
    });

    let server_cancel = cancel.clone();
    let server = Server::builder()
        // HTTP/2 flow control: 1MB windows prevent backpressure stalls on burst traffic.
        .initial_connection_window_size(1024 * 1024)
        .initial_stream_window_size(1024 * 1024)
        // Detect dead clients without waiting for TCP timeout.
        .http2_keepalive_interval(Some(Duration::from_secs(10)))
        .http2_keepalive_timeout(Some(Duration::from_secs(5)))
        .add_service(OrderbookAggregatorServer::new(service))
        .serve_with_incoming_shutdown(incoming, async move {
            server_cancel.cancelled().await;
        });

    // Run server (blocks until shutdown).
    server.await?;

    // Wait for HTTP to finish.
    let _ = http_handle.await;

    // Join all OS threads (should already be exiting due to cancellation).
    for (name, thread) in threads {
        if thread.join().is_err() {
            warn!(name, "thread panicked");
        }
    }

    info!("shutdown complete");
    Ok(())
}
