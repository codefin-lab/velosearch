//! One index's share of a search: the query put to VeloCore, the
//! candidates it gives back, and what it collected on the way.

use super::*;

/// An aggregation collector that may not be there.
///
/// Hits and aggregations were two separate searches over the same query, which
/// meant building the weight and walking every segment twice per index. At a
/// couple of hundred indices that second pass is most of the cost, so the two
/// now ride in one collector tuple -- and a request without aggregations still
/// needs something to occupy that slot.
pub(crate) struct MaybeAgg(Option<DistributedAggregationCollector>);

pub(crate) struct MaybeAggSegment(Option<velocore::aggregation::AggregationSegmentCollector>);

/// Run a shard's search, choosing where the per-segment work goes.
///
/// VeloCore's own `search` hands the segments to the index's shared executor.
/// When a query fans out over many indices the outer parallelism already keeps
/// every core busy, and asking that same pool for per-segment parallelism from
/// inside it means each shard queues behind the others: measured on two hundred
/// empty indices, a search that should be free took 147us of elapsed time
/// waiting. One index at a time still wants the pool -- that is where
/// per-segment parallelism pays.
pub(crate) fn search_shard<C: velocore::collector::Collector>(
    searcher: &Searcher,
    query: &dyn velocore::query::Query,
    collector: &C,
    fanned_out: bool,
    clock: &std::sync::Arc<Clock>,
) -> velocore::Result<C::Fruit> {
    // every walk is held to the search's clock: what it collected before the
    // deadline is the answer, and the documents after it are not visited
    let collector = &Timed::new(collector, clock);
    if !fanned_out {
        return searcher.search(query, collector);
    }
    let scoring = if velocore::collector::Collector::requires_scoring(collector) {
        velocore::query::EnableScoring::enabled_from_statistics_provider(searcher, searcher)
    } else {
        velocore::query::EnableScoring::disabled_from_searcher(searcher)
    };
    searcher.search_with_executor(query, collector, &velocore::Executor::single_thread(), scoring)
}

// One shard's work touches only its own index, so the fan-out runs across
// cores. Searching many small indices is otherwise bounded by walking them
// one at a time.
pub(crate) struct ShardOut {
    pub(crate) name: String,
    pub(crate) searcher: Searcher,
    pub(crate) st: std::sync::Arc<crate::store::IdxLock>,
    pub(crate) shards: u64,
    pub(crate) count: usize,
    pub(crate) cands: Vec<Cand>,
    pub(crate) agg: Option<IntermediateAggregationResults>,
    pub(crate) agg_req: Option<Aggregations>,
    pub(crate) agg_meta: Vec<(String, Value)>,
    pub(crate) bucket_orders: Vec<(String, String, bool)>,
    pub(crate) profile: Option<Value>,
}

/// How many shards a filter keeps a search to, where it keeps it to some.
pub(crate) fn narrowed_shard_count(filter: &Value) -> Option<u64> {
    match filter {
        Value::Object(o) => match o.get("_vs_on_shards") {
            Some(on) => on.get("shards").and_then(|v| v.as_array()).map(|a| a.len() as u64),
            None => o.values().find_map(narrowed_shard_count),
        },
        Value::Array(a) => a.iter().find_map(narrowed_shard_count),
        _ => None,
    }
}

/// The groups a search names in `stats`, which `_stats?groups=` reports on.
pub(crate) fn stats_groups(body: &Value) -> Vec<String> {
    match body.get("stats") {
        Some(Value::Array(a)) => a.iter().filter_map(|g| g.as_str().map(String::from)).collect(),
        Some(Value::String(s)) => s.split(',').map(|g| g.trim().to_string()).collect(),
        _ => Vec::new(),
    }
}

/// Search one index, as one shard of the whole request: its query phase,
/// counted into the index's search statistics and the groups the search
/// named, and written to the search slow log when it took long enough.
#[allow(clippy::too_many_arguments)]
pub(crate) fn search_one_shard(
    store: &Store,
    shard_idx: usize,
    name: &str,
    body: &Value,
    query_json: &Option<Value>,
    sort_keys: &[SortKey],
    search_after: &Option<Vec<SortValue>>,
    pit_parts: &std::collections::HashMap<String, crate::store::PitPart>,
    agg_json: &Option<Value>,
    filters_aggs: &[(String, Value)],
    page_want: usize,
    fanned_out: bool,
    views: &crate::security::view::Views,
    budget: &Budget,
) -> std::result::Result<Option<ShardOut>, Response> {
    let Some(st) = store.get(name) else { return Ok(None) };
    let started = std::time::Instant::now();
    let groups = stats_groups(body);
    let out = {
        let g = st.read();
        g.counters.search.query.current_add(1);
        for group in &groups {
            g.counters.group(group).query.current_add(1);
        }
        drop(g);
        query_shard(
            store,
            shard_idx,
            name,
            body,
            query_json,
            sort_keys,
            search_after,
            pit_parts,
            agg_json,
            filters_aggs,
            page_want,
            fanned_out,
            views,
            budget,
        )
    };
    let took = started.elapsed().as_nanos() as u64;
    let g = st.read();
    let c = &g.counters;
    c.search.query.current_add(-1);
    for group in &groups {
        c.group(group).query.current_add(-1);
    }
    match &out {
        Ok(Some(o)) => {
            c.search.query.add(took);
            for group in &groups {
                c.group(group).query.add(took);
            }
            if !g.knobs.slowlog.query.is_off() {
                crate::store::slowlog::search(
                    &g.knobs.slowlog.query,
                    "query",
                    name,
                    took,
                    o.count as u64,
                    &groups,
                    g.shard_count(),
                    body,
                );
            }
        }
        Ok(None) => {}
        Err(_) => {
            c.search.query_failed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            for group in &groups {
                c.group(group).query_failed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn query_shard(
    store: &Store,
    shard_idx: usize,
    name: &str,
    body: &Value,
    query_json: &Option<Value>,
    sort_keys: &[SortKey],
    search_after: &Option<Vec<SortValue>>,
    pit_parts: &std::collections::HashMap<String, crate::store::PitPart>,
    agg_json: &Option<Value>,
    filters_aggs: &[(String, Value)],
    page_want: usize,
    fanned_out: bool,
    views: &crate::security::view::Views,
    budget: &Budget,
) -> std::result::Result<Option<ShardOut>, Response> {
    let Some(st) = store.get(name) else { return Ok(None) };
    // the caller's view of this index: what the query may ask, what the
    // aggregations may read, what a sort may order by
    let view = views.get(name).cloned();
    let kinds: std::collections::HashMap<String, String> = match view.as_ref() {
        Some(_) => st.read().all_field_types().into_iter().collect(),
        None => std::collections::HashMap::new(),
    };
    let rewritten_query: Option<Value> =
        view.as_ref().and_then(|v| query_json.as_ref().map(|q| v.rewrite_query(q, &kinds)));
    let query_json: &Option<Value> =
        if rewritten_query.is_some() { &rewritten_query } else { query_json };
    let rewritten_aggs: Option<Value> =
        view.as_ref().and_then(|v| agg_json.as_ref().map(|a| v.rewrite_aggs(a, &kinds)));
    let agg_json: &Option<Value> =
        if rewritten_aggs.is_some() { &rewritten_aggs } else { agg_json };
    let narrowed_keys: Vec<SortKey>;
    let sort_keys: &[SortKey] = match view.as_ref() {
        Some(v) if sort_keys.iter().any(|k| v.hidden(&k.field)) => {
            narrowed_keys = sort_keys
                .iter()
                .map(|k| {
                    if v.hidden(&k.field) {
                        SortKey { field: crate::security::view::HIDDEN.into(), ..k.clone() }
                    } else {
                        k.clone()
                    }
                })
                .collect();
            &narrowed_keys
        }
        _ => sort_keys,
    };
    let g = st.read();
    let mut shards = 0u64;
    let mut cands: Vec<Cand> = Vec::new();
    let mut agg_acc: Option<IntermediateAggregationResults> = None;
    let mut agg_req: Option<Aggregations> = None;
    let mut agg_meta: Vec<(String, Value)> = Vec::new();
    let mut bucket_orders: Vec<(String, String, bool)> = Vec::new();
    // a search narrowed to some of the shards reports only those
    shards += crate::security::layer::alias_filter_for(name)
        .and_then(|f| narrowed_shard_count(&f))
        .unwrap_or_else(|| g.shard_count());
    let ctx = Ctx {
        fields: &g.fields,
        mapping: &g.mapping,
        analysis: &g.analysis,
        index: &g.index,
        max_terms_count: g.max_terms_count(),
        max_regex_length: g.max_regex_length(),
        allow_expensive: crate::search::expensive_allowed(store),
        observed_kinds: &g.observed_kinds,
        kinds_complete: g.kinds_complete,
        stats: &g.stats,
        vectors: &g.vectors,
    };
    // `_index` is a field every document of this index has, holding this
    // index's name: a query on it is answered here, where the name is known,
    // rather than by an index that does not store it -- where every such
    // query matched nothing, whatever it asked
    // an alias of this index is a name its documents answer to as well
    let also_called: Vec<String> = g.aliases.keys().cloned().collect();
    let by_name = query_json.clone().map(|mut q| {
        answer_index_name(&mut q, name, &also_called);
        q
    });
    let query_json = &by_name;
    // the filter the alias this request named puts on this index, which is
    // as much a part of the question as the query the caller wrote
    let with_alias = crate::security::with_alias_filter(name, query_json.clone());
    let query_json = &with_alias;
    let q: Box<dyn velocore::query::Query> = match &query_json {
        Some(qj) => match crate::query::build(&ctx, qj) {
            Ok(q) => q,
            Err(e) => {
                let why = e.to_string();
                // a query that reads well but cannot be run over the field it
                // names fails on the shard rather than in the parser
                if why.starts_with("Cannot create intervals")
                    || why.starts_with("failed to create query:")
                {
                    return Err(err_caused_by(
                        "search_phase_execution_exception",
                        "all shards failed",
                        &why,
                    ));
                }
                return Err(err(StatusCode::BAD_REQUEST, "parsing_exception", why));
            }
        },
        None => Box::new(velocore::query::AllQuery),
    };
    // the caller's document-level security narrows every query on this
    // index; a filter clause, so scores are untouched
    let q: Box<dyn velocore::query::Query> = match view.as_ref().and_then(|v| v.dls.clone()) {
        Some(dls) => match crate::query::build(&ctx, &dls) {
            Ok(filter) => Box::new(velocore::query::BooleanQuery::new(vec![
                (velocore::query::Occur::Must, q),
                (velocore::query::Occur::Must, filter),
            ])),
            Err(e) => {
                return Err(err(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "security_exception",
                    format!("Unable to parse DLS query: {e}"),
                ));
            }
        },
        None => q,
    };
    // a point in time holds the search to the reader it was opened over, which
    // still has every document as it was then -- an update or a delete since
    // took the old version out of the index, not out of that reader
    let pit_part = pit_parts.get(name);
    let searcher = match pit_part {
        Some(part) => part.searcher.clone(),
        None => g.reader.searcher(),
    };

    // the peeled aggregations never reach the parser, so their fields are
    // checked here rather than alongside the ones that do
    if !filters_aggs.is_empty() {
        let peeled: Value = filters_aggs
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect::<serde_json::Map<_, _>>()
            .into();
        check_agg_types(&peeled, &ctx).map_err(|e| phase_failure_of(e, name))?;
    }

    // aggregations, when asked for, run over the same query
    let mut this_agg: Option<Aggregations> = None;
    let mut agg_request_json: Option<Value> = None;
    if let Some(aj) = &agg_json {
        let mut rewritten = aj.clone();
        normalize_aggs(&mut rewritten, &mut agg_meta, true);
        check_agg_types(&rewritten, &ctx).map_err(|e| phase_failure_of(e, name))?;
        normalize_agg_dates(&mut rewritten);
        bucket_orders = extract_bucket_orders(&mut rewritten);
        let _ = extract_partitions(&mut rewritten);
        lower_nested_filters(&mut rewritten, &ctx);
        strip_untranslatable_term_filters(&mut rewritten, &ctx);
        // before the fields are renamed to the columns they live in, so
        // the mapping still answers for the name the request used
        fixed_date_histograms(&mut rewritten, &ctx);
        rewrite_agg_fields(&mut rewritten, &ctx);
        agg_request_json = Some(rewritten.clone());
        match serde_json::from_value::<Aggregations>(rewritten) {
            Ok(a) => this_agg = Some(a),
            Err(e) => {
                return Err(err(
                    StatusCode::BAD_REQUEST,
                    "x_content_parse_exception",
                    format!("failed to parse aggregation: {e}"),
                ));
            }
        }
    }

    // Documents that score the same come back in the order they were written,
    // and the writer spreads one request across its threads: a shard-level
    // prune that kept only `size` of them could drop the earlier document and
    // keep the later one. Asking for a few more lets the merge, which knows
    // the order they arrived in, choose between them.
    let want = match sort_keys.is_empty() {
        true => page_want.saturating_add(128),
        false => page_want,
    };
    // The aggregation rides along with the hit collection so the query is
    // walked once per index rather than twice. Profiling drives the phases
    // itself and keeps its own pass.
    let profiling = body.get("profile").map(|v| v == true).unwrap_or(false);
    let agg_collector = MaybeAgg(match (&this_agg, profiling) {
        (Some(a), false) => {
            let ctxp = AggContextParams::new(budget.aggs(), g.index.tokenizers().clone());
            Some(DistributedAggregationCollector::from_aggs(a.clone(), ctxp))
        }
        _ => None,
    });

    // what the collection took, which a profile reports as its collectors'
    let collecting = std::time::Instant::now();
    let searched = if want == 0 {
        // `size: 0` asks for counts and aggregations only. Collecting a
        // page anyway means scoring and heap-ordering every match for a
        // result that is thrown away.
        search_shard(&searcher, &q, &(Count, agg_collector), fanned_out, &budget.clock)
            .map(|(c, agg)| (c, Vec::new(), agg))
    } else if sort_keys.is_empty() && agg_collector.0.is_none() && count_without_walking(query_json)
    {
        // Nothing else needs every document, so the top-k collector can
        // prune: once its heap is full, whole blocks that cannot beat the
        // worst kept score are skipped. Bundling a counter alongside it
        // would force every document to be visited and give that up --
        // measured at three to four times the throughput on this shape.
        //
        // The count then comes from the weight, which answers it from the
        // postings header for the queries that can, and otherwise walks
        // the same documents the tuple would have.
        // This query is cheap: the heap prunes and only `want` documents
        // are kept. Splitting its segments across the pool costs more in
        // coordination than the walk itself, and steals cores from the
        // aggregations, which are the expensive shape and do need them.
        let topk = search_shard(
            &searcher,
            &q,
            &TopDocs::with_limit(want.max(1)).order_by_score(),
            true,
            &budget.clock,
        );
        topk.and_then(|docs| {
            let cands = docs
                .into_iter()
                .map(|(score, addr)| Cand {
                    shard: shard_idx,
                    addr,
                    score,
                    sort: Vec::new(),
                    seq: u64::MAX,
                })
                .collect::<Vec<_>>();
            let count = count_matches(&searcher, &q)?;
            Ok((count, cands, None))
        })
    } else if sort_keys.is_empty() {
        // an aggregation needs every document anyway, so there is nothing
        // to prune and hits ride along in the same pass
        let collector = (Count, TopDocs::with_limit(want.max(1)).order_by_score(), agg_collector);
        search_shard(&searcher, &q, &collector, fanned_out, &budget.clock).map(|(c, docs, agg)| {
            let cands = docs
                .into_iter()
                .map(|(score, addr)| Cand {
                    shard: shard_idx,
                    addr,
                    score,
                    sort: Vec::new(),
                    seq: u64::MAX,
                })
                .collect::<Vec<_>>();
            (c, cands, agg)
        })
    } else {
        // sort keys are evaluated during collection, so only `want`
        // candidates are ever held rather than one per match
        let sources: Vec<SortSource> = sort_keys
            .iter()
            .map(|k| match k.field.as_str() {
                "_score" => SortSource::Score,
                "_doc" => SortSource::Doc,
                // `_seq` is a column of the index itself, not a field
                // inside either JSON view, so it is named as it is
                "_seq" => SortSource::Column {
                    name: "_seq".to_string(),
                    desc: k.desc,
                    mode: k.mode.clone(),
                },
                // `_id` is a column of its own as well: named through the
                // JSON views it read a column no document has, every hit's
                // sort value was null, and `search_after` handed back the
                // same page for ever -- a client paging a whole index by id
                // saw the first thousand documents again and again
                "_shard_doc" => {
                    SortSource::ShardDoc { base: pit_part.map(|p| p.shard_doc_base).unwrap_or(0) }
                }
                "_id" => SortSource::Column {
                    name: "_id".to_string(),
                    desc: k.desc,
                    mode: k.mode.clone(),
                },
                // The values of a field inside a nested object belong to
                // the object, not to the document, so a sort that does not
                // say which object it reads inside finds nothing -- which
                // is what OpenSearch's resolveNested returning null means.
                // a script's value is worked out once the candidates are
                // known; while collecting there is nothing to read
                _ if k.script.is_some() => SortSource::Column {
                    name: "_vs_no_such_column".to_string(),
                    desc: k.desc,
                    mode: k.mode.clone(),
                },
                _ if k.nested.is_none() && under_nested(ctx.mapping, &k.field) => {
                    SortSource::Column {
                        name: "_vs_no_such_column".to_string(),
                        desc: k.desc,
                        mode: k.mode.clone(),
                    }
                }
                // a date is a number in the index -- milliseconds, or
                // nanoseconds for a date_nanos -- which is the number
                // OpenSearch reports, so nothing is rescaled
                _ => SortSource::Column {
                    name: ctx.column_name(&k.field, false),
                    desc: k.desc,
                    mode: k.mode.clone(),
                },
            })
            .collect();
        let desc: Vec<bool> = sort_keys.iter().map(|k| k.desc).collect();
        let collector = (
            Count,
            SortCollector {
                sources,
                missing_last: sort_keys.iter().map(|k| k.missing_last).collect(),
                desc,
                limit: if sort_keys.iter().any(|k| k.script.is_some()) {
                    // every match has to reach the script, which sorts them
                    10_000.max(want)
                } else {
                    want.max(1)
                },
                after: search_after.clone(),
            },
            agg_collector,
        );
        search_shard(&searcher, &q, &collector, fanned_out, &budget.clock).map(
            |(c, mut cands, agg)| {
                for cand in cands.iter_mut() {
                    cand.shard = shard_idx;
                }
                (c, cands, agg)
            },
        )
    };
    let collected_nanos = collecting.elapsed().as_nanos() as u64;
    let (count, shard_cands, shard_agg) = match searched {
        Ok(v) => v,
        Err(e) => return Err(search_error_response(&e.to_string(), name)),
    };
    if let Some(res) = shard_agg {
        agg_acc = Some(res);
        agg_req = this_agg.clone();
    }
    cands.extend(shard_cands);

    let mut shard_profile = None;
    // a profile is asked for by the request, not by the aggregations: a
    // search with no aggregations still has a shard to report on
    if profiling {
        let (mut entries, mut agg_nanos) = (Vec::new(), 0u64);
        if let Some(a) = this_agg {
            let (res, profiled, nanos) = profiled_agg_search(
                &searcher,
                q.as_ref(),
                a.clone(),
                &ctx,
                agg_request_json.as_ref(),
            );
            match res {
                Ok(res) => {
                    agg_acc = Some(res);
                    agg_req = Some(a);
                }
                Err(e) => {
                    return Err(err(
                        StatusCode::BAD_REQUEST,
                        "aggregation_execution_exception",
                        e.to_string(),
                    ));
                }
            }
            (entries, agg_nanos) = (profiled, nanos);
        }
        let agg_names: Vec<String> = entries
            .iter()
            .filter_map(|e| e.get("description").and_then(|d| d.as_str()).map(String::from))
            .collect();
        let searches = search_profile(
            &searcher,
            &ctx,
            query_json,
            (sort_keys.is_empty() && page_want > 0, !sort_keys.is_empty()),
            collected_nanos,
            &agg_names,
            agg_nanos,
            page_want,
        );
        // the index's profile, shared out between its shards once the whole
        // search is done -- see `split_by_shard`
        shard_profile = Some(json!({
            "_index": g.name,
            "_shares": matched_by_shard(&searcher, &g, q.as_ref()),
            "searches": [searches],
            "aggregations": entries,
        }));
    }

    Ok(Some(ShardOut {
        name: g.name.clone(),
        searcher,
        st: st.clone(),
        shards,
        count,
        cands,
        agg: agg_acc,
        agg_req,
        agg_meta,
        bucket_orders,
        profile: shard_profile,
    }))
}

impl velocore::collector::Collector for MaybeAgg {
    type Fruit = Option<IntermediateAggregationResults>;
    type Child = MaybeAggSegment;

    fn for_segment(
        &self,
        ord: velocore::SegmentOrdinal,
        reader: &velocore::SegmentReader,
    ) -> velocore::Result<Self::Child> {
        Ok(MaybeAggSegment(match &self.0 {
            Some(c) => Some(c.for_segment(ord, reader)?),
            None => None,
        }))
    }

    fn requires_scoring(&self) -> bool {
        false
    }

    fn merge_fruits(
        &self,
        segment_fruits: Vec<Option<velocore::Result<IntermediateAggregationResults>>>,
    ) -> velocore::Result<Self::Fruit> {
        let Some(inner) = &self.0 else { return Ok(None) };
        let present: Vec<velocore::Result<IntermediateAggregationResults>> =
            segment_fruits.into_iter().flatten().collect();
        if present.is_empty() {
            return Ok(None);
        }
        inner.merge_fruits(present).map(Some)
    }
}
impl velocore::collector::SegmentCollector for MaybeAggSegment {
    type Fruit = Option<velocore::Result<IntermediateAggregationResults>>;

    fn collect(&mut self, doc: velocore::DocId, score: velocore::Score) {
        if let Some(c) = &mut self.0 {
            c.collect(doc, score);
        }
    }

    /// Forwarding this matters: VeloCore's aggregation collects a block at a
    /// time, and the default implementation would unroll it back into one call
    /// per document.
    fn collect_block(&mut self, docs: &[velocore::DocId]) {
        if let Some(c) = &mut self.0 {
            c.collect_block(docs);
        }
    }

    fn harvest(self) -> Self::Fruit {
        self.0.map(|c| c.harvest())
    }
}

/// A failure while a shard was searched, as a response. A script that failed
/// carries its own error, which is reported as that shard's failure.
pub(crate) fn search_error_response(text: &str, index: &str) -> Response {
    // A search that ran past the memory its aggregations were given did not
    // fail to parse and was not a bad request: it was refused, by the
    // breaker whose budget it was spending. It is told the way the breaker
    // tells it, so a client reads the same `circuit_breaking_exception` and
    // the same 429 wherever the limit was reached.
    if text.contains("memory limit was exceeded") {
        crate::breaker::REQUEST.count_trip();
        let (limit, wanted) = agg_memory_numbers(text);
        return crate::breaker::Trip {
            breaker: "request",
            label: format!("<agg [{index}]>"),
            wanted,
            limit,
            durability: "TRANSIENT",
        }
        .response_saying(text);
    }
    // the engine quotes the message it carries: the JSON ends before the
    // closing quote
    if let Some(detail) =
        text.split_once("script_exception:").map(|(_, d)| d.trim_end_matches('\''))
        && let Ok(detail) = serde_json::from_str::<Value>(detail)
    {
        let mut root = detail.clone();
        if let Some(o) = root.as_object_mut() {
            o.remove("caused_by");
        }
        let body = json!({
            "error": {
                "root_cause": [root],
                "type": "search_phase_execution_exception",
                "reason": "Partial shards failure",
                "phase": "query",
                "grouped": true,
                "failed_shards": [{
                    "shard": 0, "index": index, "node": "node0", "reason": detail,
                }],
            },
            "status": 400,
        });
        return axum::response::IntoResponse::into_response((
            StatusCode::BAD_REQUEST,
            axum::Json(body),
        ));
    }
    err(StatusCode::BAD_REQUEST, "search_phase_execution_exception", text.to_string())
}

/// The two numbers VeloCore names when an aggregation runs past its budget:
/// what it was allowed, and what it had reached.
///
/// They are written as `Limit: 146 B, Current: 568 B` -- a number, a space
/// and a unit -- so they are read back rather than guessed at, and a message
/// worded differently one day leaves zeroes rather than wrong numbers.
fn agg_memory_numbers(text: &str) -> (u64, u64) {
    let after = |word: &str| -> u64 {
        let Some(rest) = text.split_once(word) else { return 0 };
        let mut parts = rest.1.split_whitespace();
        let Some(n) = parts.next().and_then(|n| n.trim_end_matches(',').parse::<f64>().ok()) else {
            return 0;
        };
        let scale: f64 = match parts.next().map(|u| u.trim_end_matches(',').to_lowercase()) {
            Some(u) if u.starts_with("kb") || u.starts_with("k") => 1024.0,
            Some(u) if u.starts_with("mb") || u.starts_with("m") => 1024.0 * 1024.0,
            Some(u) if u.starts_with("gb") || u.starts_with("g") => 1024.0 * 1024.0 * 1024.0,
            _ => 1.0,
        };
        (n * scale) as u64
    };
    (after("Limit:"), after("Current:"))
}

/// An aggregation that cannot read its field fails on the shard, and is
/// told as OpenSearch tells it: every shard failed, for that reason.
fn phase_failure_of(e: Response, index: &str) -> Response {
    let Some(what) = e.extensions().get::<crate::api::shared::ErrorKind>().cloned() else {
        return e;
    };
    if !what.reason.contains("is not supported for aggregation")
        && !what.reason.starts_with("Fielddata is not supported on field")
        && !what.reason.starts_with("[variable_width_histogram] cannot be nested")
    {
        return e;
    }
    let detail = json!({"type": what.kind, "reason": what.reason});
    let wrapped = json!({
        "error": {
            "root_cause": [detail],
            "type": "search_phase_execution_exception",
            "reason": "all shards failed",
            "phase": "query",
            "grouped": true,
            "failed_shards": [{"shard": 0, "index": index, "node": "node0", "reason": detail}],
            // the shard's exception wraps the cause once more
            "caused_by": {"type": what.kind, "reason": what.reason, "caused_by": detail},
        },
        "status": 400,
    });
    axum::response::IntoResponse::into_response((StatusCode::BAD_REQUEST, axum::Json(wrapped)))
}

/// Replace every clause that asks after `_index` with the answer for this
/// index: all of its documents, or none of them.
pub(crate) fn answer_index_name(query: &mut Value, index: &str, aliases: &[String]) {
    use serde_json::json;
    let everything = json!({"match_all": {}});
    let nothing = json!({"bool": {"must_not": [{"match_all": {}}]}});
    let named = |value: &Value| -> Option<String> {
        match value {
            Value::String(s) => Some(s.clone()),
            Value::Object(o) => o.get("value").and_then(|v| v.as_str()).map(|s| s.to_string()),
            _ => None,
        }
    };
    let Some(o) = query.as_object_mut() else {
        if let Some(a) = query.as_array_mut() {
            a.iter_mut().for_each(|q| answer_index_name(q, index, aliases));
        }
        return;
    };
    let is_me = |want: &str| want == index || aliases.iter().any(|a| a == want);
    let decided = if let Some(v) = o.get("term").and_then(|t| t.get("_index")) {
        named(v).map(|want| is_me(&want))
    } else if let Some(Value::Array(list)) = o.get("terms").and_then(|t| t.get("_index")) {
        Some(list.iter().filter_map(|v| v.as_str()).any(is_me))
    } else if let Some(v) = o.get("prefix").and_then(|t| t.get("_index")) {
        // the alias names count here as they do for `term`: a prefix of an
        // alias of this index reaches its documents
        named(v)
            .map(|want| index.starts_with(&want) || aliases.iter().any(|a| a.starts_with(&want)))
    } else if let Some(v) = o.get("wildcard").and_then(|t| t.get("_index")) {
        named(v).map(|want| {
            crate::store::glob_match(&want, index)
                || aliases.iter().any(|a| crate::store::glob_match(&want, a))
        })
    } else {
        None
    };
    if let Some(hit) = decided {
        *query = if hit { everything } else { nothing };
        return;
    }
    for (_, v) in o.iter_mut() {
        answer_index_name(v, index, aliases);
    }
}
