//! Application-level error type for the server entry point.
//!
//! Covers the small set of fallible operations in `main()`: port binding,
//! thread spawning, and gRPC serving. Parse errors on the hot path use
//! `Option` (not `Result`) -- no error variant needed for those.

use std::fmt;

/// Top-level error for the order book aggregator server.
#[derive(Debug)]
pub enum Error {
    /// Failed to bind a TCP listener (gRPC or metrics port).
    Bind(std::io::Error),
    /// Failed to spawn an OS thread.
    Spawn(std::io::Error),
    /// gRPC transport error.
    Transport(tonic::transport::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bind(e) => write!(f, "failed to bind port: {e}"),
            Self::Spawn(e) => write!(f, "failed to spawn thread: {e}"),
            Self::Transport(e) => write!(f, "gRPC transport error: {e}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Bind(e) | Self::Spawn(e) => Some(e),
            Self::Transport(e) => Some(e),
        }
    }
}

impl From<tonic::transport::Error> for Error {
    fn from(e: tonic::transport::Error) -> Self {
        Self::Transport(e)
    }
}
