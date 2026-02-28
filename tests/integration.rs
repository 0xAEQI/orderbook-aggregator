//! End-to-end integration tests: mock exchange data → merger → gRPC → client.

#![allow(clippy::float_cmp)] // Exact f64 values from proto -- no arithmetic rounding.

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
use orderbook_aggregator::types::{FixedPoint, OrderBook, RawLevel, Summary};

// ---------------------------------------------------------------------------
// Test harness — shared setup for merger + gRPC server + client
// ---------------------------------------------------------------------------

fn make_book(exchange: &'static str, bid_price: f64, ask_price: f64) -> OrderBook {
    OrderBook {
        exchange,
        bids: [RawLevel {
            price: FixedPoint::from_f64(bid_price),
            amount: FixedPoint::from_f64(5.0),
        }]
        .into_iter()
        .collect(),
        asks: [RawLevel {
            price: FixedPoint::from_f64(ask_price),
            amount: FixedPoint::from_f64(3.0),
        }]
        .into_iter()
        .collect(),
        decode_start: Instant::now(),
    }
}

struct TestHarness {
    prod_a: rtrb::Producer<OrderBook>,
    prod_b: rtrb::Producer<OrderBook>,
    stream: tonic::Streaming<proto::Summary>,
    cancel: CancellationToken,
    merger_thread: Option<std::thread::JoinHandle<()>>,
}

impl TestHarness {
    async fn new() -> Self {
        let (prod_a, cons_a) = rtrb::RingBuffer::new(16);
        let (prod_b, cons_b) = rtrb::RingBuffer::new(16);
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
        let stream = client
            .book_summary(Request::new(proto::Empty {}))
            .await
            .unwrap()
            .into_inner();

        Self {
            prod_a,
            prod_b,
            stream,
            cancel,
            merger_thread: Some(merger_thread),
        }
    }

    /// Receive summaries until one matches the expected bid/ask counts, with timeout.
    async fn recv_merged(&mut self, expected_bids: usize, expected_asks: usize) -> proto::Summary {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(s) = self.stream.message().await.unwrap()
                    && s.bids.len() == expected_bids
                    && s.asks.len() == expected_asks
                {
                    return s;
                }
            }
        })
        .await
        .expect("timed out waiting for merged summary")
    }
}

impl Drop for TestHarness {
    fn drop(&mut self) {
        self.cancel.cancel();
        if let Some(thread) = self.merger_thread.take() {
            thread.join().unwrap();
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// End-to-end: mock exchange data → SPSC → merger → gRPC server → gRPC client.
#[tokio::test]
async fn grpc_streams_merged_summary() {
    let mut h = TestHarness::new().await;

    h.prod_a
        .push(OrderBook {
            exchange: "test_a",
            bids: [RawLevel {
                price: FixedPoint::from_f64(100.0),
                amount: FixedPoint::from_f64(5.0),
            }]
            .into_iter()
            .collect(),
            asks: [RawLevel {
                price: FixedPoint::from_f64(101.0),
                amount: FixedPoint::from_f64(3.0),
            }]
            .into_iter()
            .collect(),
            decode_start: Instant::now(),
        })
        .expect("push test_a");

    h.prod_b
        .push(OrderBook {
            exchange: "test_b",
            bids: [RawLevel {
                price: FixedPoint::from_f64(100.5),
                amount: FixedPoint::from_f64(2.0),
            }]
            .into_iter()
            .collect(),
            asks: [RawLevel {
                price: FixedPoint::from_f64(100.8),
                amount: FixedPoint::from_f64(4.0),
            }]
            .into_iter()
            .collect(),
            decode_start: Instant::now(),
        })
        .expect("push test_b");

    let summary = h.recv_merged(2, 2).await;

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
}

/// Push from A, then push updated A with different prices — verify stream reflects new data.
#[tokio::test]
async fn grpc_streams_updated_book() {
    let mut h = TestHarness::new().await;

    // Initial push from both exchanges.
    h.prod_a
        .push(make_book("test_a", 100.0, 101.0))
        .expect("push test_a initial");
    h.prod_b
        .push(make_book("test_b", 100.5, 100.8))
        .expect("push test_b");

    let s1 = h.recv_merged(2, 2).await;
    assert_eq!(s1.bids[0].exchange, "test_b");
    assert_eq!(s1.bids[0].price, 100.5);

    // Updated push from A with a better bid than B.
    h.prod_a
        .push(make_book("test_a", 200.0, 201.0))
        .expect("push test_a updated");

    // Wait for the updated summary where best bid is now test_a at 200.0.
    let s2 = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(s) = h.stream.message().await.unwrap()
                && s.bids.len() == 2
                && s.bids[0].price == 200.0
            {
                return s;
            }
        }
    })
    .await
    .expect("timed out waiting for updated summary");

    assert_eq!(s2.bids[0].exchange, "test_a");
    assert_eq!(s2.bids[0].price, 200.0);
    assert_eq!(s2.asks[0].exchange, "test_b");
    assert_eq!(s2.asks[0].price, 100.8);
}

/// Push from only one exchange — verify stream delivers (startup/degraded mode).
#[tokio::test]
async fn grpc_streams_single_exchange() {
    let mut h = TestHarness::new().await;

    // Only push from exchange A.
    h.prod_a
        .push(make_book("test_a", 50.0, 51.0))
        .expect("push test_a only");

    let summary = h.recv_merged(1, 1).await;

    assert_eq!(summary.bids.len(), 1);
    assert_eq!(summary.asks.len(), 1);
    assert_eq!(summary.bids[0].exchange, "test_a");
    assert_eq!(summary.bids[0].price, 50.0);
    assert_eq!(summary.asks[0].exchange, "test_a");
    assert_eq!(summary.asks[0].price, 51.0);
    assert!((summary.spread - 1.0).abs() < 1e-10);
}
