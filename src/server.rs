//! gRPC server implementing the `OrderbookAggregator` service.

use std::pin::Pin;

use tokio::sync::watch;
use tokio_stream::{wrappers::WatchStream, Stream, StreamExt};
use tonic::{Request, Response, Status};

use crate::types::Summary;

pub mod proto {
    tonic::include_proto!("orderbook");
}

use proto::orderbook_aggregator_server::OrderbookAggregator;

pub struct OrderbookService {
    summary_rx: watch::Receiver<Summary>,
}

impl OrderbookService {
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
        let stream = WatchStream::new(rx).map(|summary| Ok(to_proto(summary)));
        Ok(Response::new(Box::pin(stream)))
    }
}

fn to_proto(summary: Summary) -> proto::Summary {
    proto::Summary {
        spread: summary.spread,
        bids: summary
            .bids
            .into_iter()
            .map(|l| proto::Level {
                exchange: l.exchange,
                price: l.price,
                amount: l.amount,
            })
            .collect(),
        asks: summary
            .asks
            .into_iter()
            .map(|l| proto::Level {
                exchange: l.exchange,
                price: l.price,
                amount: l.amount,
            })
            .collect(),
    }
}
