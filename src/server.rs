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
        exchange: l.exchange.to_string(),
        price: l.price.to_f64(),
        amount: l.amount.to_f64(),
    };
    proto::Summary {
        spread: summary.spread_raw as f64 / crate::types::FixedPoint::SCALE as f64,
        bids: summary.bids.into_iter().map(cvt).collect(),
        asks: summary.asks.into_iter().map(cvt).collect(),
    }
}
