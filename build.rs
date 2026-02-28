//! Build script: git commit embedding + optional proto codegen.
//!
//! Embeds `GIT_COMMIT` at compile time so the running binary can report its
//! exact source revision (logged at startup, useful for incident response).
//!
//! Proto codegen is opt-in via the `codegen` feature. By default, the
//! checked-in `src/gen/orderbook.rs` is used directly -- no protoc needed.
//! To regenerate after editing `proto/orderbook.proto`:
//!
//! ```bash
//! cargo build --features codegen
//! cp target/*/build/orderbook-aggregator-*/out/orderbook.rs src/gen/
//! ```

fn main() {
    // Embed short git commit hash for version traceability.
    let commit = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map_or_else(|| "unknown".into(), |s| s.trim().to_string());
    println!("cargo:rustc-env=GIT_COMMIT={commit}");

    #[cfg(feature = "codegen")]
    tonic_build::compile_protos("proto/orderbook.proto").expect("failed to compile protos");
}
