//! gRPC server implementing the [`OrderbookAggregator`] service.
//!
//! Wraps a `watch::Receiver<Summary>` from the merger and streams updates to
//! connected clients. Protobuf conversion (`to_proto`) happens here -- on the
//! per-client tokio task -- keeping the merger free of serialization overhead.

use std::pin::Pin;

use tokio::sync::watch;
use tokio_stream::{Stream, StreamExt, wrappers::WatchStream};
use tonic::{Request, Response, Status};

use crate::types::Summary;

#[allow(clippy::pedantic)] // Generated code.
pub mod proto {
    include!("gen/orderbook.rs");
}

use proto::orderbook_aggregator_server::OrderbookAggregator;

/// gRPC service backed by a `watch` channel from the merger.
pub struct OrderbookService {
    summary_rx: watch::Receiver<Summary>,
}

impl OrderbookService {
    #[must_use]
    pub fn new(summary_rx: watch::Receiver<Summary>) -> Self {
        Self { summary_rx }
    }
}

type SummaryStream = Pin<Box<dyn Stream<Item = Result<proto::Summary, Status>> + Send>>;

#[tonic::async_trait]
impl OrderbookAggregator for OrderbookService {
    type BookSummaryStream = SummaryStream;

    async fn book_summary(
        &self,
        _request: Request<proto::Empty>,
    ) -> Result<Response<Self::BookSummaryStream>, Status> {
        let rx = self.summary_rx.clone();
        // `from_changes` skips the initial default (empty) Summary and only
        // yields once at least one exchange has published real data.
        let stream = WatchStream::from_changes(rx).map(|summary| Ok(to_proto(summary)));
        Ok(Response::new(Box::pin(stream)))
    }
}

/// `Summary` → proto. Heap allocations (`String`, `Vec`) happen here on the
/// per-client task, not in the merger's hot path.
fn to_proto(summary: Summary) -> proto::Summary {
    let cvt = |l: crate::types::Level| proto::Level {
        exchange: crate::exchange_name(l.exchange_id).to_string(),
        price: l.price.to_f64(),
        amount: l.amount.to_f64(),
    };
    proto::Summary {
        spread: crate::types::FixedPoint::spread_to_f64(summary.spread_raw),
        bids: summary.bids.into_iter().map(cvt).collect(),
        asks: summary.asks.into_iter().map(cvt).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{FixedPoint, Level};

    #[allow(clippy::float_cmp)] // Exact f64 values from FixedPoint -- no arithmetic rounding.
    #[test]
    fn to_proto_converts_summary() {
        let mut bids = arrayvec::ArrayVec::new();
        bids.push(Level {
            price: FixedPoint::from_f64(100.5),
            amount: FixedPoint::from_f64(2.0),
            exchange_id: 0, // binance
        });
        let mut asks = arrayvec::ArrayVec::new();
        asks.push(Level {
            price: FixedPoint::from_f64(100.8),
            amount: FixedPoint::from_f64(4.0),
            exchange_id: 1, // bitstamp
        });
        let summary = Summary {
            spread_raw: 30_000_000, // 0.3 in 8-decimal fixed point
            bids,
            asks,
        };

        let proto = to_proto(summary);

        assert!((proto.spread - 0.3).abs() < 1e-10);
        assert_eq!(proto.bids.len(), 1);
        assert_eq!(proto.asks.len(), 1);
        assert_eq!(proto.bids[0].exchange, "binance");
        assert_eq!(proto.bids[0].price, 100.5);
        assert_eq!(proto.bids[0].amount, 2.0);
        assert_eq!(proto.asks[0].exchange, "bitstamp");
        assert_eq!(proto.asks[0].price, 100.8);
        assert_eq!(proto.asks[0].amount, 4.0);
    }
}
