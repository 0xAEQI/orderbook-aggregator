//! Regenerates proto types when the `codegen` feature is enabled.
//!
//! By default, the checked-in `src/gen/orderbook.rs` is used directly -- no
//! protoc or build-time codegen needed. To regenerate after editing
//! `proto/orderbook.proto`:
//!
//! ```bash
//! cargo build --features codegen
//! cp target/*/build/orderbook-aggregator-*/out/orderbook.rs src/gen/
//! ```

fn main() {
    #[cfg(feature = "codegen")]
    tonic_build::compile_protos("proto/orderbook.proto").expect("failed to compile protos");
}
