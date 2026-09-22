//! The memory a node will let its requests take, and what it does when they
//! ask for more.
//!
//! `_nodes/stats` has always reported four breakers with the limits the
//! reference derives from its heap. They were a report and nothing else: the
//! limits were computed for the answer and never consulted, `tripped` was the
//! literal 0, and a node's only protection against a request larger than its
//! memory was the kernel's out-of-memory killer. A search that asked for more
//! than the machine had did not fail: the node died, taking every other
//! request with it, and a `kill -9` is exactly the case the restart check
//! covers -- so nothing was lost, and everything in flight was.
//!
//! The four are accounted here, and three of them refuse:
//!
//! - `in_flight_requests` holds the bytes of the request bodies being read at
//!   this moment. A hundred concurrent 100MB bulks are 10GB of memory before
//!   a single document is indexed, and the body limit alone cannot see them
//!   together.
//! - `request` holds what the aggregations of a search may build. A search
//!   that aggregates is given a budget out of this breaker and VeloCore is
//!   held to it while it collects, so the sum of what every aggregating
//!   search on the node may hold is the breaker's limit and not more.
//! - `parent` is the whole of it: the other breakers' reservations, and --
//!   because most of what a node holds was never reserved by anything -- the
//!   memory the allocator actually has, which is what `use_real_memory`
//!   means in the reference and what it means here.
//! - `fielddata` has nothing behind it. There is no fielddata cache in this
//!   engine; doc values are read from the index rather than loaded onto a
//!   heap, so the breaker is reported with a limit and a used of 0 because
//!   that is true, not because it is unmeasured.
//!
//! Every limit is a cluster setting the reference already has
//! (`indices.breaker.*.limit`, `indices.breaker.total.use_real_memory`), read
//! at the moment it is needed so that raising one takes effect on the next
//! request rather than the next restart. A refusal is the reference's:
//! `circuit_breaking_exception`, 429, with the bytes wanted and the bytes
//! allowed.

use crate::store::Store;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// One breaker: what it holds now, and how often it has refused.
pub struct Breaker {
    pub name: &'static str,
    /// the cluster setting that says how much it may hold
    setting: &'static str,
    /// the share of the machine's memory it holds when nothing says otherwise
    default_percent: u64,
    /// what the reference multiplies an estimate by before comparing it
    pub overhead: f64,
    used: AtomicU64,
    tripped: AtomicU64,
}

impl Breaker {
    const fn new(
        name: &'static str,
        setting: &'static str,
        default_percent: u64,
        overhead: f64,
    ) -> Breaker {
        Breaker {
            name,
            setting,
            default_percent,
            overhead,
            used: AtomicU64::new(0),
            tripped: AtomicU64::new(0),
        }
    }

    /// The bytes reserved through this breaker at this moment.
    pub fn used(&self) -> u64 {
        self.used.load(Relaxed)
    }

    /// How many requests this breaker has refused since the node started.
    pub fn tripped(&self) -> u64 {
        self.tripped.load(Relaxed)
    }

    /// What this breaker may hold: the cluster setting if there is one, a
    /// share of the machine's memory if there is not.
    ///
    /// A setting is either a size (`4gb`) or a share (`60%`), as the
    /// reference writes them.
    pub fn limit(&self, store: Option<&Store>) -> u64 {
        let total = machine_memory();
        let said = store.and_then(|s| s.cluster_setting(self.setting)).and_then(|v| {
            v.as_str().map(|s| s.to_string()).or_else(|| v.as_u64().map(|n| n.to_string()))
        });
        match said.as_deref().and_then(|s| limit_of(s, total)) {
            Some(bytes) => bytes,
            None => total * self.default_percent / 100,
        }
    }

    /// Take `bytes` out of this breaker for as long as the reservation lives.
    ///
    /// `label` is what the reference calls the thing being held -- it is
    /// named in the refusal, because "data too large" without saying what
    /// data tells an operator nothing.
    pub fn reserve(
        &'static self,
        bytes: u64,
        label: &str,
        store: Option<&Store>,
    ) -> Result<Reservation, Trip> {
        let want = (bytes as f64 * self.overhead) as u64;
        let limit = self.limit(store);
        let now = self.used.fetch_add(want, Relaxed) + want;
        if now > limit {
            self.used.fetch_sub(want, Relaxed);
            self.tripped.fetch_add(1, Relaxed);
            return Err(Trip {
                breaker: self.name,
                label: label.to_string(),
                wanted: want,
                limit,
                durability: "TRANSIENT",
            });
        }
        Ok(Reservation { breaker: self, bytes: want })
    }

    /// Count a refusal this breaker made somewhere other than `reserve` --
    /// the aggregation budget, which VeloCore enforces while it collects.
    pub fn count_trip(&self) {
        self.tripped.fetch_add(1, Relaxed);
    }
}

/// The bytes a request holds, given back when it is done with them.
pub struct Reservation {
    breaker: &'static Breaker,
    bytes: u64,
}

impl Reservation {
    /// How much this reservation took.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.breaker.used.fetch_sub(self.bytes, Relaxed);
    }
}

/// A breaker that would not hold what was asked of it.
#[derive(Debug, Clone)]
pub struct Trip {
    pub breaker: &'static str,
    pub label: String,
    pub wanted: u64,
    pub limit: u64,
    pub durability: &'static str,
}

impl Trip {
    /// The refusal, in the shape the reference gives it: the type a client
    /// retries on, the bytes it asked for and the bytes it was allowed.
    pub fn response(&self) -> Response {
        self.response_saying(&self.reason())
    }

    /// What this trip says, where the reason is worded somewhere else -- the
    /// engine's own complaint, when it is the engine that reached the limit.
    pub fn response_saying(&self, reason: &str) -> Response {
        let reason = reason.to_string();
        let body = json!({
            "error": {
                "type": "circuit_breaking_exception",
                "reason": reason,
                "bytes_wanted": self.wanted,
                "bytes_limit": self.limit,
                "durability": self.durability,
                "root_cause": [{
                    "type": "circuit_breaking_exception",
                    "reason": reason,
                    "bytes_wanted": self.wanted,
                    "bytes_limit": self.limit,
                    "durability": self.durability,
                }],
            },
            "status": 429,
        });
        let mut r = (StatusCode::TOO_MANY_REQUESTS, axum::Json(body)).into_response();
        r.extensions_mut().insert(crate::api::shared::ErrorKind {
            kind: "circuit_breaking_exception".to_string(),
            reason,
        });
        r
    }

    /// The reference's wording for a breaker that would not hold what it was
    /// asked to.
    fn reason(&self) -> String {
        format!(
            "[{}] Data too large, data for [{}] would be [{}/{}], which is larger than the limit of [{}/{}]",
            self.breaker,
            self.label,
            self.wanted,
            crate::api::shared::sized(None, self.wanted),
            self.limit,
            crate::api::shared::sized(None, self.limit),
        )
    }
}

/// The bytes of the request bodies being read at this moment.
pub static IN_FLIGHT: Breaker =
    Breaker::new("in_flight_requests", "network.breaker.inflight_requests.limit", 100, 2.0);

/// What the aggregations of the searches running at this moment may build.
pub static REQUEST: Breaker = Breaker::new("request", "indices.breaker.request.limit", 60, 1.0);

/// Reported, and honestly empty: there is no fielddata cache to fill.
pub static FIELDDATA: Breaker =
    Breaker::new("fielddata", "indices.breaker.fielddata.limit", 40, 1.03);

/// The whole of what the node holds, the allocator included.
pub static PARENT: Breaker = Breaker::new("parent", "indices.breaker.total.limit", 95, 1.0);

/// Every breaker, in the order `_nodes/stats` lists them.
pub fn all() -> [&'static Breaker; 4] {
    [&REQUEST, &FIELDDATA, &IN_FLIGHT, &PARENT]
}

/// What the parent breaker counts: the memory the allocator holds when
/// `use_real_memory` is on, and the other breakers' reservations when it is
/// not.
pub fn parent_used(store: Option<&Store>) -> u64 {
    if real_memory_wanted(store) {
        real_memory().max(IN_FLIGHT.used() + REQUEST.used())
    } else {
        IN_FLIGHT.used() + REQUEST.used() + FIELDDATA.used()
    }
}

/// Whether the parent breaker is held to the memory the process actually has.
fn real_memory_wanted(store: Option<&Store>) -> bool {
    store
        .and_then(|s| s.cluster_setting("indices.breaker.total.use_real_memory"))
        .map(|v| v.as_bool().unwrap_or_else(|| v.as_str() == Some("true")))
        .unwrap_or(true)
}

/// Refuse the request if the node is already holding more than the parent
/// breaker allows.
///
/// This is the check that stands between a node and the out-of-memory killer:
/// what it reads is the memory the allocator has committed, which counts
/// every index, every buffer and every request, reserved or not.
pub fn check_parent(label: &str, store: Option<&Store>) -> Result<(), Trip> {
    let limit = PARENT.limit(store);
    let used = parent_used(store);
    if used > limit {
        PARENT.count_trip();
        return Err(Trip {
            breaker: "parent",
            label: label.to_string(),
            wanted: used,
            limit,
            durability: "PERMANENT",
        });
    }
    Ok(())
}

/// The machine's memory, which stands for the reference's heap: it is what a
/// node can grow into before the kernel stops it.
fn machine_memory() -> u64 {
    static TOTAL: AtomicU64 = AtomicU64::new(0);
    let seen = TOTAL.load(Relaxed);
    if seen != 0 {
        return seen;
    }
    let total = crate::api::sysinfo::memory().total.max(1);
    TOTAL.store(total, Relaxed);
    total
}

/// The memory the allocator holds, sampled rather than asked for every time.
///
/// The parent breaker is consulted on every request that can take memory, and
/// the allocator's own accounting walks its arenas. A reading a tenth of a
/// second old is as good an answer for a limit measured in gigabytes, and
/// costs nothing.
fn real_memory() -> u64 {
    const FRESH_MILLIS: u64 = 100;
    static AT: AtomicU64 = AtomicU64::new(0);
    static VALUE: AtomicU64 = AtomicU64::new(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let taken = AT.load(Relaxed);
    if now.saturating_sub(taken) < FRESH_MILLIS && taken != 0 {
        return VALUE.load(Relaxed);
    }
    let (committed, _peak) = crate::api::sysinfo::allocator();
    VALUE.store(committed, Relaxed);
    AT.store(now, Relaxed);
    committed
}

/// The layer that holds a request to the node's memory.
///
/// Two things are asked of every request that can take memory: that the body
/// it is about to be read into fits in what the node has left for bodies, and
/// that the node is not already past its parent limit. Both are the
/// reference's breakers; neither could be asked before, because nothing was
/// counting.
pub async fn layer(
    axum::extract::State(store): axum::extract::State<Store>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let pool = crate::api::pools::pool_for(request.method(), request.uri().path());
    // a request that neither holds a body nor builds anything is not worth a
    // reading of the allocator: `_cat`, `_cluster/health`, a node asking
    // another what it is
    let weighs = matches!(pool, "search" | "write" | "get");
    let body = request
        .headers()
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    let _held = if body > 0 {
        match IN_FLIGHT.reserve(body, "<http_request>", Some(&store)) {
            Ok(held) => Some(held),
            Err(trip) => return trip.response(),
        }
    } else {
        None
    };
    if weighs && let Err(trip) = check_parent("<transport_request>", Some(&store)) {
        return trip.response();
    }
    next.run(request).await
}

/// A size as a setting writes one, where the share is of the machine's
/// memory: what a limit is written in, read from anywhere that needs one.
pub fn size_of(said: &str) -> Option<u64> {
    limit_of(said, machine_memory())
}

/// A limit as a setting writes one: a share of the machine, or a size.
fn limit_of(said: &str, total: u64) -> Option<u64> {
    let s = said.trim();
    if let Some(pct) = s.strip_suffix('%') {
        let share: f64 = pct.trim().parse().ok()?;
        if !(0.0..=100.0).contains(&share) {
            return None;
        }
        return Some((total as f64 * share / 100.0) as u64);
    }
    crate::api::shared::parse_size(s).or_else(|| s.parse::<u64>().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    static TESTING: Breaker = Breaker::new("testing", "indices.breaker.testing.limit", 1, 1.0);

    #[test]
    fn a_limit_is_a_share_or_a_size() {
        assert_eq!(limit_of("50%", 1000), Some(500));
        assert_eq!(limit_of("1kb", 1000), Some(1024));
        assert_eq!(limit_of("4096", 1000), Some(4096));
        assert_eq!(limit_of("200%", 1000), None);
        assert_eq!(limit_of("a lot", 1000), None);
    }

    #[test]
    fn a_reservation_is_given_back_when_it_is_dropped() {
        let before = TESTING.used();
        {
            let _held = TESTING.reserve(1024, "a test", None).expect("under the limit");
            assert_eq!(TESTING.used(), before + 1024);
        }
        assert_eq!(TESTING.used(), before);
    }

    #[test]
    fn more_than_the_limit_is_refused_and_counted() {
        let tripped = TESTING.tripped();
        let limit = TESTING.limit(None);
        let Err(trip) = TESTING.reserve(limit + 1, "a test", None) else {
            panic!("over the limit")
        };
        assert_eq!(trip.breaker, "testing");
        assert_eq!(trip.limit, limit);
        // a refusal leaves nothing held behind it
        assert_eq!(TESTING.used(), 0);
        assert_eq!(TESTING.tripped(), tripped + 1);
    }

    #[test]
    fn a_refusal_is_the_reference_s_own_shape() {
        let trip = Trip {
            breaker: "request",
            label: "a search".to_string(),
            wanted: 9,
            limit: 4,
            durability: "T",
        };
        let r = trip.response();
        assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
        let kind = r.extensions().get::<crate::api::shared::ErrorKind>().expect("kind");
        assert_eq!(kind.kind, "circuit_breaking_exception");
    }
}
