//! Library surface so benchmarks can drive the same code the server does.
// A handler answers with an axum `Response` either way, so the error half of
// its Result is as large as the ok half. Boxing it would put an allocation in
// front of every error a request can produce, to satisfy a lint about a shape
// that is deliberate.
#![allow(clippy::result_large_err)]
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
