//! gRPC server implementing the [`OrderbookAggregator`] service.
//!
//! Wraps a `watch::Receiver<Summary>` from the merger and streams updates to
//! connected clients. Protobuf conversion (`to_proto`) happens here — on the
//! per-client tokio task — keeping the merger free of serialization overhead.

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
///
/// Each `BookSummary` call clones the receiver, so multiple clients share the
/// same underlying data with zero additional cost on the merger side.
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

/// Convert internal `Summary` (stack-allocated `ArrayVec`) to Protobuf types
/// (heap-allocated `Vec<Level>` + `String`). Runs on the gRPC handler task.
fn to_proto(summary: Summary) -> proto::Summary {
    proto::Summary {
        spread: summary.spread,
        bids: summary
            .bids
            .into_iter()
            .map(|l| proto::Level {
                exchange: l.exchange.to_string(),
                price: l.price,
                amount: l.amount,
            })
            .collect(),
        asks: summary
            .asks
            .into_iter()
            .map(|l| proto::Level {
                exchange: l.exchange.to_string(),
                price: l.price,
                amount: l.amount,
            })
            .collect(),
    }
}
