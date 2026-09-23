//! Library surface so benchmarks can drive the same code the server does.
// A handler answers with an axum `Response` either way, so the error half of
// its Result is as large as the ok half. Boxing it would put an allocation in
// front of every error a request can produce, to satisfy a lint about a shape
// that is deliberate.
#![allow(clippy::result_large_err)]

/// The OpenSearch this answers as: what `GET /` reports, what a client reads
/// to decide which API it is talking to, and what `_nodes` says every node
/// is.
///
/// It is not this project's own version, which is `VERSION` below and comes
/// from the manifest. CHANGELOG.md says why the two are different numbers:
/// one moves when the API this targets moves, the other when this project
/// releases. They were the same literal `3.9.0` written out in twelve places,
/// so moving the target meant finding all twelve.
pub const OPENSEARCH_VERSION: &str = "3.9.0";

/// This project's own version, from the manifest, so that a build cannot
/// report a version the crate was not built as.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The commit this binary was built from, compiled in by `build.rs`.
///
/// A gate that asks a node for this and compares it with the binary it meant
/// to start can tell that it is counting the right build's answers; without
/// it, a node left over from another session on the same port is
/// indistinguishable from the one just started.
pub fn build_hash() -> &'static str {
    env!("VELOSEARCH_BUILD_HASH")
}

pub mod analysis;
pub mod api;
pub mod blockstats;
pub mod breaker;
pub mod cluster;
pub mod console;
pub mod hdr;
pub mod http_compat;
pub mod ingest;
pub mod ism;
pub mod knn;
pub mod painless;
pub mod query;
pub mod search;
pub mod security;
pub mod snapshot;
pub mod source;
pub mod sql;
pub mod store;
pub mod tasks;
pub mod tls;
pub mod tz;
