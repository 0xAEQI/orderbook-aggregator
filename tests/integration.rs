//! End-to-end integration test: mock exchange data → merger → gRPC → client.

#![allow(clippy::float_cmp)] // Exact f64 values from proto — no arithmetic rounding.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tonic::Request;

use orderbook_aggregator::merger;
use orderbook_aggregator::metrics::Metrics;
use orderbook_aggregator::server::proto::orderbook_aggregator_client::OrderbookAggregatorClient;
use orderbook_aggregator::server::proto::orderbook_aggregator_server::OrderbookAggregatorServer;
use orderbook_aggregator::server::{OrderbookService, proto};
use orderbook_aggregator::types::{FixedPoint, Level, OrderBook, Summary};

/// End-to-end: mock exchange data → SPSC → merger → gRPC server → gRPC client.
#[tokio::test]
async fn grpc_streams_merged_summary() {
    let (mut prod_a, cons_a) = rtrb::RingBuffer::new(16);
    let (mut prod_b, cons_b) = rtrb::RingBuffer::new(16);
    let (summary_tx, summary_rx) = watch::channel(Summary::default());
    let cancel = CancellationToken::new();
    let metrics = Arc::new(Metrics::register(&["test_a", "test_b"]));

    // Merger on a dedicated OS thread (matches production architecture).
    let merger_cancel = cancel.clone();
    let merger_metrics = metrics.clone();
    let merger_thread = std::thread::Builder::new()
        .name("test-merger".into())
        .spawn(move || {
            merger::run_spsc(
                vec![cons_a, cons_b],
                &summary_tx,
                &merger_metrics,
                &merger_cancel,
            );
        })
        .unwrap();

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

    // Retry connection until the gRPC server is ready (no sleep race).
    let mut client = {
        let url = format!("http://{addr}");
        let mut attempts = 0;
        loop {
            match OrderbookAggregatorClient::connect(url.clone()).await {
                Ok(c) => break c,
                Err(_) if attempts < 20 => {
                    attempts += 1;
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Err(e) => panic!("gRPC server did not start: {e}"),
            }
        }
    };
    let mut stream = client
        .book_summary(Request::new(proto::Empty {}))
        .await
        .unwrap()
        .into_inner();

    // Push order books via SPSC producers (sync, no .await needed).
    prod_a
        .push(OrderBook {
            exchange: "test_a",
            bids: [Level {
                exchange: "test_a",
                price: FixedPoint::from_f64(100.0),
                amount: FixedPoint::from_f64(5.0),
            }]
            .into_iter()
            .collect(),
            asks: [Level {
                exchange: "test_a",
                price: FixedPoint::from_f64(101.0),
                amount: FixedPoint::from_f64(3.0),
            }]
            .into_iter()
            .collect(),
            decode_start: Instant::now(),
        })
        .expect("push test_a");

    prod_b
        .push(OrderBook {
            exchange: "test_b",
            bids: [Level {
                exchange: "test_b",
                price: FixedPoint::from_f64(100.5),
                amount: FixedPoint::from_f64(2.0),
            }]
            .into_iter()
            .collect(),
            asks: [Level {
                exchange: "test_b",
                price: FixedPoint::from_f64(100.8),
                amount: FixedPoint::from_f64(4.0),
            }]
            .into_iter()
            .collect(),
            decode_start: Instant::now(),
        })
        .expect("push test_b");

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
    merger_thread.join().unwrap();
}
