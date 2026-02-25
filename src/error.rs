//! Unified error types for the order book aggregator.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("WebSocket error: {0}")]
    WebSocket(#[from] tokio_tungstenite::tungstenite::Error),

    #[error("gRPC transport error: {0}")]
    Transport(#[from] tonic::transport::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
