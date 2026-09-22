//! The thread pools a request passes through, as `_nodes/stats` and
//! `_cat/thread_pool` report them -- and, for the pools that are bounded, the
//! queue it waits in and the refusal it gets when that queue is full.
//!
//! OpenSearch hands each request to a pool named for its kind of work --
//! `search`, `write`, `get` -- and counts what each pool is running, has
//! queued, has refused and has finished. A request here runs on the async
//! runtime's workers rather than in a pool of its own, so those columns read
//! 0 however busy the node was, and a zero there was taken as evidence that
//! nothing was waiting. Every request is now counted under the pool
//! OpenSearch would have given it: `active` is how many of its kind are
//! running at this moment, `completed` how many have finished, `rejected` how
//! many were answered 429.
//!
//! Counting is not the whole of what a pool is for. A node with no ceiling on
//! how many searches it runs at once does not refuse the ten thousandth one:
//! it accepts it, and every one of the ten thousand gets slower, until the
//! memory they hold between them is more than the machine has. The reference
//! answers that with a bounded pool and a bounded queue in front of it, and
//! `429 rejected_execution_exception` when the queue is full -- a client can
//! read that and back off, which it cannot do with a request that merely
//! takes a minute.
//!
//! So each bounded pool here has a number of requests it will run at once and
//! a queue of the ones waiting for a turn. A request of a bounded kind takes
//! a place before it reaches its handler and gives it back when the answer is
//! written -- when the client goes away mid-request as much as when it does
//! not. `queue` is then what is waiting for that pool, measured rather than
//! assumed; for `generic`, which has no ceiling, it stays the runtime's own
//! backlog.
//!
//! How many at once is not the reference's thread count. A thread there is
//! busy for the whole of a request; a request here gives its worker up
//! whenever it waits for a disk or another node, so holding it to one request
//! per thread would leave the node idle under load. The default is twice the
//! size the reference gives the pool, and `VELOSEARCH_THREAD_POOL_<pool>_SIZE`
//! and `_QUEUE_SIZE` say otherwise -- `0` for a queue means refuse rather than
//! wait, and `-1` means a queue with no end, which is what the admission
//! control was before it existed.

use axum::extract::Request;
use axum::http::{Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use tokio::sync::Semaphore;

/// One pool: how it is sized, what is waiting for it, and what it has done.
pub struct Pool {
    pub name: &'static str,
    /// `fixed` or `scaling`, as the reference sizes it
    pub kind: &'static str,
    /// how long a queue the reference puts in front of it; `-1` for a pool
    /// that takes everything it is given
    reference_queue: i64,
    active: AtomicU64,
    largest: AtomicU64,
    completed: AtomicU64,
    rejected: AtomicU64,
    waiting: AtomicU64,
    places: OnceLock<Semaphore>,
}

impl Pool {
    const fn new(name: &'static str, kind: &'static str, reference_queue: i64) -> Pool {
        Pool {
            name,
            kind,
            reference_queue,
            active: AtomicU64::new(0),
            largest: AtomicU64::new(0),
            completed: AtomicU64::new(0),
            rejected: AtomicU64::new(0),
            waiting: AtomicU64::new(0),
            places: OnceLock::new(),
        }
    }

    pub fn active(&self) -> u64 {
        self.active.load(Relaxed)
    }

    pub fn largest(&self) -> u64 {
        self.largest.load(Relaxed)
    }

    pub fn completed(&self) -> u64 {
        self.completed.load(Relaxed)
    }

    pub fn rejected(&self) -> u64 {
        self.rejected.load(Relaxed)
    }

    /// The backlog waiting for this pool: what it is holding back where it
    /// is bounded, the runtime's own for `generic`.
    pub fn queue(&self) -> u64 {
        if self.name == "generic" {
            return tokio::runtime::Handle::try_current()
                .map(|h| h.metrics().global_queue_depth() as u64)
                .unwrap_or(0);
        }
        self.waiting.load(Relaxed)
    }

    /// How many requests of this kind the node runs at once, `None` where it
    /// runs as many as it is given.
    ///
    /// A scaling pool has no ceiling in the reference and gets none here.
    pub fn at_once(&self) -> Option<usize> {
        if self.kind != "fixed" || !admission_on() {
            return None;
        }
        match said_number(self.name, "SIZE") {
            Some(n) if n <= 0 => None,
            Some(n) => Some(n as usize),
            // twice the reference's thread count: see the note at the top --
            // a request here gives its worker up whenever it waits
            None => Some((self.size() as usize * 2).max(4)),
        }
    }

    /// How many requests of this kind may wait for a turn; `None` for a queue
    /// with no end.
    pub fn queue_size(&self) -> Option<usize> {
        self.at_once()?;
        match said_number(self.name, "QUEUE_SIZE") {
            Some(n) if n < 0 => None,
            Some(n) => Some(n as usize),
            None if self.reference_queue < 0 => None,
            None => Some(self.reference_queue as usize),
        }
    }

    /// The places this pool hands out, made on first use because how many
    /// there are is read from the machine and the environment.
    fn places(&self) -> Option<&Semaphore> {
        let at_once = self.at_once()?;
        Some(self.places.get_or_init(|| Semaphore::new(at_once)))
    }

    /// How many threads the pool has: what a fixed pool is sized to, the
    /// runtime's workers for `generic`, and for a scaling pool the most it
    /// has had running at once.
    pub fn threads(&self) -> u64 {
        if self.name == "generic" {
            return tokio::runtime::Handle::try_current()
                .map(|h| h.metrics().num_workers() as u64)
                .unwrap_or(0)
                .max(self.largest());
        }
        match self.kind {
            // a node told how many of this kind to run at once reports that,
            // rather than the size the reference would have given it
            "fixed" => match said_number(self.name, "SIZE") {
                Some(n) if n > 0 => n as u64,
                _ => self.size(),
            },
            _ => self.largest(),
        }
    }

    /// The size the reference gives a fixed pool on a machine of this many
    /// processors; a scaling pool's largest.
    pub fn size(&self) -> u64 {
        let cpus = std::thread::available_parallelism().map(|n| n.get() as u64).unwrap_or(1);
        match self.name {
            "search" => cpus * 3 / 2 + 1,
            "write" | "get" | "system_write" => cpus,
            "analyze" => 1,
            "force_merge" => 1,
            "search_throttled" => 1,
            "index_searcher" => cpus * 2,
            "listener" => (cpus / 2).clamp(1, 10),
            _ => self.largest().max(1),
        }
    }
}

/// Every pool, in name order, as the reference lists them, with the queue it
/// puts in front of each.
pub static POOLS: [Pool; 16] = [
    Pool::new("analyze", "fixed", 16),
    Pool::new("fetch_shard_started", "scaling", -1),
    Pool::new("fetch_shard_store", "scaling", -1),
    Pool::new("flush", "scaling", -1),
    Pool::new("force_merge", "fixed", -1),
    Pool::new("generic", "scaling", -1),
    Pool::new("get", "fixed", 1000),
    Pool::new("index_searcher", "fixed", 1000),
    Pool::new("listener", "fixed", -1),
    Pool::new("management", "scaling", -1),
    Pool::new("refresh", "scaling", -1),
    Pool::new("search", "fixed", 1000),
    Pool::new("search_throttled", "fixed", 100),
    Pool::new("snapshot", "scaling", -1),
    Pool::new("warmer", "scaling", -1),
    Pool::new("write", "fixed", 10000),
];

/// Whether this node holds its requests to its pools at all.
///
/// `VELOSEARCH_THREAD_POOL_ADMISSION=off` takes the ceilings away, for a run
/// that wants the node to attempt whatever it is given.
fn admission_on() -> bool {
    !matches!(
        std::env::var("VELOSEARCH_THREAD_POOL_ADMISSION").as_deref(),
        Ok("off") | Ok("false") | Ok("0")
    )
}

/// A number said about one pool in the environment, if one was.
fn said_number(pool: &str, what: &str) -> Option<i64> {
    std::env::var(format!("VELOSEARCH_THREAD_POOL_{}_{what}", pool.to_uppercase()))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// What a pool answers when it has neither a place nor room to wait for one.
///
/// The reference's own refusal, down to the type a client's back-off reads.
fn too_busy(pool: &Pool, queue: usize) -> Response {
    let reason = format!(
        "rejected execution of coordinating operation [thread_pool_name = {}, queue_capacity = {}, \
         task_count = {}]",
        pool.name,
        queue,
        pool.active() + pool.queue(),
    );
    let body = serde_json::json!({
        "error": {
            "type": "rejected_execution_exception",
            "reason": reason,
            "root_cause": [{"type": "rejected_execution_exception", "reason": reason}],
        },
        "status": 429,
    });
    let mut r = (StatusCode::TOO_MANY_REQUESTS, axum::Json(body)).into_response();
    r.extensions_mut().insert(crate::api::shared::ErrorKind {
        kind: "rejected_execution_exception".to_string(),
        reason,
    });
    r
}

fn pool(name: &str) -> &'static Pool {
    POOLS.iter().find(|p| p.name == name).unwrap_or(&POOLS[5])
}

/// The pool OpenSearch runs a request of this method and path in.
pub fn pool_for(method: &Method, path: &str) -> &'static str {
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let has = |name: &str| parts.contains(&name);
    if has("_search") || has("_msearch") || has("_count") || has("_field_caps") || has("_explain") {
        return "search";
    }
    if has("_bulk") || has("_update") || has("_delete_by_query") || has("_update_by_query") {
        return "write";
    }
    if has("_doc") || has("_create") || has("_source") {
        return match *method {
            Method::GET | Method::HEAD => "get",
            _ => "write",
        };
    }
    if has("_mget") || has("_termvectors") || has("_mtermvectors") {
        return "get";
    }
    if has("_refresh") {
        return "refresh";
    }
    if has("_flush") {
        return "flush";
    }
    if has("_forcemerge") {
        return "force_merge";
    }
    if has("_analyze") {
        return "analyze";
    }
    if has("_snapshot") {
        return "snapshot";
    }
    if parts.first().map(|p| p.starts_with('_')).unwrap_or(true) {
        return "management";
    }
    "generic"
}

/// The layer that holds every request to its pool and counts it there.
///
/// A place is taken before the request reaches anything that can allocate on
/// its behalf and given back once the answer is written. Where the pool is
/// full and its queue is full too, the request is refused here rather than
/// accepted into a node that cannot get to it.
pub async fn track(request: Request, next: Next) -> Response {
    let p = pool(pool_for(request.method(), request.uri().path()));
    let _place = match p.places() {
        None => None,
        Some(places) => match places.try_acquire() {
            Ok(place) => Some(place),
            Err(_) => {
                let room = p.queue_size().unwrap_or(usize::MAX);
                if p.waiting.load(Relaxed) as usize >= room {
                    p.rejected.fetch_add(1, Relaxed);
                    return too_busy(p, room);
                }
                // a request that waits is in the queue while it waits, and
                // out of it however it leaves -- taken, refused, or dropped
                // because the client went away
                struct Waiting(&'static Pool);
                impl Drop for Waiting {
                    fn drop(&mut self) {
                        self.0.waiting.fetch_sub(1, Relaxed);
                    }
                }
                p.waiting.fetch_add(1, Relaxed);
                let queued = Waiting(p);
                let got = places.acquire().await.ok();
                drop(queued);
                match got {
                    Some(place) => Some(place),
                    // the semaphore is never closed; this is unreachable
                    None => return too_busy(p, room),
                }
            }
        },
    };
    let now = p.active.fetch_add(1, Relaxed) + 1;
    p.largest.fetch_max(now, Relaxed);
    // a request whose client goes away is dropped mid-flight; the count it
    // took is given back either way
    struct Done(&'static Pool);
    impl Drop for Done {
        fn drop(&mut self) {
            self.0.active.fetch_sub(1, Relaxed);
            self.0.completed.fetch_add(1, Relaxed);
        }
    }
    let done = Done(p);
    let response = next.run(request).await;
    if response.status() == axum::http::StatusCode::TOO_MANY_REQUESTS {
        p.rejected.fetch_add(1, Relaxed);
    }
    drop(done);
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_is_counted_under_the_pool_the_reference_runs_it_in() {
        assert_eq!(pool_for(&Method::POST, "/orders/_search"), "search");
        assert_eq!(pool_for(&Method::PUT, "/orders/_doc/1"), "write");
        assert_eq!(pool_for(&Method::GET, "/orders/_doc/1"), "get");
        assert_eq!(pool_for(&Method::POST, "/_bulk"), "write");
        assert_eq!(pool_for(&Method::GET, "/_cat/indices"), "management");
        assert_eq!(pool_for(&Method::PUT, "/orders"), "generic");
    }
}
