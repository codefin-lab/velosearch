//! The search path: query execution, hit assembly, sorting and aggregations.
//!
//! Aggregation requests are handed to VeloCore almost untouched -- its
//! aggregation JSON already matches OpenSearch's -- after rewriting each
//! `field` onto the JSON view that backs it.

use crate::api::{Params, apply_source_selector, err, err_caused_by, no_such_index};
use crate::query::{Ctx, View};
use crate::store::{IdxState, Store};
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::{Value, json};
use std::cmp::Ordering;
use velocore::aggregation::AggContextParams;
use velocore::aggregation::DistributedAggregationCollector;
use velocore::aggregation::agg_req::Aggregations;
use velocore::aggregation::intermediate_agg_result::IntermediateAggregationResults;
use velocore::collector::{Count, TopDocs};
use velocore::schema::Value as _;
use velocore::{DocAddress, Searcher, TantivyDocument};

pub(crate) mod request_cache;
pub(crate) use request_cache::RequestCache;
mod run;
pub use run::*;
pub(crate) use run::{Finish, finish_search};

mod budget;
pub(crate) use budget::*;
mod calendar;
pub(crate) use calendar::*;
mod candidates;
pub(crate) use candidates::*;
mod limits;
pub(crate) use limits::*;
mod shard;
pub(crate) use shard::*;

pub(crate) mod extras;
pub(crate) use extras::*;
mod geo;
pub(crate) use geo::*;
pub(crate) mod highlight;
pub(crate) use highlight::*;
mod explain;
pub(crate) use explain::*;
mod lookup;
pub(crate) use lookup::*;
mod nested;
pub(crate) use nested::*;
mod page;
pub(crate) use page::*;
mod profile;
pub(crate) use profile::*;
mod routing;
pub(crate) use routing::*;
mod sort;
pub(crate) use sort::*;

pub(crate) mod hybrid;
pub mod pipeline;
mod suggest;
pub(crate) use suggest::*;
mod phrase_suggest;
pub(crate) use phrase_suggest::*;
pub(crate) mod aggs;
pub(crate) use aggs::*;

const DEFAULT_TRACK_TOTAL_HITS: u64 = 10_000;

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum SortValue {
    I64(i64),
    U64(u64),
    F64(f64),
    Str(String),
    Missing,
}

impl SortValue {
    fn as_f64(&self) -> Option<f64> {
        match self {
            SortValue::I64(v) => Some(*v as f64),
            SortValue::U64(v) => Some(*v as f64),
            SortValue::F64(v) => Some(*v),
            _ => None,
        }
    }
}

impl SortValue {
    fn cmp_asc(&self, other: &SortValue) -> Ordering {
        match (self, other) {
            // a missing value always sorts last, whatever the direction
            (SortValue::Missing, SortValue::Missing) => Ordering::Equal,
            (SortValue::Missing, _) => Ordering::Greater,
            (_, SortValue::Missing) => Ordering::Less,
            (SortValue::I64(a), SortValue::I64(b)) => a.cmp(b),
            (SortValue::U64(a), SortValue::U64(b)) => a.cmp(b),
            (SortValue::Str(a), SortValue::Str(b)) => a.cmp(b),
            (SortValue::Str(_), _) => Ordering::Greater,
            (_, SortValue::Str(_)) => Ordering::Less,
            (a, b) => a.as_f64().partial_cmp(&b.as_f64()).unwrap_or(Ordering::Equal),
        }
    }

    fn to_json(&self) -> Value {
        match self {
            SortValue::I64(v) => json!(v),
            SortValue::U64(v) => json!(v),
            SortValue::F64(n) => {
                if n.fract() == 0.0 && n.abs() < 9e15 {
                    json!(*n as i64)
                } else {
                    json!(n)
                }
            }
            SortValue::Str(s) => json!(s),
            SortValue::Missing => Value::Null,
        }
    }
}

#[derive(Clone)]
pub(crate) struct SortKey {
    field: String,
    desc: bool,
    mode: Option<String>,
    /// where a document with no value for this field goes. `_last` means last
    /// in the order the caller sees, whichever direction that is.
    missing_last: bool,
    /// the nested object this key reads inside, where the caller named one
    nested: Option<String>,
    /// only the objects matching this take part in the sort
    nested_filter: Option<Value>,
    /// the width the values are read as, where the caller asked for one other
    /// than the field's own
    numeric_type: Option<String>,
    /// `unmapped_type`: what to treat the field as where an index does not
    /// map it. Without one, an index that does not map the field is an error
    /// rather than a document with no value.
    unmapped_type: Option<String>,
    /// a `_script` sort: the script that makes each document's value, and
    /// whether it is read as a number or as text
    script: Option<(Value, String)>,
}

/// One segment's readers for one sort field: the strings, the numbers, and
/// which of the two the column turned out to be.
type SegmentColumn = (
    Option<velocore::columnar::StrColumn>,
    Option<velocore::columnar::Column<u64>>,
    Option<velocore::columnar::ColumnType>,
);

/// Per-segment readers for one sort field, opened once and reused.
struct SortColumns {
    per_segment: Vec<SegmentColumn>,
}

impl SortColumns {
    /// Open the readers for a single segment.
    fn for_segment(reader: &velocore::SegmentReader, column: &str) -> SortColumns {
        let ff = reader.fast_fields();
        let str_col = ff.str(column).ok().flatten();
        let (num_col, ty) = match ff.u64_lenient(column) {
            Ok(Some((c, t))) => (Some(c), Some(t)),
            _ => (None, None),
        };
        SortColumns { per_segment: vec![(str_col, num_col, ty)] }
    }

    /// Every numeric value a document holds for this column.
    fn numeric_values(&self, doc: velocore::DocId) -> Vec<f64> {
        let Some((_, num, ty)) = self.per_segment.first() else { return Vec::new() };
        let (Some(col), Some(ty)) = (num, ty) else { return Vec::new() };
        col.values_for_doc(doc)
            .filter_map(|raw| decode_col_value(raw, *ty).and_then(|v| v.as_f64()))
            .collect()
    }

    /// Read the value for a document inside the segment this was opened for.
    fn read(&self, doc: velocore::DocId, desc: bool, mode: Option<&str>) -> SortValue {
        self.value(DocAddress::new(0, doc), desc, mode)
    }

    fn value(&self, addr: DocAddress, desc: bool, mode: Option<&str>) -> SortValue {
        let mode = mode.unwrap_or(if desc { "max" } else { "min" });
        let Some((str_col, num_col, ty)) = self.per_segment.get(addr.segment_ord as usize) else {
            return SortValue::Missing;
        };
        if let (Some(num), Some(ty)) = (num_col, ty) {
            let mut vals: Vec<SortValue> = Vec::new();
            for raw in num.values_for_doc(addr.doc_id) {
                use velocore::columnar::ColumnType;
                let decoded = match ty {
                    ColumnType::I64 | ColumnType::DateTime => Some(SortValue::I64(
                        velocore::columnar::MonotonicallyMappableToU64::from_u64(raw),
                    )),
                    ColumnType::F64 => Some(SortValue::F64(
                        <f64 as velocore::columnar::MonotonicallyMappableToU64>::from_u64(raw),
                    )),
                    ColumnType::U64 => Some(SortValue::U64(raw)),
                    ColumnType::Bool => Some(SortValue::I64(raw as i64)),
                    ColumnType::Str | ColumnType::Bytes | ColumnType::IpAddr => None,
                };
                if let Some(d) = decoded {
                    vals.push(d);
                }
            }
            if !vals.is_empty() {
                return reduce_sort_values(&mut vals, mode);
            }
        }
        if let Some(sc) = str_col
            && let Some(ord) = sc.term_ords(addr.doc_id).next()
        {
            let mut buf = Vec::new();
            if sc.ord_to_bytes(ord, &mut buf).unwrap_or(false)
                && let Ok(s) = String::from_utf8(buf)
            {
                return SortValue::Str(s);
            }
        }
        SortValue::Missing
    }
}

pub(crate) struct Hit {
    shard_idx: usize,
    index: String,
    id: String,
    score: f32,
    source: Value,
    sort: Vec<SortValue>,
    version: u64,
    ignored: Option<Value>,
    seq: u64,
}

/// What one sort key reads out of a segment.
#[derive(Clone)]
enum SortSource {
    Score,
    Doc,
    Column {
        name: String,
        desc: bool,
        mode: Option<String>,
    },
    /// `_shard_doc`: the write order inside an index, offset by where the
    /// point in time puts that index, so the value names one document across
    /// every index the point in time covers
    ShardDoc {
        base: u64,
    },
}

/// Top-K collector that evaluates the sort keys while collecting, so a query
/// matching millions of documents still only ever holds K candidates.
struct SortCollector {
    sources: Vec<SortSource>,
    desc: Vec<bool>,
    /// per key, whether a document with no value goes last
    missing_last: Vec<bool>,
    limit: usize,
    /// Where a previous page ended. Documents up to and including it are not
    /// collected at all, so the page limit applies to what comes after.
    after: Option<Vec<SortValue>>,
}

struct SortSegmentCollector {
    segment_ord: u32,
    sources: Vec<SortSource>,
    columns: Vec<Option<SortColumns>>,
    desc: Vec<bool>,
    missing_last: Vec<bool>,
    limit: usize,
    after: Option<Vec<SortValue>>,
    buf: Vec<Cand>,
    /// The worst candidate currently kept. A document whose leading sort value
    /// cannot beat it is dropped before anything is allocated for it.
    cutoff: Option<SortValue>,
    /// Set when the whole sort is one numeric column, which lets a block of
    /// documents be read from the columnar in one call instead of one at a time.
    block: Option<velocore::columnar::ColumnBlockAccessor<u64>>,
}

impl velocore::collector::Collector for SortCollector {
    type Fruit = Vec<Cand>;
    type Child = SortSegmentCollector;

    fn for_segment(
        &self,
        segment_ord: u32,
        reader: &velocore::SegmentReader,
    ) -> velocore::Result<Self::Child> {
        let columns: Vec<Option<SortColumns>> = self
            .sources
            .iter()
            .map(|src| match src {
                SortSource::Column { name, .. } => Some(SortColumns::for_segment(reader, name)),
                SortSource::ShardDoc { .. } => Some(SortColumns::for_segment(reader, "_seq")),
                _ => None,
            })
            .collect();
        // VELOSEARCH_NO_BLOCK_SORT=1 disables the vectorised path, for A/B runs
        let single_numeric = std::env::var("VELOSEARCH_NO_BLOCK_SORT").is_err()
            && self.sources.len() == 1
            && matches!(self.sources[0], SortSource::Column { ref mode, .. } if mode.is_none())
            && columns
                .first()
                .and_then(|c| c.as_ref())
                .map(|c| {
                    let (str_col, num, _) = &c.per_segment[0];
                    // a multi-valued column would yield several rows per doc,
                    // which the block path has no way to reduce
                    num.as_ref().map(|n| n.index.get_cardinality().is_full()).unwrap_or(false)
                        && str_col.is_none()
                })
                .unwrap_or(false);
        Ok(SortSegmentCollector {
            segment_ord,
            sources: self.sources.clone(),
            columns,
            desc: self.desc.clone(),
            missing_last: self.missing_last.clone(),
            limit: self.limit,
            after: self.after.clone(),
            buf: Vec::with_capacity(self.limit.saturating_mul(4).clamp(512, 4096)),
            cutoff: None,
            block: if single_numeric { Some(Default::default()) } else { None },
        })
    }

    fn requires_scoring(&self) -> bool {
        self.sources.iter().any(|s| matches!(s, SortSource::Score))
    }

    fn merge_fruits(&self, children: Vec<Vec<Cand>>) -> velocore::Result<Self::Fruit> {
        let mut out: Vec<Cand> = children.into_iter().flatten().collect();
        prune_by(&mut out, self.limit, &self.desc);
        Ok(out)
    }
}

impl SortSegmentCollector {
    fn read_key(&self, i: usize, doc: velocore::DocId, score: velocore::Score) -> SortValue {
        match &self.sources[i] {
            SortSource::Score => SortValue::F64(score as f64),
            // `_doc` is the document's place in the shard, not its place in
            // one segment of it: two documents in different segments both
            // read 0, so they sorted equal and `search_after` on `_doc` --
            // the documented cheap way to walk everything -- could not
            // advance past them. The segment's ordinal is the high half.
            SortSource::Doc => SortValue::I64(((self.segment_ord as i64) << 32) | (doc as i64)),
            SortSource::Column { desc, mode, .. } => self.columns[i]
                .as_ref()
                .map(|c| c.read(doc, *desc, mode.as_deref()))
                .unwrap_or(SortValue::Missing),
            SortSource::ShardDoc { base } => {
                match self.columns[i].as_ref().map(|c| c.read(doc, false, None)) {
                    Some(SortValue::U64(seq)) => SortValue::U64(base + seq),
                    Some(SortValue::I64(seq)) => SortValue::I64(*base as i64 + seq),
                    _ => SortValue::Missing,
                }
            }
        }
    }

    fn prune(&mut self) {
        prune_by(&mut self.buf, self.limit, &self.desc);
        // once the buffer is full the worst kept entry becomes the bar to clear
        if self.buf.len() >= self.limit {
            let worst = self
                .buf
                .iter()
                .max_by(|a, b| cmp_sorted(&a.sort, &b.sort, &self.desc))
                .map(|c| c.sort[0].clone());
            self.cutoff = worst;
        }
    }
}

impl velocore::collector::SegmentCollector for SortSegmentCollector {
    type Fruit = Vec<Cand>;

    /// Vectorised path: for a single numeric sort key the whole block of
    /// matching documents is pulled out of the columnar in one call.
    fn collect_block(&mut self, docs: &[velocore::DocId]) {
        if self.block.is_none() {
            for &d in docs {
                self.collect(d, 0.0);
            }
            return;
        }
        // the block path is only taken where there is one numeric column and a
        // buffer to read it into; anything else collects a document at a time
        let Some((col, ty)) = self.columns[0].as_ref().and_then(|sc| {
            let (_, num, ty) = &sc.per_segment[0];
            Some((num.clone()?, (*ty)?))
        }) else {
            for &d in docs {
                self.collect(d, 0.0);
            }
            return;
        };
        let Some(mut block) = self.block.take() else {
            for &d in docs {
                self.collect(d, 0.0);
            }
            return;
        };
        block.fetch_block(docs, &col);
        let desc = self.desc[0];
        let after = self.after.as_ref().and_then(|a| a.first().cloned());
        for (doc, raw) in block.iter_docid_vals(docs, &col) {
            let Some(v) = decode_col_value(raw, ty) else { continue };
            // the vectorized path has to honour the page boundary too
            if let Some(marker) = &after {
                let ord = v.cmp_asc(marker);
                let ord = if desc { ord.reverse() } else { ord };
                if ord != Ordering::Greater {
                    continue;
                }
            }
            if let Some(cut) = &self.cutoff {
                let ord = v.cmp_asc(cut);
                let ord = if desc { ord.reverse() } else { ord };
                if ord == Ordering::Greater {
                    continue;
                }
            }
            self.buf.push(Cand {
                shard: 0,
                addr: DocAddress::new(self.segment_ord, doc),
                score: 1.0,
                sort: vec![v],
                seq: u64::MAX,
            });
            if self.limit > 0 && self.buf.len() >= self.limit.saturating_mul(4).max(512) {
                self.prune();
            }
        }
        self.block = Some(block);
    }

    fn collect(&mut self, doc: velocore::DocId, score: velocore::Score) {
        let first = self.read_key(0, doc, score);
        // cheap rejection before allocating anything for this document
        if let Some(cut) = &self.cutoff {
            let ord = first.cmp_asc(cut);
            let ord = if self.desc[0] { ord.reverse() } else { ord };
            if ord == Ordering::Greater {
                return;
            }
        }
        let mut sort = Vec::with_capacity(self.sources.len());
        sort.push(first);
        for i in 1..self.sources.len() {
            sort.push(self.read_key(i, doc, score));
        }
        // anything at or before where the last page ended is already served
        if let Some(after) = &self.after {
            let mut behind = true;
            for (i, want_desc) in self.desc.iter().enumerate() {
                let Some(marker) = after.get(i) else { break };
                let key = SortKey {
                    field: String::new(),
                    desc: *want_desc,
                    mode: None,
                    missing_last: self.missing_last.get(i).copied().unwrap_or(true),
                    nested: None,
                    nested_filter: None,
                    numeric_type: None,
                    unmapped_type: None,
                    script: None,
                };
                let ord = cmp_with_missing(&sort[i], marker, &key);
                match ord {
                    Ordering::Greater => {
                        behind = false;
                        break;
                    }
                    Ordering::Less => {
                        behind = true;
                        break;
                    }
                    Ordering::Equal => {}
                }
            }
            if behind {
                return;
            }
        }
        self.buf.push(Cand {
            shard: 0,
            addr: DocAddress::new(self.segment_ord, doc),
            score,
            sort,
            seq: u64::MAX,
        });
        if self.limit > 0 && self.buf.len() >= self.limit.saturating_mul(4).max(512) {
            self.prune();
        }
    }

    fn harvest(mut self) -> Self::Fruit {
        prune_by(&mut self.buf, self.limit, &self.desc);
        self.buf
    }
}

/// A matched document before its `_source` is read. Reading stored fields is by
/// far the most expensive part of a hit, so it is deferred until the page is known.
pub(crate) struct Cand {
    shard: usize,
    addr: DocAddress,
    score: f32,
    sort: Vec<SortValue>,
    /// the order this document's write arrived in; filled once the candidates
    /// from every segment are together, and only used to settle ties
    seq: u64,
}

/// Which of the clauses that need work after the search are in this query.
///
/// Each of them used to be looked for on its own, which is several walks of
/// the query for a search that has none of them; this is one walk.
#[derive(Default)]
pub(crate) struct Extras {
    geo: bool,
    distance_feature: bool,
    nested_inner_hits: bool,
    /// a `nested` clause is in the query somewhere; whether any of them can
    /// actually be settled is decided by `settleable_nested`
    nested_query: bool,
    named: bool,
}

/// Aggregations address fields by name; rewrite each onto the JSON view that
/// actually carries the doc values for it.
const NUMERIC_AGGS: &[&str] = &[
    "avg",
    "sum",
    "min",
    "max",
    "stats",
    "extended_stats",
    "percentiles",
    "histogram",
    "date_histogram",
];

/// Weight aggregation buckets by `_doc_count`.
///
/// A document may stand for several, which every bucket count has to reflect.
/// Rather than a second collection pass, each bucket agg gains two helpers --
/// the sum of the field and how many documents carry it -- and the correction
/// is `doc_count + sum - carried`: documents without the field still count
/// once, documents with it count what it says.
const DC_SUM: &str = "__bs_dc_sum";
const DC_CNT: &str = "__bs_dc_count";

/// What a node hands a coordinator besides the page: the aggregations
/// still intermediate (postcard bytes), and how they were asked.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct NativeParts {
    pub agg_acc: Option<Vec<u8>>,
    pub agg_req: Option<Value>,
    pub agg_meta: Vec<(String, Value)>,
    pub bucket_orders: Vec<(String, String, bool)>,
    pub weighted: bool,
    pub empty_shards: u64,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Outcome {
    pub took_ms: u64,
    pub skipped: u64,
    pub shards: u64,
    pub total: u64,
    pub hits: Vec<Value>,
    pub max_score: Option<f32>,
    pub aggs: Option<Value>,
    pub profile: Option<Value>,
    pub suggest: Option<Value>,
    /// shards that could not answer, and why
    pub failures: Vec<Value>,
    /// a document-level security filter was laid over the query, which
    /// makes it a real query: nothing ends early under it
    pub filtered: bool,
    /// the search stopped at its deadline, and what is here is what it had
    /// collected by then -- the reference's `timed_out`, measured
    #[serde(default)]
    pub timed_out: bool,
    /// filled only for a coordinator, by a node answering it
    #[serde(default)]
    pub native: Option<NativeParts>,
}

/// The parts of a query that only a document's own values can settle.
///
/// A geo shape and `distance_feature` ask something the index cannot answer
/// on its own: whether a point is inside a shape, how far a value is from an
/// origin. The query put
/// to VeloCore matches more widely than that, and the candidates it found are
/// read back here and judged properly.
type Searchers = [(String, Searcher, std::sync::Arc<crate::store::IdxLock>)];

/// The aggregations a request asks for, sorted into who answers them.
///
/// VeloCore answers what it can parse. What it cannot -- `filters`, a
/// pipeline, an aggregation over a field no index has -- is peeled off here and
/// computed a bucket at a time through the ordinary query path, and a pipeline
/// that reads finished buckets is held back until there are buckets to read.
pub(crate) struct AggPlan {
    /// what is left for VeloCore to parse
    request: Option<Value>,
    /// this engine's own, one filtered search per named bucket
    peeled: Vec<(String, Value)>,
    /// pipelines over the whole answer, applied last
    siblings: Vec<(String, Value)>,
    /// pipelines that sit inside a bucketing aggregation, by the path to it
    inner: Vec<(Vec<String>, String, Value)>,
    /// whether a document stands for several, so buckets have to be weighted
    weighted: bool,
}

/// What a hit should carry back besides its `_source`.
///
/// `fields` and `docvalue_fields` name the same values -- both are read out of
/// the stored source, which holds everything either could report -- so they are
/// gathered into one list. `stored_fields` is its own thing.
pub(crate) struct OutputSpecs {
    source: Option<Value>,
    fields: Option<Vec<(String, Option<String>)>>,
    stored: Option<Vec<String>>,
}

/// The sibling pipelines: aggregations whose input is other aggregations'
/// buckets rather than documents.
/// Pipelines that live inside a bucketing aggregation and add a value to each
/// of its buckets, rather than beside it summarising them all.
const BUCKET_PIPELINES: &[&str] = &[
    "cumulative_sum",
    "derivative",
    "moving_avg",
    "moving_fn",
    "serial_diff",
    "bucket_sort",
    "bucket_selector",
    "bucket_script",
];

const PIPELINES: &[&str] = &[
    "avg_bucket",
    "sum_bucket",
    "min_bucket",
    "max_bucket",
    "stats_bucket",
    "extended_stats_bucket",
    "percentiles_bucket",
];

/// One source of a composite aggregation: what to bucket by, and how the key
/// it produces should be read back.
pub(crate) struct CompSource {
    name: String,
    node: Value,
    date: bool,
    format: Option<String>,
    desc: bool,
    /// the earliest value the field holds, in nanoseconds, which is what says
    /// which unit the histogram answered in
    least_ns: Option<f64>,
    /// how far the zone this source is reported in sits from UTC
    zone_ms: i64,
    /// an address is held in a fixed-width form and read back out of it
    ip: bool,
    /// documents with no value for this source still make a bucket of their own
    missing_bucket: bool,
    /// where that bucket goes: `first` or `last`
    missing_last: bool,
    /// the field the source reads, for finding the documents that lack it
    field: String,
}
