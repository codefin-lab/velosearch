//! What one search may spend: how long it may take, and how much memory its
//! aggregations may build.
//!
//! A search used to be given neither. `timeout` was read out of the request,
//! checked for being a time value, and then ignored -- every answer said
//! `"timed_out": false` because the field was written, not measured -- and a
//! query that walked a billion documents walked all of them however long the
//! caller was prepared to wait. The aggregations were handed VeloCore's
//! default ceiling of 500MB, per search, with nothing counting how many
//! searches were aggregating at once.
//!
//! Both are held here, in one object that a search carries from the moment it
//! is parsed to the moment its answer is written:
//!
//! - **the clock.** The deadline the request asked for, or the one the
//!   cluster sets with `search.default_search_timeout` for the requests that
//!   ask for nothing. It is read while the walk is running -- every few
//!   thousand documents, and again at each segment -- so a search that is
//!   past it stops collecting and answers with what it has and
//!   `"timed_out": true`, which is what the reference does and what a
//!   dashboard behind a thirty-second gateway needs it to do.
//! - **the budget.** A share of the `request` breaker, reserved for as long
//!   as the search runs and handed to VeloCore as the ceiling its
//!   aggregations are held to. The share is the breaker's limit divided by
//!   how many searches the node runs at once, so the searches aggregating at
//!   any moment cannot between them hold more than the breaker allows: the
//!   one after that is refused with `circuit_breaking_exception` instead of
//!   being let in to finish the machine off.
//!
//! The clock is also where the parent breaker is read during a walk. A search
//! that was admitted when the node had room can run into a node that no
//! longer does -- a neighbouring bulk, a merge, another search -- and the
//! walk gives up rather than taking the node down with it.

use super::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::Instant;
use velocore::aggregation::AggregationLimitsGuard;

/// How many documents a segment collects between readings of the clock.
///
/// Reading it per document would cost more than the collection; a few
/// thousand documents is under a millisecond of work on any shape measured
/// here, so a deadline is never overrun by anything a caller would notice.
const DOCS_BETWEEN_READINGS: u32 = 4096;

/// The deadline a search is held to, and what became of it.
///
/// Shared, because the shards of a search are walked on other threads: one
/// clock, read by all of them.
pub struct Clock {
    until: Option<Instant>,
    /// the parent breaker's limit as it was when the search started, or
    /// `None` where the walk is not to consult it
    parent_limit: Option<u64>,
    hit: AtomicBool,
    broke: AtomicBool,
}

impl Clock {
    /// A clock with no deadline and no ceiling: for the walks the server runs
    /// for itself, where there is no caller waiting.
    pub fn none() -> Arc<Clock> {
        Arc::new(Clock { until: None, parent_limit: None, hit: false.into(), broke: false.into() })
    }

    /// Whether this search is past what it was given.
    ///
    /// Sticky: once a search has run out of time, every thread of it sees
    /// that at its next reading without asking the clock again.
    pub fn expired(&self) -> bool {
        if self.hit.load(Relaxed) {
            return true;
        }
        let Some(until) = self.until else { return false };
        if Instant::now() >= until {
            self.hit.store(true, Relaxed);
            return true;
        }
        false
    }

    /// Whether the node has gone past its parent breaker while this search
    /// was running.
    pub fn over_memory(&self) -> bool {
        if self.broke.load(Relaxed) {
            return true;
        }
        let Some(limit) = self.parent_limit else { return false };
        if crate::breaker::parent_used(None) > limit {
            self.broke.store(true, Relaxed);
            crate::breaker::PARENT.count_trip();
            return true;
        }
        false
    }

    /// Whether the walk is to stop where it is.
    fn stop(&self) -> bool {
        self.expired() || self.over_memory()
    }

    /// Whether this search ran out of time, as the answer reports it.
    pub fn timed_out(&self) -> bool {
        self.hit.load(Relaxed)
    }

    /// The refusal for a search the node could not afford to finish, if that
    /// is how it ended.
    pub fn broken(&self) -> Option<crate::breaker::Trip> {
        if !self.broke.load(Relaxed) {
            return None;
        }
        Some(crate::breaker::Trip {
            breaker: "parent",
            label: "<search>".to_string(),
            wanted: crate::breaker::parent_used(None),
            limit: self.parent_limit.unwrap_or(0),
            durability: "TRANSIENT",
        })
    }
}

/// A collector held to a clock.
///
/// It collects what the collector it wraps would collect, up to the moment
/// the search runs out of time or the node runs out of memory; after that it
/// collects nothing, and what was gathered before is the answer. A segment
/// the walk never reached is not collected at all.
pub struct Timed<'a, C> {
    inner: &'a C,
    clock: &'a Arc<Clock>,
}

impl<'a, C: velocore::collector::Collector> Timed<'a, C> {
    pub fn new(inner: &'a C, clock: &'a Arc<Clock>) -> Timed<'a, C> {
        Timed { inner, clock }
    }
}

pub struct TimedChild<S> {
    inner: S,
    clock: Arc<Clock>,
    until_reading: u32,
    stopped: bool,
}

impl<S: velocore::collector::SegmentCollector> TimedChild<S> {
    /// Whether this segment has stopped collecting, reading the clock once
    /// every `DOCS_BETWEEN_READINGS` documents.
    #[inline]
    fn done(&mut self, docs: u32) -> bool {
        if self.stopped {
            return true;
        }
        if self.until_reading > docs {
            self.until_reading -= docs;
            return false;
        }
        self.until_reading = DOCS_BETWEEN_READINGS;
        if self.clock.stop() {
            self.stopped = true;
            return true;
        }
        false
    }
}

impl<C: velocore::collector::Collector> velocore::collector::Collector for Timed<'_, C> {
    type Fruit = C::Fruit;
    type Child = TimedChild<C::Child>;

    fn for_segment(
        &self,
        ord: velocore::SegmentOrdinal,
        reader: &velocore::SegmentReader,
    ) -> velocore::Result<Self::Child> {
        Ok(TimedChild {
            inner: self.inner.for_segment(ord, reader)?,
            clock: Arc::clone(self.clock),
            until_reading: DOCS_BETWEEN_READINGS,
            stopped: false,
        })
    }

    fn requires_scoring(&self) -> bool {
        self.inner.requires_scoring()
    }

    fn merge_fruits(
        &self,
        segment_fruits: Vec<<Self::Child as velocore::collector::SegmentCollector>::Fruit>,
    ) -> velocore::Result<Self::Fruit> {
        self.inner.merge_fruits(segment_fruits)
    }

    fn check_schema(&self, schema: &velocore::schema::Schema) -> velocore::Result<()> {
        self.inner.check_schema(schema)
    }

    /// A segment the search has no time left for is not walked at all: the
    /// collector is made so that its fruit has the shape the merge expects,
    /// and harvested empty.
    fn collect_segment(
        &self,
        weight: &dyn velocore::query::Weight,
        ord: velocore::SegmentOrdinal,
        reader: &velocore::SegmentReader,
    ) -> velocore::Result<<Self::Child as velocore::collector::SegmentCollector>::Fruit> {
        use velocore::collector::SegmentCollector;
        if self.clock.stop() {
            return Ok(self.for_segment(ord, reader)?.harvest());
        }
        self.inner.collect_segment(weight, ord, reader)
    }
}

impl<S: velocore::collector::SegmentCollector> velocore::collector::SegmentCollector
    for TimedChild<S>
{
    type Fruit = S::Fruit;

    fn collect(&mut self, doc: velocore::DocId, score: velocore::Score) {
        if self.done(1) {
            return;
        }
        self.inner.collect(doc, score);
    }

    fn collect_block(&mut self, docs: &[velocore::DocId]) {
        if self.done(docs.len() as u32) {
            return;
        }
        self.inner.collect_block(docs);
    }

    fn harvest(self) -> Self::Fruit {
        self.inner.harvest()
    }
}

/// What a search may spend, carried with it.
pub struct Budget {
    pub clock: Arc<Clock>,
    /// the ceiling VeloCore holds this search's aggregations to, shared by
    /// every shard of it
    aggs: AggregationLimitsGuard,
    /// the share of the `request` breaker that ceiling was taken out of,
    /// given back when the search is done
    _held: Option<crate::breaker::Reservation>,
    /// that this thread is inside a search, so the searches this one runs
    /// under it do not each take a share of their own
    _inside: Nested,
}

/// Whether this thread is already answering a search.
///
/// A search that runs searches of its own runs them on its own thread, one
/// after another, so this marks the outermost and is put back as it was when
/// that one is done -- the same shape as `limits::as_the_server`, which marks
/// a walk the server runs for itself.
pub struct Nested {
    was: Option<bool>,
}

thread_local! {
    static IN_A_SEARCH: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

impl Nested {
    /// Mark this thread as answering a search, remembering whether it
    /// already was.
    fn enter() -> Nested {
        Nested { was: Some(IN_A_SEARCH.with(|c| c.replace(true))) }
    }

    /// A mark that marks nothing, for a budget that is not a search's.
    fn none() -> Nested {
        Nested { was: None }
    }

    /// Whether the thread was inside a search before this one began.
    fn was_already_in_one(&self) -> bool {
        self.was.unwrap_or(false)
    }
}

impl Drop for Nested {
    fn drop(&mut self) {
        if let Some(was) = self.was {
            IN_A_SEARCH.with(|c| c.set(was));
        }
    }
}

impl Budget {
    /// The budget for a search: its deadline, and the memory its
    /// aggregations may build.
    ///
    /// Refused, rather than made, when the node has no room left for another
    /// aggregating search.
    pub fn of_search(
        store: &Store,
        body: &Value,
        p: &Params,
        aggregates: bool,
    ) -> std::result::Result<Budget, Response> {
        let until = deadline(store, body, p);
        let parent_limit = Some(crate::breaker::PARENT.limit(Some(store)));
        let clock = Arc::new(Clock {
            until,
            parent_limit,
            hit: AtomicBool::new(false),
            broke: AtomicBool::new(false),
        });
        // A search runs searches of its own -- an aggregation this engine
        // answers a bucket at a time, a `collapse`, a terms lookup -- and each
        // of them comes back through here. Only the outermost takes a budget:
        // an inner one taking a second share out of the breaker would refuse
        // the request for being two searches when it is one, and the share
        // the outer holds already covers what the inner builds under it.
        let inside = Nested::enter();
        if !aggregates || inside.was_already_in_one() {
            // nothing is built that the breaker would hold, or nothing new
            return Ok(Budget {
                clock,
                aggs: AggregationLimitsGuard::default(),
                _held: None,
                _inside: inside,
            });
        }
        let share = per_search(store);
        let held = crate::breaker::REQUEST
            .reserve(share, "<agg [search]>", Some(store))
            .map_err(|trip| trip.response())?;
        let buckets = crate::search::max_buckets(store).min(u32::MAX as u64) as u32;
        Ok(Budget {
            clock,
            aggs: AggregationLimitsGuard::new(Some(held.bytes()), Some(buckets)),
            _held: Some(held),
            _inside: inside,
        })
    }

    /// A budget for the searches the server runs for itself: no deadline, and
    /// VeloCore's own ceiling.
    pub fn unbounded() -> Budget {
        Budget {
            clock: Clock::none(),
            aggs: AggregationLimitsGuard::default(),
            _held: None,
            _inside: Nested::none(),
        }
    }

    /// The ceiling this search's aggregations are held to. Cloning shares the
    /// count, which is how every shard of one search is held to one budget.
    pub fn aggs(&self) -> AggregationLimitsGuard {
        self.aggs.clone()
    }

    /// Whether the search ran out of time.
    pub fn timed_out(&self) -> bool {
        self.clock.timed_out()
    }

    /// The refusal, where the node ran out of memory under the search.
    pub fn broken(&self) -> Option<Response> {
        self.clock.broken().map(|trip| trip.response())
    }
}

/// When this search is to stop: what it asked for, else what the cluster asks
/// of the searches that ask for nothing.
fn deadline(store: &Store, body: &Value, p: &Params) -> Option<Instant> {
    let said = body
        .get("timeout")
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .or_else(|| p.get("timeout").cloned())
        .or_else(|| {
            store
                .cluster_setting("search.default_search_timeout")
                .and_then(|v| v.as_str().map(|s| s.to_string()))
        })?;
    // `-1` and `0` are how the reference writes "no timeout"
    if matches!(said.trim(), "-1" | "0" | "") {
        return None;
    }
    let nanos = crate::api::shared::parse_time_value_nanos(&said)?;
    if nanos == 0 {
        return None;
    }
    Some(Instant::now() + std::time::Duration::from_nanos(nanos))
}

/// The share of the `request` breaker one aggregating search may hold.
///
/// The node runs so many searches at once and no more (see `api::pools`), so
/// dividing the breaker by that number is a budget whose sum cannot pass the
/// breaker's limit however many searches arrive. `indices.breaker.request.
/// per_search` says otherwise, as a size or a share of the machine.
fn per_search(store: &Store) -> u64 {
    let limit = crate::breaker::REQUEST.limit(Some(store));
    if let Some(said) = store.cluster_setting("indices.breaker.request.per_search")
        && let Some(bytes) = said
            .as_str()
            .map(|s| s.to_string())
            .or_else(|| said.as_u64().map(|n| n.to_string()))
            .and_then(|s| crate::breaker::size_of(&s))
    {
        return bytes.min(limit).max(1);
    }
    let at_once = crate::api::pools::POOLS
        .iter()
        .find(|p| p.name == "search")
        .and_then(|p| p.at_once())
        .unwrap_or(16) as u64;
    (limit / at_once.max(1)).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clock_with_no_deadline_never_expires() {
        let clock = Clock::none();
        assert!(!clock.expired());
        assert!(!clock.timed_out());
    }

    #[test]
    fn a_deadline_that_has_passed_is_sticky() {
        let clock = Arc::new(Clock {
            until: Some(Instant::now() - std::time::Duration::from_millis(1)),
            parent_limit: None,
            hit: AtomicBool::new(false),
            broke: AtomicBool::new(false),
        });
        assert!(clock.expired());
        assert!(clock.timed_out());
        assert!(clock.expired());
    }

    /// A segment collector that only says how much it was given.
    struct Counting(u64);
    impl velocore::collector::SegmentCollector for Counting {
        type Fruit = u64;
        fn collect(&mut self, _doc: velocore::DocId, _score: velocore::Score) {
            self.0 += 1;
        }
        fn harvest(self) -> u64 {
            self.0
        }
    }

    #[test]
    fn a_walk_collects_nothing_once_the_clock_has_run_out() {
        use velocore::collector::SegmentCollector;
        let clock = Arc::new(Clock {
            until: Some(Instant::now() - std::time::Duration::from_millis(1)),
            parent_limit: None,
            hit: AtomicBool::new(false),
            broke: AtomicBool::new(false),
        });
        let mut child = TimedChild { inner: Counting(0), clock, until_reading: 1, stopped: false };
        child.collect(1, 1.0);
        child.collect(2, 1.0);
        assert_eq!(child.harvest(), 0);
    }

    #[test]
    fn a_walk_within_its_time_collects_everything() {
        use velocore::collector::SegmentCollector;
        let clock = Arc::new(Clock {
            until: Some(Instant::now() + std::time::Duration::from_secs(60)),
            parent_limit: None,
            hit: AtomicBool::new(false),
            broke: AtomicBool::new(false),
        });
        let mut child = TimedChild { inner: Counting(0), clock, until_reading: 1, stopped: false };
        for doc in 0..10_000 {
            child.collect(doc, 1.0);
        }
        assert_eq!(child.harvest(), 10_000);
    }
}
