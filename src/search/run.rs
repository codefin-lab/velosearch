//! One search, from the request to the answer.

use super::*;

/// Every `rank_feature` clause a query holds, wherever it stands in it.
fn collect_rank_features(node: &Value, out: &mut Vec<Value>) {
    match node {
        Value::Object(o) => {
            for (key, value) in o {
                if key == "rank_feature" {
                    out.push(value.clone());
                } else {
                    collect_rank_features(value, out);
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|item| collect_rank_features(item, out)),
        _ => {}
    }
}

/// The score a rank feature asks for: the value of a field, curved.
///
/// A feature says how much a document is worth on its own -- how many people
/// link to it, how short its address is -- and the curve says how quickly that
/// worth stops mattering.
fn rescore_by_rank_features(
    searchers: &[(String, velocore::Searcher, std::sync::Arc<crate::store::IdxLock>)],
    cands: &mut [Cand],
    features: &[Value],
) {
    for cand in cands.iter_mut() {
        let (_, searcher, st) = &searchers[cand.shard];
        let g = st.read();
        let Some((_, source)) = source_of(searcher, &g, cand.addr) else { continue };
        let mut total = 0.0f32;
        for spec in features {
            let field = spec.get("field").and_then(|v| v.as_str()).unwrap_or("");
            let held = source
                .pointer(&format!("/{}", field.replace('.', "/")))
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0) as f32;
            let boost = spec.get("boost").and_then(|v| v.as_f64()).unwrap_or(1.0) as f32;
            // whether a larger value is worth more is the field's own
            // property, which the query may not override
            let positive = g
                .mapping
                .field_option(field, "positive_score_impact")
                .and_then(|v| v.as_bool())
                // a `rank_features` field is a map of features, so the option
                // stands on the field the feature is written under
                .or_else(|| {
                    let (parent, _) = field.rsplit_once('.')?;
                    g.mapping.field_option(parent, "positive_score_impact")?.as_bool()
                })
                .or_else(|| spec.get("positive_score_impact").and_then(|v| v.as_bool()))
                .unwrap_or(true);
            // a feature whose larger values are worth less is held as its own
            // reciprocal, so every curve below is written the one way round
            let value = match positive {
                true => held,
                false => 1.0 / held.max(f32::MIN_POSITIVE),
            };
            let curved = if let Some(log) = spec.get("log") {
                let scaling =
                    log.get("scaling_factor").and_then(|v| v.as_f64()).unwrap_or(1.0) as f32;
                (scaling + value).ln()
            } else if let Some(saturation) = spec.get("saturation") {
                let pivot = saturation
                    .get("pivot")
                    .and_then(|v| v.as_f64())
                    .map(|p| p as f32)
                    .unwrap_or(value.max(1.0));
                value / (value + pivot)
            } else if let Some(sigmoid) = spec.get("sigmoid") {
                let pivot = sigmoid.get("pivot").and_then(|v| v.as_f64()).unwrap_or(1.0) as f32;
                let exponent =
                    sigmoid.get("exponent").and_then(|v| v.as_f64()).unwrap_or(1.0) as f32;
                value.powf(exponent) / (value.powf(exponent) + pivot.powf(exponent))
            } else if spec.get("linear").is_some() {
                value
            } else {
                // without a curve named, saturation with the value as its own
                // pivot is what OpenSearch settles on
                value / (value + 1.0)
            };
            total += boost * curved;
        }
        cand.score = total;
    }
}

/// The score `function_score` asks for, in place of the one the query gave.
///
/// A function may name a filter -- it counts only for the documents that
/// match it -- and either a weight, or a field whose value stands for how
/// much the document is worth. `boost_mode` says how what the functions make
/// meets what the query scored.
/// A script's failure, reported the way a search reports one: the shards
/// failed, and the script exception is why.
pub(crate) fn search_script_failure(e: crate::painless::ScriptError, index: &str) -> Response {
    let detail = e.to_json();
    let mut root = detail.clone();
    if let Some(o) = root.as_object_mut() {
        o.remove("caused_by");
    }
    let body = json!({
        "error": {
            "root_cause": [root],
            "type": "search_phase_execution_exception",
            "reason": "all shards failed",
            "phase": "query",
            "grouped": true,
            "failed_shards": [{
                "shard": 0,
                "index": index,
                "node": "node0",
                "reason": detail,
            }],
        },
        "status": 400,
    });
    axum::response::IntoResponse::into_response((StatusCode::BAD_REQUEST, axum::Json(body)))
}

/// A double the way Java prints one: `-9.0`, not `-9`.
fn java_double(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e16 { format!("{v:.1}") } else { v.to_string() }
}

/// The same failure, as a walk over documents reports it: some shards
/// answered before one did not.
pub(crate) fn search_script_failure_partial(
    e: crate::painless::ScriptError,
    index: &str,
) -> Response {
    let detail = e.to_json();
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
                "shard": 0,
                "index": index,
                "node": "node0",
                "reason": detail,
            }],
        },
        "status": 400,
    });
    axum::response::IntoResponse::into_response((StatusCode::BAD_REQUEST, axum::Json(body)))
}

/// A failure of one kind and reason, reported as the shards failing.
pub(crate) fn search_shard_failure(kind: &str, reason: &str, index: &str) -> Response {
    let body = json!({
        "error": {
            "root_cause": [{"type": kind, "reason": reason}],
            "type": "search_phase_execution_exception",
            "reason": "all shards failed",
            "phase": "query",
            "grouped": true,
            "failed_shards": [{
                "shard": 0,
                "index": index,
                "node": "node0",
                "reason": {"type": kind, "reason": reason},
            }],
        },
        "status": 400,
    });
    axum::response::IntoResponse::into_response((StatusCode::BAD_REQUEST, axum::Json(body)))
}

/// The term statistics a score script asks for, read from the segment the
/// document sits in: how often a term appears in this document, in how many
/// documents, and how many tokens the field holds in all.
fn term_stats_for(
    searcher: &velocore::Searcher,
    st: &IdxState,
    addr: DocAddress,
) -> crate::painless::contexts::TermStats {
    use velocore::schema::IndexRecordOption;
    let reader = searcher.segment_reader(addr.segment_ord).clone();
    let fields = st.fields;
    let mapping = st.mapping.clone();
    let doc = addr.doc_id;
    Box::new(move |what: &str, field: &str, term: &str| -> f64 {
        // a keyword is kept whole in the raw view; text is tokenised into
        // the dynamic one, and a term of it is one lowercased token
        let kind = mapping.type_of(field).unwrap_or("keyword");
        let (f, text) = if matches!(kind, "text" | "match_only_text" | "annotated_text") {
            (fields.dynamic, term.to_lowercase())
        } else {
            (fields.raw, term.to_string())
        };
        let mut t = velocore::schema::Term::from_field_json_path(f, field, true);
        t.append_type_and_str(&text);
        let Ok(inverted) = reader.inverted_index(f) else { return 0.0 };
        match what {
            "termFreq" => inverted
                .read_postings(&t, IndexRecordOption::WithFreqs)
                .ok()
                .flatten()
                .map(|mut postings| {
                    use velocore::{DocSet, postings::Postings};
                    if postings.seek(doc) == doc { postings.term_freq() as f64 } else { 0.0 }
                })
                .unwrap_or(0.0),
            "docFreq" => inverted.doc_freq(&t).map(|n| n as f64).unwrap_or(0.0),
            "totalTermFreq" => inverted
                .read_postings(&t, IndexRecordOption::WithFreqs)
                .ok()
                .flatten()
                .map(|mut postings| {
                    use velocore::{DocSet, postings::Postings};
                    let mut total = 0.0;
                    let mut d = postings.doc();
                    while d != velocore::TERMINATED {
                        total += postings.term_freq() as f64;
                        d = postings.advance();
                    }
                    total
                })
                .unwrap_or(0.0),
            // the field's terms sit together in the dictionary, under the
            // path they share; each one's postings say how often it appears
            "sumTotalTermFreq" | "sumDocFreq" => {
                let prefix = velocore::schema::Term::from_field_json_path(f, field, true);
                let low = prefix.serialized_value_bytes().to_vec();
                let mut high = low.clone();
                high.push(0xff);
                let Ok(mut stream) = inverted.terms().range().ge(&low).lt(&high).into_stream()
                else {
                    return 0.0;
                };
                let mut total = 0.0;
                while stream.advance() {
                    let info = stream.value().clone();
                    if what == "sumDocFreq" {
                        total += info.doc_freq as f64;
                        continue;
                    }
                    if let Ok(mut postings) =
                        inverted.read_postings_from_terminfo(&info, IndexRecordOption::WithFreqs)
                    {
                        use velocore::{DocSet, postings::Postings};
                        let mut d = postings.doc();
                        while d != velocore::TERMINATED {
                            total += postings.term_freq() as f64;
                            d = postings.advance();
                        }
                    }
                }
                total
            }
            _ => 0.0,
        }
    })
}

/// `script_score`: the score is what the script says, given the query's
/// score and the document.
fn rescore_by_script(
    searchers: &[(String, velocore::Searcher, std::sync::Arc<crate::store::IdxLock>)],
    cands: &mut Vec<Cand>,
    spec: &Value,
) -> std::result::Result<(), Response> {
    let Some(script) = spec.get("script") else { return Ok(()) };
    let min_score = spec.get("min_score").and_then(|v| v.as_f64());
    let boost = spec.get("boost").and_then(|v| v.as_f64()).unwrap_or(1.0) as f32;
    let mut failure = None;
    cands.retain_mut(|cand| {
        if failure.is_some() {
            return true;
        }
        let (name, searcher, st) = &searchers[cand.shard];
        let g = st.read();
        let Some((_, source)) = source_of(searcher, &g, cand.addr) else { return true };
        let expanded = crate::store::expand_for_indexing(source, &g.mapping);
        let stats = term_stats_for(searcher, &g, cand.addr);
        match crate::painless::contexts::run_on_doc_with(
            script,
            &expanded,
            &g.mapping,
            cand.score as f64,
            Some(stats),
        ) {
            Ok(v) => {
                let made = v.as_f64().unwrap_or(0.0);
                if made < 0.0 {
                    failure = Some(search_shard_failure(
                        "illegal_argument_exception",
                        &format!(
                            "script score function must not produce negative scores, but got: \
                             [{made}]"
                        ),
                        name,
                    ));
                    return true;
                }
                cand.score = made as f32 * boost;
                min_score.map(|m| made >= m).unwrap_or(true)
            }
            Err(e) => {
                failure = Some(search_script_failure(e, name));
                true
            }
        }
    });
    match failure {
        Some(r) => Err(r),
        None => Ok(()),
    }
}

fn rescore_by_functions(
    searchers: &[(String, velocore::Searcher, std::sync::Arc<crate::store::IdxLock>)],
    cands: &mut [Cand],
    spec: &Value,
) -> std::result::Result<(), Response> {
    let mut functions: Vec<Value> =
        spec.get("functions").and_then(|f| f.as_array()).cloned().unwrap_or_default();
    // a single function may be written beside the query rather than in a list
    for named in ["field_value_factor", "weight", "random_score", "script_score"] {
        if let Some(one) = spec.get(named) {
            functions.push(json!({ named: one }));
        }
    }
    if functions.is_empty() {
        return Ok(());
    }
    let score_mode = spec.get("score_mode").and_then(|v| v.as_str()).unwrap_or("multiply");
    let boost_mode = spec.get("boost_mode").and_then(|v| v.as_str()).unwrap_or("multiply");
    let query_boost = spec.get("boost").and_then(|v| v.as_f64()).unwrap_or(1.0) as f32;
    for cand in cands.iter_mut() {
        let (name, searcher, st) = &searchers[cand.shard];
        let g = st.read();
        let Some((_, source)) = source_of(searcher, &g, cand.addr) else { continue };
        let mut made: Vec<f32> = Vec::new();
        // the script sees the document as the index read it
        let mut expanded: Option<Value> = None;
        for function in &functions {
            // a function with a filter counts only where the filter matches
            if let Some(filter) = function.get("filter")
                && !matches_here(&source, filter)
            {
                continue;
            }
            let weight = function.get("weight").and_then(|v| v.as_f64()).unwrap_or(1.0) as f32;
            // `gauss`, `exp` and `linear` score by how far a value is from an
            // origin. None of the three was read: they counted as one, so a
            // decay over price or date or place changed nothing, and the
            // documents came back in the order the query alone gave them.
            if let Some((shape, decay_spec)) =
                ["gauss", "exp", "linear"].iter().find_map(|k| function.get(*k).map(|d| (*k, d)))
            {
                made.push(weight * decay_value(shape, decay_spec, &source).unwrap_or(1.0));
                continue;
            }
            let value = match function.get("field_value_factor") {
                None if function.get("script_score").is_some() => {
                    let script = function.pointer("/script_score/script").unwrap_or(&Value::Null);
                    let seen = expanded.get_or_insert_with(|| {
                        crate::store::expand_for_indexing(source.clone(), &g.mapping)
                    });
                    let stats = term_stats_for(searcher, &g, cand.addr);
                    match crate::painless::contexts::run_on_doc_with(
                        script,
                        seen,
                        &g.mapping,
                        cand.score as f64,
                        Some(stats),
                    ) {
                        Ok(v) => {
                            let made = v.as_f64().unwrap_or(0.0);
                            if made < 0.0 {
                                return Err(search_shard_failure(
                                    "illegal_argument_exception",
                                    &format!(
                                        "script score function must not produce negative \
                                         scores, but got: [{}]",
                                        java_double(made)
                                    ),
                                    name,
                                ));
                            }
                            made as f32
                        }
                        Err(e) => return Err(search_script_failure(e, name)),
                    }
                }
                Some(spec) => {
                    let field = spec.get("field").and_then(|v| v.as_str()).unwrap_or("");
                    let factor = spec.get("factor").and_then(|v| v.as_f64()).unwrap_or(1.0);
                    let missing = spec.get("missing").and_then(|v| v.as_f64());
                    let held = source
                        .pointer(&format!("/{}", field.replace('.', "/")))
                        .and_then(|v| v.as_f64())
                        .or(missing)
                        .unwrap_or(0.0);
                    let scaled = held * factor;
                    (match spec.get("modifier").and_then(|v| v.as_str()).unwrap_or("none") {
                        "log" => scaled.log10(),
                        "log1p" => (1.0 + scaled).log10(),
                        "log2p" => (2.0 + scaled).log10(),
                        "ln" => scaled.ln(),
                        "ln1p" => (1.0 + scaled).ln_1p(),
                        "ln2p" => (2.0 + scaled).ln(),
                        "square" => scaled * scaled,
                        "sqrt" => scaled.sqrt(),
                        "reciprocal" => {
                            if scaled == 0.0 {
                                0.0
                            } else {
                                1.0 / scaled
                            }
                        }
                        _ => scaled,
                    }) as f32
                }
                None => 1.0,
            };
            made.push(weight * value);
        }
        if made.is_empty() {
            continue;
        }
        let combined = match score_mode {
            "sum" => made.iter().sum(),
            "avg" => made.iter().sum::<f32>() / made.len() as f32,
            "first" => made[0],
            "max" => made.iter().cloned().fold(f32::MIN, f32::max),
            "min" => made.iter().cloned().fold(f32::MAX, f32::min),
            _ => made.iter().product(),
        };
        cand.score = match boost_mode {
            "replace" => combined,
            "sum" => cand.score + combined,
            "avg" => (cand.score + combined) / 2.0,
            "max" => cand.score.max(combined),
            "min" => cand.score.min(combined),
            _ => cand.score * combined,
        } * query_boost;
    }
    Ok(())
}

/// Whether a document, as it stands, answers a simple filter.
///
/// Only the filters a function names are read here -- a term, a range, a
/// match on one field -- which is what `function_score` puts in front of a
/// weight.
fn matches_here(source: &Value, filter: &Value) -> bool {
    let Some((kind, body)) = filter.as_object().and_then(|o| o.iter().next()) else {
        return true;
    };
    let held = |field: &str| source.pointer(&format!("/{}", field.replace('.', "/"))).cloned();
    match kind.as_str() {
        "match_all" => true,
        "match_none" => false,
        "term" | "match" | "match_phrase" => {
            let Some((field, wanted)) = body.as_object().and_then(|o| o.iter().next()) else {
                return false;
            };
            let wanted = wanted.get("value").or_else(|| wanted.get("query")).unwrap_or(wanted);
            // a field may hold several values, and matches when any of them
            // does: `tags: ["a", "b"]` was compared whole with `a`, never
            // matched, and a weight behind a filter on it was never applied
            let one = |v: &Value| match v {
                Value::String(s) => wanted.as_str().map(|w| s.contains(w)).unwrap_or(false),
                other => other == wanted,
            };
            match held(field) {
                Some(Value::Array(values)) => values.iter().any(one),
                Some(v) => one(&v),
                None => false,
            }
        }
        "terms" => {
            let Some((field, wanted)) = body.as_object().and_then(|o| o.iter().next()) else {
                return false;
            };
            let held = held(field);
            wanted
                .as_array()
                .map(|any| any.iter().any(|w| held.as_ref() == Some(w)))
                .unwrap_or(false)
        }
        "range" => {
            let Some((field, bounds)) = body.as_object().and_then(|o| o.iter().next()) else {
                return false;
            };
            let Some(value) = held(field).and_then(|v| v.as_f64()) else { return false };
            let past = |name: &str, ok: fn(f64, f64) -> bool| {
                bounds
                    .get(name)
                    .and_then(|v| v.as_f64())
                    .map(|edge| ok(value, edge))
                    .unwrap_or(true)
            };
            past("gte", |v, e| v >= e)
                && past("gt", |v, e| v > e)
                && past("lte", |v, e| v <= e)
                && past("lt", |v, e| v < e)
        }
        "exists" => {
            body.get("field").and_then(|v| v.as_str()).map(|f| held(f).is_some()).unwrap_or(false)
        }
        "bool" => {
            let all = |name: &str, want: bool| {
                body.get(name)
                    .and_then(|v| v.as_array())
                    .map(|cs| cs.iter().all(|c| matches_here(source, c) == want))
                    .unwrap_or(true)
            };
            all("must", true) && all("filter", true) && all("must_not", false)
        }
        _ => true,
    }
}

/// Run a search across every resolved index and merge the results.
/// Every hit a query matches, a page at a time.
///
/// An aggregation written as a script is folded over the documents rather
/// than over a column, so it has to see all of them. Reading one page and
/// answering as though that were the index -- which is what these did -- is
/// an answer that is quietly a fraction of the truth.
pub fn walk_every_hit(
    store: &Store,
    targets: &[String],
    query: &Value,
    track_scores: bool,
) -> std::result::Result<Outcome, Response> {
    walk_every_hit_of(store, targets, query, track_scores, None)
}

/// Whether a request body names a script anywhere in it.
///
/// Blunt on purpose: every place a script can appear -- a `script` query, a
/// `script_score`, a `_script` sort, `script_fields`, a scripted aggregation,
/// a `function_score` -- writes the word, and a caller whose view of the
/// index is narrowed may not run any of them.
fn mentions_a_script(body: &Value) -> bool {
    match body {
        Value::Object(o) => {
            o.iter().any(|(k, v)| k == "script" || k == "_script" || mentions_a_script(v))
        }
        Value::Array(a) => a.iter().any(mentions_a_script),
        _ => false,
    }
}

/// How many documents a `post_filter` may be answered over. It is answered by
/// looking at every document the filter matches, so this is the size of what
/// one request may ask the node to hold.
const MOST_POST_FILTERED: usize = 1_000_000;

/// Every document a query matches, read in one pass rather than paged.
///
/// A page-by-page walk asks for `from + size` each time, so the last pages
/// collect and prune a million candidates apiece: reading a million documents
/// cost the square of that. This asks each searcher for the whole set of
/// matching addresses once and reads the sources from it.
pub fn every_matching_source(
    store: &Store,
    targets: &[String],
    query: &Value,
    fields: &[String],
    ceiling: usize,
) -> std::result::Result<Vec<Value>, Response> {
    let mut out: Vec<Value> = Vec::new();
    for name in targets {
        let Some(st) = store.get(name) else { continue };
        let g = st.read();
        let searcher = g.reader.searcher();
        let ctx = crate::query::Ctx {
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
        // the caller's own filter is part of the query, as it is everywhere
        // a search is run
        let asked = crate::security::with_dls(store, name, Some(query.clone()))
            .unwrap_or_else(|| query.clone());
        let q = crate::query::build(&ctx, &asked)
            .map_err(|e| err(StatusCode::BAD_REQUEST, "query_shard_exception", e.to_string()))?;
        // asked how many before they are collected: refusing once the set is
        // in memory is refusing after the harm is done
        let how_many = searcher.search(&q, &velocore::collector::Count).map_err(|e| {
            err(StatusCode::INTERNAL_SERVER_ERROR, "search_exception", e.to_string())
        })?;
        if out.len() + how_many > ceiling {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "too_many_buckets_exception",
                format!(
                    "This aggregation reads every matching document, and this one matches more \
                     than [{ceiling}]. Narrow the query, or aggregate over a filtered subset."
                ),
            ));
        }
        let found = searcher.search(&q, &velocore::collector::DocSetCollector).map_err(|e| {
            err(StatusCode::INTERNAL_SERVER_ERROR, "search_exception", e.to_string())
        })?;
        if out.len() + found.len() > ceiling {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "too_many_buckets_exception",
                format!(
                    "This aggregation reads every matching document, and this one matches more \
                     than [{ceiling}]. Narrow the query, or aggregate over a filtered \
                     subset."
                ),
            ));
        }
        let view = crate::security::view::view_for(store, name);
        for addr in found {
            let Some((id, mut source)) = source_of(&searcher, &g, addr) else { continue };
            // and what the caller may not see of a document is not read here
            // either
            if let Some(view) = &view {
                view.filter_source(&mut source);
            }
            let mut kept = source;
            if !fields.is_empty() {
                kept = crate::api::apply_source_selector(&kept, &json!(fields));
            }
            out.push(json!({"_index": name, "_id": id, "_source": kept}));
        }
    }
    Ok(out)
}

/// The same, reading only the fields named.
pub fn walk_every_hit_of(
    store: &Store,
    targets: &[String],
    query: &Value,
    track_scores: bool,
    source: Option<Value>,
) -> std::result::Result<Outcome, Response> {
    const PAGE: usize = 10_000;
    const CEILING: usize = 1_000_000;
    let mut out: Option<Outcome> = None;
    let mut from = 0usize;
    loop {
        let mut probe = json!({
            "query": query.clone(),
            "from": from,
            "size": PAGE,
            "track_scores": track_scores,
        });
        if let Some(fields) = &source {
            probe["_source"] = fields.clone();
        }
        let page = crate::search::as_the_server(|| {
            run(store, &targets.join(","), &probe, &Params::new())
        })?;
        let read = page.hits.len();
        match out.as_mut() {
            Some(all) => all.hits.extend(page.hits),
            None => out = Some(page),
        }
        if read < PAGE {
            return Ok(out.unwrap_or_else(|| unreachable!("a page was read")));
        }
        from += PAGE;
        if from >= CEILING {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "too_many_buckets_exception",
                format!(
                    "A scripted aggregation reads every matching document, and this one matches \
                     more than [{CEILING}]. Narrow the query, or aggregate over a field."
                ),
            ));
        }
    }
}

/// A script written as `{"id": "..."}` replaced by the script that id names.
///
/// `Compiled::of` resolves a stored script when it is given somewhere to look
/// it up, and most of the places that compile one pass the store. The places
/// that run a script over a document read it out of the request body and had
/// nowhere to look, so a stored script in `script_fields`, in a `_script`
/// sort, in a `script` query or in `_explain` was answered with `unable to
/// find script [...] in cluster state` -- the id was resolved for a
/// `_update` and not for a search. Resolving it once, here, puts the source
/// where every one of those reads it.
fn inline_stored_scripts(store: &Store, node: &mut Value) -> bool {
    let mut any = false;
    match node {
        Value::Object(o) => {
            if let Some(spec) = o.get_mut("script")
                && let Some(inner) = spec.as_object_mut()
                && inner.get("source").is_none()
                && inner.get("inline").is_none()
                && let Some(id) = inner.get("id").and_then(|v| v.as_str()).map(|s| s.to_string())
                && let Some(found) = store.stored_script(&id)
            {
                if let Some(text) = found.get("source") {
                    inner.insert("source".into(), text.clone());
                }
                if let Some(lang) = found.get("lang")
                    && inner.get("lang").is_none()
                {
                    inner.insert("lang".into(), lang.clone());
                }
                inner.remove("id");
                any = true;
            }
            for v in o.values_mut() {
                any |= inline_stored_scripts(store, v);
            }
        }
        Value::Array(a) => {
            for v in a {
                any |= inline_stored_scripts(store, v);
            }
        }
        _ => {}
    }
    any
}

pub fn run(
    store: &Store,
    expr: &str,
    body: &Value,
    p: &Params,
) -> std::result::Result<Outcome, Response> {
    // a `wrapper` carries its query as base64 JSON: it is opened first, so
    // that everything below reads the query it holds
    let unwrapped;
    let body = if crate::search::extras::names_a_wrapper(body) {
        let mut copy = body.clone();
        crate::search::extras::unwrap_wrappers(&mut copy)?;
        unwrapped = copy;
        &unwrapped
    } else {
        body
    };
    // a stored script named by id is the script it names, from here on
    let inlined;
    let body = if mentions_a_script(body) {
        let mut copy = body.clone();
        if inline_stored_scripts(store, &mut copy) {
            inlined = copy;
            &inlined
        } else {
            body
        }
    } else {
        body
    };
    // A point in time says which nodes hold its parts, and a search held to
    // one is asked of those nodes whatever the routing says now: a copy made
    // since does not hold the readers the point in time was opened over.
    if let Some(pit_id) = body.pointer("/pit/id")
        && !p.contains_key("_native_only")
        && !p.contains_key("_local_only")
    {
        let id = pit_of(pit_id)?;
        if let Some(plan) = crate::cluster::search::pit_plan(&id) {
            return crate::cluster::search::run_spanning(store, "", body, p, plan);
        }
    }
    // An alias may be a narrower view of an index, and the filter that makes
    // it narrower belongs to the request rather than to the query: it is put
    // where every path that builds a query for one index can read it, which
    // is the only place that knows which index it is building for. Nothing
    // read it before, so a search -- and a `_delete_by_query` -- through a
    // filtered alias reached the whole index.
    //
    // A search already inside such a scope keeps it: a scratch index built
    // for `derived` fields, or an aggregation running a search of its own,
    // must not have the filter laid over it a second time under a name the
    // alias does not cover.
    //
    // A search narrowed by `routing` or `preference=_shards:` is narrowed the
    // same way, for the same reason: the shards it may ask are a property of
    // the request, and each index's share of it is kept to the documents
    // those shards hold.
    if crate::security::layer::ALIAS_FILTERS.try_with(|_| ()).is_err() {
        let mut filters = store.alias_filters(expr);
        let narrowed = store.search_narrowing(
            expr,
            p.get("routing").map(|s| s.as_str()),
            p.get("preference").map(|s| s.as_str()),
        );
        for (name, shards) in narrowed {
            let Some(st) = store.get(&name) else { continue };
            let on_shards = st.read().on_shards_filter(&shards);
            let combined = match filters.remove(&name) {
                Some(alias) => json!({"bool": {"filter": [alias, on_shards]}}),
                None => on_shards,
            };
            filters.insert(name, combined);
        }
        if !filters.is_empty() {
            return crate::security::layer::ALIAS_FILTERS
                .sync_scope(filters, || run(store, expr, body, p));
        }
    }
    // a `derived` section defines fields for this search alone: the
    // documents are copied into a scratch index mapped with them, and the
    // search runs there
    if let Some(defs) = body.get("derived").and_then(|d| d.as_object())
        && !defs.is_empty()
        && !expr.starts_with("_derived")
    {
        return run_with_derived(store, expr, body, p, defs);
    }
    // indices held on other nodes: a coordinator asks each node for its
    // share and merges; a node answering one, or a request told to stay
    // here, runs as it always did
    if !p.contains_key("_native_only")
        && !p.contains_key("_local_only")
        && body.get("pit").is_none()
        && let Some(plan) =
            crate::cluster::search::plan(store, expr, p.get("preference").map(|s| s.as_str()))
        && plan.spans_nodes()
    {
        return crate::cluster::search::run_spanning(store, expr, body, p, plan);
    }
    const BODY_KEYS: &[&str] = &[
        "derived",
        "query",
        "from",
        "size",
        "sort",
        "_source",
        "aggs",
        "aggregations",
        "post_filter",
        "highlight",
        "track_total_hits",
        "track_scores",
        "stored_fields",
        "docvalue_fields",
        "script_fields",
        "explain",
        "version",
        "seq_no_primary_term",
        "min_score",
        "timeout",
        "terminate_after",
        "search_after",
        "collapse",
        "rescore",
        "indices_boost",
        "profile",
        "suggest",
        "fields",
        "slice",
        "pit",
        "stats",
        "batched_reduce_size",
        "ext",
        "knn",
    ];
    if let Some(o) = body.as_object() {
        for k in o.keys() {
            if !BODY_KEYS.contains(&k.as_str()) {
                return Err(err(
                    StatusCode::BAD_REQUEST,
                    "parsing_exception",
                    format!("Unknown key for a START_OBJECT in [{k}]."),
                ));
            }
        }
    }
    for key in ["from", "size"] {
        if let Some(n) = as_i64(body_or_param(body, p, key))
            && n < 0
        {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "illegal_argument_exception",
                format!("[{key}] parameter cannot be negative, found [{n}]"),
            ));
        }
    }
    if let Some(n) = as_i64(p.get("batched_reduce_size").map(|v| json!(v)))
        && n < 2
    {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!("batchedReduceSize must be >= 2, got {n}"),
        ));
    }
    if let Some(n) = as_i64(p.get("pre_filter_shard_size").map(|v| json!(v)))
        && n < 1
    {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!("preFilterShardSize must be >= 1, got {n}"),
        ));
    }
    if let Some(n) = as_i64(
        body.get("track_total_hits")
            .cloned()
            .or_else(|| p.get("track_total_hits").map(|v| json!(v))),
    ) && n < -1
    {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!("[track_total_hits] parameter must be positive or equals to -1, got {n}"),
        ));
    }
    if let Some(st) = p.get("search_type")
        && (st == "query_and_fetch" || st == "dfs_query_and_fetch")
    {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!("Unsupported search type [{st}]"),
        ));
    }
    validate_params(body, p)?;
    let from = as_usize(body_or_param(body, p, "from")).unwrap_or(0);
    let size = as_usize(body_or_param(body, p, "size")).unwrap_or(10);
    // A request carrying a suggester and nothing else asks the term
    // dictionary a question about words, not the index a question about
    // documents. There is no query to run, so none is run and no hits are
    // reported -- not even the ones a `match_all` nobody wrote would match.
    let suggest_only = body.get("suggest").is_some()
        && body.get("query").is_none()
        && body.get("aggs").is_none()
        && body.get("aggregations").is_none();
    let size = if suggest_only { 0 } else { size };
    // reading documents skips the closed indices a pattern would otherwise
    // reach; a closed index named outright is a different complaint
    // `pit` names a point in time rather than an index expression: it carries
    // both which indices to search and the reader each is searched through
    let pit = match body.pointer("/pit/id") {
        Some(v) => {
            let id = pit_of(v)?;
            let keep = body
                .pointer("/pit/keep_alive")
                .and_then(|k| k.as_str())
                .and_then(crate::api::shared::parse_keep_alive)
                .map(|s| s * 1000);
            let Some(held) = store.read_pit(&id.token, keep) else {
                return Err(pit_missing(&id));
            };
            // an index deleted since, or made again under its name, is not
            // the index the point in time read
            let gone = held.parts.iter().any(|part| {
                store.get(&part.index).map(|st| st.read().uuid != part.uuid).unwrap_or(true)
            });
            if gone {
                return Err(pit_missing(&id));
            }
            Some(held)
        }
        None => None,
    };
    let pit_expr = pit.as_ref().map(|h| h.names().join(","));
    let expr: &str = pit_expr.as_deref().unwrap_or(expr);
    let pit_parts: std::collections::HashMap<String, crate::store::PitPart> = pit
        .as_ref()
        .map(|h| h.parts.iter().map(|part| (part.index.clone(), part.clone())).collect())
        .unwrap_or_default();
    let mut targets = match &pit {
        Some(held) => held.names(),
        None => store.resolve_open(expr),
    };
    store.refresh_for_search(&targets);
    // the result window is a ceiling on what a caller may page through; a
    // walk this server runs for itself -- a geo aggregation reading every
    // matching document -- is not paging for anyone
    if !crate::search::is_the_server() {
        check_limits(store, &targets, body, p, from, size)?;
    }
    // `ignore_unavailable` says to pass over what cannot be searched rather
    // than to complain about it
    let lenient = p.get("ignore_unavailable").map(|v| v != "false").unwrap_or(false);
    // `expand_wildcards` naming closed indices means a pattern reaches them,
    // and a closed index cannot be searched whichever way it was reached
    let wants_closed = p
        .get("expand_wildcards")
        .map(|v| v.split(',').any(|w| matches!(w.trim(), "closed" | "all")))
        .unwrap_or(false);
    // a closed index is closed cluster-wide: this node may hold no copy of it
    // The published state speaks for the index it was published for: an
    // index deleted and made again under the same name -- a restore over a
    // closed one -- read as closed until the next publish caught up, because
    // the state was looked up by name alone.
    let closed_in_cluster = |name: &str| -> bool {
        store.is_closed(name)
            || crate::cluster::with_state(|s| {
                s.indices
                    .get(name)
                    .map(|m| {
                        m.state == "close"
                            && store
                                .get(name)
                                .map(|st| m.uuid.is_empty() || st.read().uuid == m.uuid)
                                .unwrap_or(true)
                    })
                    .unwrap_or(false)
            })
    };
    if wants_closed && !lenient {
        for name in crate::api::cluster_resolve(store, expr) {
            if closed_in_cluster(&name) {
                return Err(err(
                    StatusCode::BAD_REQUEST,
                    "index_closed_exception",
                    format!("closed index [{name}]"),
                ));
            }
        }
    }
    for name in
        expr.split(',').map(|n| n.trim()).filter(|n| !n.is_empty() && !n.contains('*') && !lenient)
    {
        if closed_in_cluster(name) {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "index_closed_exception",
                format!("closed index [{name}]"),
            ));
        }
    }
    // an index held closed to readers refuses a search, the way one held
    // closed to writers refuses a write. `read_only` is not such a block: it
    // stops changes, and a read-only index is searched as any other -- the
    // reference answers it, and refusing it here refused the one thing a
    // read-only index is kept for.
    for name in &targets {
        let blocked = store
            .get(name)
            .map(|st| st.read().setting("blocks.read").as_deref() == Some("true"))
            .unwrap_or(false);
        if blocked {
            return Err(err(
                StatusCode::FORBIDDEN,
                "cluster_block_exception",
                format!("index [{name}] blocked by: [FORBIDDEN/7/index read (api)];"),
            ));
        }
    }
    // Every name in a list is asked for, not only the first: `present,missing`
    // searched `present` and said nothing about `missing`, so a typo in one
    // index of several was answered as though it had been spelled right. The
    // reference refuses it, naming the index it could not find.
    if !lenient && expr != "_all" {
        for part in expr.split(',').map(|n| n.trim()).filter(|n| !n.is_empty()) {
            if part.contains('*') || part.starts_with('-') || part.contains(':') {
                continue;
            }
            if store.resolve_open(part).is_empty()
                && crate::api::cluster_resolve(store, part).is_empty()
            {
                return Err(no_such_index(&crate::store::resolve_date_math_name(part)));
            }
        }
    }
    if targets.is_empty() && !expr.contains('*') && expr != "_all" && !expr.is_empty() && !lenient {
        // a date-math name is reported as the index it stands for, since that
        // is the one that was not there
        return Err(no_such_index(&crate::store::resolve_date_math_name(expr)));
    }
    // `allow_no_indices=false` makes an expression that reaches nothing an
    // error rather than a search with nothing to search
    if targets.is_empty()
        && !expr.is_empty()
        && p.get("allow_no_indices").map(|v| v == "false").unwrap_or(false)
    {
        return Err(no_such_index(expr));
    }
    // a `terms` lookup names a document to read the term list from
    // A shard whose documents an aggregation cannot take answers with an
    // error rather than with a result, and the search goes on without it.
    // Here the one that fails is the one holding a value the sketch refuses.
    let mut failures: Vec<Value> = Vec::new();
    // shards that were counted as searched and then refused the query, which
    // no shard of the remaining targets can account for
    let mut refused_shards: u64 = 0;
    let mut excluded_ids: Vec<String> = Vec::new();
    if let Some(field) =
        body.get("aggs").or_else(|| body.get("aggregations")).and_then(hdr_percentiles_field)
    {
        let shards = targets
            .iter()
            .filter_map(|n| store.get(n))
            .map(|st| st.read().shard_count())
            .max()
            .unwrap_or(1);
        let probe = json!({"query": {"range": {field.clone(): {"lt": 0}}}, "size": 1});
        let refused = run(store, &targets.join(","), &probe, &Params::new())
            .ok()
            .and_then(|o| o.hits.first().and_then(|h| h.get("_id")?.as_str().map(String::from)));
        if let Some(id) = refused {
            let route = |r: &str| {
                targets
                    .first()
                    .and_then(|n| store.get(n))
                    .map(|st| st.read().shard_for(r))
                    .unwrap_or_else(|| routing_shard(r, shards))
            };
            let bad = route(&id);
            let all = json!({"query": {"match_all": {}}, "size": 10_000, "_source": false});
            if let Ok(o) = run(store, &targets.join(","), &all, &Params::new()) {
                for hit in &o.hits {
                    let Some(other) = hit.get("_id").and_then(|v| v.as_str()) else { continue };
                    if route(other) == bad {
                        excluded_ids.push(other.to_string());
                    }
                }
            }
            failures.push(json!({
                "shard": bad,
                "index": targets.first().cloned().unwrap_or_default(),
                "node": "node-0",
                "reason": {
                    "type": "array_index_out_of_bounds_exception",
                    "reason": "-1",
                },
            }));
        }
    }
    // a request that asks for more buckets than may ever be answered is
    // refused before any of them are built
    check_asked_sizes(store, body.get("aggs").or_else(|| body.get("aggregations")))?;
    let mut extras = Extras::default();
    if let Some(q) = body.get("query") {
        scan_extras(q, &mut extras);
    }
    let extras = extras;
    // a `nested` clause that will be settled against the candidates' own
    // objects is asked here in its widest form, so the settling has something
    // to accept; see `relaxed_for_nested`
    let mut query_json = body.get("query").map(crate::search::extras::relaxed_for_nested);
    if !excluded_ids.is_empty() {
        let base = query_json.take().unwrap_or_else(|| json!({"match_all": {}}));
        query_json = Some(json!({
            "bool": {
                "must": [base],
                "must_not": [{"ids": {"values": excluded_ids.clone()}}],
            }
        }));
    }
    // what a join asked to list is read before the join is rewritten away
    let mut join_inner_hits: Vec<(String, String, Value, Value)> = Vec::new();
    if let Some(q) = query_json.as_mut() {
        resolve_terms_lookups(store, q)?;
        expand_bitmap_terms(q)?;
        expand_more_like_this(store, &targets, q);
        // a joining query walks one set of documents to answer about another,
        // which is one of the costs a cluster may have turned off
        if !expensive_allowed(store) && names_a_join(q) {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "illegal_argument_exception",
                "[joining] queries cannot be executed when 'search.allow_expensive_queries' is \
                 set to false.",
            ));
        }
        // A clause the mapping makes impossible is the shard's complaint and
        // is answered as one: a relation asked of an index with no join
        // field used to come back as an unknown query, because the rewrite
        // that turns a relation into two passes quietly left the clause
        // alone and nothing downstream knew the name.
        let has_join = join_field(store, &targets).is_some();
        // The complaint belongs to the index that raised it, and every target
        // is asked in turn: a `percolate` over several indices is meant for
        // the one holding the queries, and the others have no such field. Only
        // where no target can carry the clause has every shard failed; where
        // one can, the search is answered from it and the rest are reported as
        // the shard failures they are. Asking only the first target failed the
        // whole search over what one index could not do.
        let mut refused: Vec<(String, Value, u64)> = Vec::new();
        for name in &targets {
            let Some(st) = store.get(name) else { continue };
            let g = st.read();
            let Some((reason, behind)) =
                crate::search::extras::mapping_complaint(q, name, &g.mapping, has_join)
            else {
                continue;
            };
            let uuid = g.setting("uuid").unwrap_or_else(|| crate::store::index_uuid(name));
            let cause =
                crate::api::shared::query_shard_cause(name, &uuid, &reason, behind.as_deref());
            refused.push((name.clone(), cause, g.shard_count()));
        }
        if !refused.is_empty() {
            if refused.len() == targets.len() {
                let (index, cause, _) = refused.remove(0);
                return Err(crate::api::shared::all_shards_failed(
                    StatusCode::BAD_REQUEST,
                    &index,
                    cause,
                ));
            }
            for (index, cause, count) in &refused {
                for shard in 0..*count {
                    failures.push(json!({
                        "shard": shard,
                        "index": index,
                        "node": "node-0",
                        "reason": cause,
                    }));
                }
                refused_shards += count;
            }
            targets.retain(|name| !refused.iter().any(|(refused, _, _)| refused == name));
        }
        collect_join_inner_hits(q, &mut join_inner_hits);
        expand_joins(store, &targets, q);
        if names_a_percolate(q) {
            expand_percolate(store, &targets, q)?;
        }
    }

    // a field cannot be both kept and dropped: naming it in both lists asks
    // for two answers about the same field
    if let (Some(inc), Some(exc)) = (
        body.pointer("/_source/includes").and_then(|v| v.as_array()),
        body.pointer("/_source/excludes").and_then(|v| v.as_array()),
    ) && let Some(both) = inc.iter().find(|i| exc.contains(i)).and_then(|v| v.as_str())
    {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!("The same entry [{both}] cannot be both included and excluded in _source."),
        ));
    }

    // `_shard_doc` orders by where a document sits within a shard, which only
    // holds still while a point-in-time is open; without one the order it
    // names does not exist
    if body.get("pit").is_none() {
        let names_shard_doc = |v: &Value| match v {
            Value::String(s) => s == "_shard_doc",
            Value::Object(o) => o.keys().any(|k| k == "_shard_doc"),
            _ => false,
        };
        let asked = match body.get("sort") {
            Some(Value::Array(a)) => a.iter().any(names_shard_doc),
            Some(one) => names_shard_doc(one),
            None => false,
        };
        if asked {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "action_request_validation_exception",
                "Validation Failed: 1: _shard_doc is only supported with point-in-time;",
            ));
        }
    }

    // unsigned_long cannot be sorted alongside another numeric type
    let mut sort_keys = parse_sort(body.get("sort"));
    // `_shard_doc` orders by where a document sits within a shard. That is the
    // order it was written in, which is what `_seq` records -- and it only
    // holds still while a point in time is open, which is why it is refused
    // without one.
    for k in sort_keys.iter_mut() {
        // a join field is sorted by the relation each document stands in,
        // which is what the field's own value is
        let joined = targets
            .iter()
            .filter_map(|n| store.get(n))
            .any(|st| st.read().mapping.type_of(&k.field) == Some("join"));
        if joined {
            k.field = format!("{}.name", k.field);
        }
    }
    // `_doc` is the order the index holds its documents in, and an index that
    // was told to sort itself holds them in that order
    if sort_keys.len() == 1 && sort_keys[0].field == "_doc" {
        let declared = targets.iter().filter_map(|n| store.get(n)).find_map(|st| {
            let g = st.read();
            let fields = g.setting("sort.field")?;
            let orders = g.setting("sort.order").unwrap_or_default();
            let orders: Vec<String> =
                orders.split(',').map(|s| s.trim().trim_matches('"').to_string()).collect();
            let keys: Vec<SortKey> = fields
                .trim_matches(|c| c == '[' || c == ']')
                .split(',')
                .map(|f| f.trim().trim_matches('"').to_string())
                .filter(|f| !f.is_empty())
                .enumerate()
                .map(|(i, field)| SortKey {
                    field,
                    desc: orders.get(i).map(|o| o == "desc").unwrap_or(false),
                    mode: None,
                    missing_last: true,
                    nested: None,
                    nested_filter: None,
                    numeric_type: None,
                    unmapped_type: None,
                    script: None,
                })
                .collect();
            (!keys.is_empty()).then_some(keys)
        });
        if let Some(keys) = declared {
            sort_keys = keys;
        }
    }
    for k in &sort_keys {
        // A field nothing maps is a sort nothing can answer. It used to sort
        // every document as `null`, so the answer came back in no order at
        // all and, worse, `search_after` could not advance: a client paging
        // through with the sort values it was handed asked the same question
        // for ever. `unmapped_type` is the caller saying to treat it as a
        // field with no values, which is the one case where nulls are right.
        let special = matches!(
            k.field.as_str(),
            "_score" | "_doc" | "_seq" | "_script" | "_geo_distance" | "_shard_doc" | "_index"
        ) || k.script.is_some()
            // `_id` can be sorted on unless the cluster said it may not:
            // `indices.id_field_data.enabled` is true by default in the
            // reference, and a sort by id was refused here outright
            || (k.field == "_id"
                && store
                    .cluster_setting("indices.id_field_data.enabled")
                    .map(|v| v != json!(false) && v != json!("false"))
                    .unwrap_or(true));
        if !special && k.unmapped_type.is_none() {
            if k.field == "_id" {
                return Err(err(
                    StatusCode::BAD_REQUEST,
                    "illegal_argument_exception",
                    "Fielddata access on the _id field is disallowed, you can re-enable it by updating the dynamic cluster setting: indices.id_field_data.enabled",
                ));
            }
            // a multi-field is not in the type table under its own name --
            // `users.last.keyword` is a way of reading `users.last` -- so a
            // name whose parent is mapped is a name the index knows
            let parent = k.field.rsplit_once('.').map(|(head, _)| head.to_string());
            let mapped = targets.iter().any(|n| {
                store
                    .get(n)
                    .map(|st| {
                        let g = st.read();
                        let knows = |name: &str| {
                            g.mapping.type_of(name).is_some()
                                || g.all_field_types().iter().any(|(f, _)| f == name)
                        };
                        knows(&k.field) || parent.as_deref().map(knows).unwrap_or(false)
                    })
                    .unwrap_or(false)
            });
            if !mapped && !targets.is_empty() {
                return Err(err(
                    StatusCode::BAD_REQUEST,
                    "query_shard_exception",
                    format!("No mapping found for [{}] in order to sort on", k.field),
                ));
            }
        }
        let mut kinds: Vec<String> = Vec::new();
        for n in &targets {
            if let Some(st) = store.get(n)
                && let Some(t) = st.read().mapping.type_of(&k.field)
                && !kinds.contains(&t.to_string())
            {
                kinds.push(t.to_string());
            }
        }
        if kinds.len() > 1 && kinds.iter().any(|t| t == "unsigned_long") {
            return Err(err_caused_by(
                "search_phase_execution_exception",
                "all shards failed",
                "Can't do sort across indices, as a field has [unsigned_long] type in one index, \
                 and different type in another index!",
            ));
        }
    }
    let AggPlan {
        request: agg_json,
        peeled: filters_aggs,
        siblings: pipeline_aggs,
        inner: bucket_pipelines,
        weighted,
    } = plan_aggs(store, &targets, body)?;

    let OutputSpecs { source: source_sel, fields: field_specs, stored } =
        output_specs(store, &targets, body, p)?;

    // What this search may spend: the deadline it asked for, and -- where it
    // aggregates -- a share of the `request` breaker, held until its answer
    // is written. A node with no share left refuses here, rather than
    // accepting a search it cannot afford and finding out while it runs.
    let budget = Budget::of_search(store, body, p, agg_json.is_some() || !filters_aggs.is_empty())?;

    let started = std::time::Instant::now();
    // a slice divides the index between readers, so the page it can offer is
    // cut from every matching document rather than from the first few
    let slice = body.get("slice").filter(|s| s.get("max").is_some()).cloned();
    // collapsing decides the page from groups rather than from documents, so
    // the best few documents are not enough to cut it from
    // a sort that only counts some of a document's nested objects is settled
    // after the candidates are in hand, so the page cannot be cut while
    // collecting
    let nested_filtered = sort_keys.iter().any(|k| k.nested_filter.is_some());
    // a score that is only settled once the candidates are in hand cannot
    // cut the page while collecting either
    let rescored_later = body.pointer("/query/function_score").is_some()
        || body.pointer("/query/script_score").is_some();
    // a narrowing that happens after the candidates are in hand -- a
    // `post_filter`, a score floor -- decides both the page and the total, so
    // the collection cannot stop at a page's worth
    let narrowed_after = body.get("post_filter").is_some() || body.get("min_score").is_some();
    // a rescore reorders a window wider than the page: keeping only a page's
    // worth of candidates meant `window_size` did nothing at all, and a
    // document the rescore query scores highest could never reach the page
    let rescored = body.get("rescore").is_some();
    let page_want = if slice.is_some()
        || body.get("collapse").is_some()
        || nested_filtered
        || rescored_later
        || rescored
        || narrowed_after
    {
        65_536
    } else {
        from + size
    };
    let mut cands: Vec<Cand> = Vec::new();
    let mut searchers: Vec<(String, Searcher, std::sync::Arc<crate::store::IdxLock>)> = Vec::new();
    let mut total: u64 = 0;
    let mut shards: u64 = refused_shards;
    let mut empty_shards: u64 = 0;
    let agg_acc: Option<IntermediateAggregationResults>;
    let mut agg_req: Option<Aggregations> = None;
    let mut fruits: Vec<IntermediateAggregationResults> = Vec::new();
    let mut shard_profiles: Vec<Value> = Vec::new();
    let mut agg_meta: Vec<(String, Value)> = Vec::new();
    let mut bucket_orders: Vec<(String, String, bool)> = Vec::new();
    // which slice of the term space was asked for is a property of the
    // request, not of any one shard, so it is read once here
    let partitions: Vec<(String, i64, i64, usize)> =
        agg_json.clone().map(|mut a| extract_partitions(&mut a)).unwrap_or_default();

    // `search_after` names where the previous page ended
    let search_after: Option<Vec<SortValue>> = body
        .get("search_after")
        .and_then(|v| v.as_array())
        .filter(|a| a.len() == sort_keys.len() && !a.is_empty())
        // a marker of nulls names no page at all: it is where a caller starts
        .filter(|a| !a.iter().all(|v| v.is_null()))
        .map(|a| {
            a.iter()
                .zip(sort_keys.iter())
                .map(|(v, k)| sort_value_from_json(v, date_sort_kind(store, &targets, &k.field)))
                .collect()
        });
    let fanned_out = targets.len() > 1;
    // what the caller may see of each target, worked out here on the
    // request's own task, before any thread that cannot ask
    // a geo clause is answered by narrowing the whole result, so
    // it may only stand where that means the same thing
    if let Some(q) = body.get("query")
        && let Some(why) = crate::search::extras::placement_complaint(q)
    {
        return Err(err(StatusCode::BAD_REQUEST, "query_shard_exception", why));
    }
    let views = crate::security::view::views_for(store, &targets);
    // A script reads the document's source, all of it: `doc['salary']` in a
    // `script` query, a `_script` sort or a `script_score` answers from a
    // field the caller may not read, and a sort even writes the value into
    // the hit. Field-level rules are applied to what comes back, and a
    // script is a way round them -- so a caller who has any is not allowed to
    // run one here.
    if views.values().any(|v| v.restricts_fields()) && mentions_a_script(body) {
        return Err(err(
            StatusCode::FORBIDDEN,
            "security_exception",
            "a script reads the whole document, and this caller may not read the whole \
             document: scripts are not allowed in a search of an index whose fields are \
             restricted for you",
        ));
    }
    // the aggregations that run as searches of their own read `query_json`
    // rather than the shard's query, so where every target is filtered the
    // same way the filter is folded in here once; each shard folds its own
    // in as well, and a filter laid twice changes nothing
    let query_json: Option<Value> =
        match targets.first().and_then(|t| views.get(t)).and_then(|v| v.dls.clone()) {
            Some(dls)
                if targets
                    .iter()
                    .all(|t| views.get(t).and_then(|v| v.dls.as_ref()) == Some(&dls)) =>
            {
                let base = query_json.unwrap_or_else(|| json!({"match_all": {}}));
                Some(json!({"bool": {"must": [base], "filter": [dls]}}))
            }
            _ => query_json,
        };
    // The filters a request's aliases put on each index are carried in a
    // task-local, and a task-local belongs to the thread that set it: the
    // shards of a search over several indices are searched on other threads,
    // where it was not there. A filtered alias named beside another index lost
    // its filter -- `aliased,other` answered with every document the alias
    // was meant to hide. The map is taken here and set again on each thread.
    let alias_filters = crate::security::layer::ALIAS_FILTERS.try_with(|f| f.clone()).ok();
    let run_shard =
        |shard_idx: usize, name: &String| -> std::result::Result<Option<ShardOut>, Response> {
            let search = || {
                search_one_shard(
                    store,
                    shard_idx,
                    name,
                    body,
                    &query_json,
                    &sort_keys,
                    &search_after,
                    &pit_parts,
                    &agg_json,
                    &filters_aggs,
                    page_want,
                    fanned_out,
                    &views,
                    &budget,
                )
            };
            match &alias_filters {
                Some(f) => crate::security::layer::ALIAS_FILTERS.sync_scope(f.clone(), search),
                None => search(),
            }
        };

    let outs: Vec<std::result::Result<Option<ShardOut>, Response>> = if targets.len() > 1 {
        use rayon::prelude::*;
        targets.par_iter().enumerate().map(|(i, n)| run_shard(i, n)).collect()
    } else {
        targets.iter().enumerate().map(|(i, n)| run_shard(i, n)).collect()
    };

    for out in outs {
        let Some(mut o) = out? else { continue };
        // a candidate names the searcher it came from by slot. The slot a
        // target had in `targets` is not the slot its searcher takes here:
        // a target that answered nothing -- an index deleted between the
        // resolve and the search -- is not pushed, and every later
        // candidate then pointed one place too far along. That was a panic
        // where the list was short, and a document read out of a different
        // index where it was not.
        let slot = searchers.len();
        for c in o.cands.iter_mut() {
            c.shard = slot;
        }
        shards += o.shards;
        total += o.count as u64;
        // a pre-filter skips shards, not indices: an index of two shards
        // that cannot match is two shards the search did not need
        if o.count == 0 {
            empty_shards += o.shards;
        }
        cands.extend(o.cands);
        if let Some(res) = o.agg {
            fruits.push(res);
        }
        if o.agg_req.is_some() {
            agg_req = o.agg_req;
        }
        if agg_meta.is_empty() {
            agg_meta = o.agg_meta;
        }
        if bucket_orders.is_empty() {
            bucket_orders = o.bucket_orders;
        }
        if let Some(pr) = o.profile {
            shard_profiles.push(pr);
        }
        searchers.push((o.name, o.searcher, o.st));
    }
    // A walk that stopped because the node ran out of memory has an answer
    // that is missing documents nobody asked it to leave out. That is a
    // refusal, not a page: the caller is told which breaker stopped it.
    if let Some(refusal) = budget.broken() {
        return Err(refusal);
    }

    // the query phase ends here; what follows reads the page back
    let fetch_started = std::time::Instant::now();

    // A wide fan-out leaves one intermediate result per index to combine.
    // Folding them one after another is linear and single-threaded, which at
    // a couple of hundred indices is a visible share of the whole request; a
    // tree reduction spreads it over the pool the shards already ran on.
    {
        agg_acc = if fruits.len() > 8 {
            use rayon::prelude::*;
            fruits.into_par_iter().reduce_with(|mut a, b| {
                let _ = a.merge_fruits(b);
                a
            })
        } else {
            fruits.into_iter().reduce(|mut a, b| {
                let _ = a.merge_fruits(b);
                a
            })
        };
    }

    // the order documents arrived in settles a tie, so it has to be known
    // before the page is cut rather than after
    fill_seq(&mut cands, &searchers);
    prune(&mut cands, page_want, &sort_keys);
    // `indices_boost` weights whole indices against each other, so it is
    // applied to the scores before they are ranked. An alias may name the
    // index instead of the index naming itself.
    if let Some(boosts) = body.get("indices_boost") {
        apply_indices_boost(store, &mut cands, &searchers, boosts, p)?;
    }

    // a geo shape or a distance_feature is settled from the candidates' own
    // values, and what survives is the new total
    if extras.geo || extras.distance_feature || extras.nested_query {
        let before = cands.len();
        settle_by_value(&mut cands, &searchers, body, &extras);
        if cands.len() != before {
            total = cands.len() as u64;
        }
    }

    // `rescore` runs a second query over the top of the page and mixes its
    // score into the one already there
    let rescored = apply_rescores(store, &targets, &mut cands, &searchers, body, &sort_keys)?;
    // Where a sort names a filter on the nested objects it reads, only the
    // objects that match it have anything to say. A document whose objects all
    // fail the filter has no value at all, and sorts with the missing ones.
    if nested_filtered {
        sort_by_filtered_nested(store, &targets, &mut cands, &searchers, &sort_keys);
    }
    // a `_script` sort reads each candidate through its script
    if sort_keys.iter().any(|k| k.script.is_some()) {
        crate::search::sort_by_script(&mut cands, &searchers, &sort_keys)?;
    }
    // `function_score` says what a document's score should be, given what the
    // query scored it and what the document itself holds
    if let Some(spec) = body.pointer("/query/function_score") {
        rescore_by_functions(&searchers, &mut cands, spec)?;
    }
    // `boosting` keeps what `positive` finds and lowers, by `negative_boost`,
    // the score of whatever `negative` also matches. The query was answered
    // as `positive` alone, so the documents it was written to push down came
    // back in the same places.
    if let Some(spec) = body.pointer("/query/boosting")
        && let Some(negative) = spec.get("negative")
    {
        let factor = spec.get("negative_boost").and_then(|v| v.as_f64()).unwrap_or(1.0) as f32;
        let lowered: std::collections::HashSet<String> =
            crate::search::lookup::matching_ids_here(store, &targets, negative)
                .into_iter()
                .collect();
        if !lowered.is_empty() {
            for c in cands.iter_mut() {
                let (_, searcher, st) = &searchers[c.shard];
                let g = st.read();
                if let Some((id, _)) = source_of(searcher, &g, c.addr)
                    && lowered.contains(&id)
                {
                    c.score *= factor;
                }
            }
        }
    }
    // `script_score` hands each candidate's score to a script and keeps what
    // it returns; a `min_score` drops those the script rated too low
    if let Some(spec) = body.pointer("/query/script_score") {
        let before = cands.len();
        rescore_by_script(&searchers, &mut cands, spec)?;
        if cands.len() != before {
            total = cands.len() as u64;
        }
    }
    // a rank feature scores by the value of a field, curved the way the query
    // asks for
    if let Some(query) = body.get("query") {
        let mut features = Vec::new();
        collect_rank_features(query, &mut features);
        if !features.is_empty() {
            rescore_by_rank_features(&searchers, &mut cands, &features);
        }
    }

    cands.sort_by(|a, b| cmp_cands(a, b, &sort_keys));

    // a score is only the best score when the ranking is by score descending;
    // any other order makes the top hit's score arbitrary
    let ranked_by_score = sort_keys.is_empty()
        || sort_keys.first().map(|k| k.field == "_score" && k.desc).unwrap_or(false);
    let max_score = if ranked_by_score {
        cands.iter().map(|c| c.score).fold(None::<f32>, |acc, s| Some(acc.map_or(s, |a| a.max(s))))
    } else {
        None
    };

    // A slice takes the shards whose number falls to it. Which shard a
    // document belongs to follows from its id, so the split holds however the
    // documents were spread -- and every slice together covers all of them.
    if let Some(slice) = slice.as_ref() {
        let id = slice.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
        let max = slice.get("max").and_then(|v| v.as_u64()).unwrap_or(1).max(1);
        cands.retain(|c| {
            let (_, searcher, st) = &searchers[c.shard];
            let g = st.read();
            match source_of(searcher, &g, c.addr) {
                Some((doc_id, _)) => {
                    // placed by the index's own fold, as its writes were
                    let routed = g.shard_for(&doc_id);
                    routed % max == id
                }
                None => false,
            }
        });
        total = cands.len() as u64;
    }

    // `min_score` is the score a document has to reach to be an answer at
    // all: one below it is not a hit, and is not counted as one
    if let Some(floor) = body.get("min_score").and_then(|v| v.as_f64()) {
        cands.retain(|c| c.score as f64 >= floor);
        total = cands.len() as u64;
    }

    // `post_filter` narrows what comes back without narrowing what the
    // aggregations saw, which is the whole point of asking for it
    if let Some(spec) = body.get("post_filter") {
        // The filter is run over each searcher and every document it matches
        // is collected, so what is kept is what really matches. It used to be
        // a search of its own for the top ten thousand by *score*, and the
        // page kept only the ids in that: past ten thousand matches, hits
        // that do match were dropped and `hits.total` was wrong -- badly so
        // when the page was sorted by something other than score.
        let mut keep: Vec<std::collections::HashSet<velocore::DocAddress>> = Vec::new();
        for (_, searcher, st) in searchers.iter() {
            let g = st.read();
            let ctx = crate::query::Ctx {
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
            let q = crate::query::build(&ctx, spec).map_err(|e| {
                err(StatusCode::BAD_REQUEST, "query_shard_exception", e.to_string())
            })?;
            // how many it matches is asked before the set of them is built:
            // collecting first and refusing afterwards is refusing after the
            // memory has already been taken
            let how_many = searcher.search(&q, &velocore::collector::Count).map_err(|e| {
                err(StatusCode::INTERNAL_SERVER_ERROR, "search_exception", e.to_string())
            })?;
            if how_many > MOST_POST_FILTERED {
                return Err(err(
                    StatusCode::BAD_REQUEST,
                    "too_many_buckets_exception",
                    format!(
                        "a post_filter is answered by looking at every document it matches, and \
                         this one matches [{how_many}], more than [{MOST_POST_FILTERED}]. Put \
                         the narrowing in the query, or narrow the query first."
                    ),
                ));
            }
            // a search that failed is not a search that matched nothing
            let found =
                searcher.search(&q, &velocore::collector::DocSetCollector).map_err(|e| {
                    err(StatusCode::INTERNAL_SERVER_ERROR, "search_exception", e.to_string())
                })?;
            keep.push(found);
        }
        cands.retain(|c| keep.get(c.shard).map(|k| k.contains(&c.addr)).unwrap_or(false));
        total = cands.len() as u64;
    }

    // `collapse` keeps one hit per distinct value of a field: the best one,
    // which after the sort is the first each value is seen at. It has to run
    // before the page is cut, or a page could be all one value's worth
    if let Some(field) = body.pointer("/collapse/field").and_then(|v| v.as_str()) {
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        cands.retain(|c| {
            let (_, searcher, st) = &searchers[c.shard];
            let g = st.read();
            // a field declared as an alias is another name for one that is
            // really in the document
            let real = g.mapping.target_of(field).unwrap_or(field);
            let path = format!("/{}", real.replace('.', "/"));
            let value = source_of(searcher, &g, c.addr)
                .and_then(|(_, src)| src.pointer(&path).cloned())
                .map(|v| match v {
                    Value::String(s) => s,
                    other => other.to_string(),
                });
            match value {
                Some(v) => seen.insert(v),
                // a document with no value there collapses with nothing
                None => true,
            }
        });
    }

    // now, and only now, read stored fields -- for at most `size` documents
    let setting_up = std::time::Instant::now();
    let track_scores = !sort_keys.is_empty()
        && body.get("track_scores").and_then(|v| v.as_bool()).unwrap_or(false);
    let mut all_hits: Vec<Hit> = Vec::new();
    // a profile asks what each part of reading the hits back cost, and on
    // which shard; the loop reads them the same way either way
    let profiling_fetch = !shard_profiles.is_empty();
    let mut readers_seen: std::collections::HashSet<(usize, u64)> = Default::default();
    let fetch_setup = setting_up.elapsed().as_nanos() as u64;
    for c in cands.into_iter().skip(from).take(size) {
        let (name, searcher, st) = &searchers[c.shard];
        let reading = std::time::Instant::now();
        let g = st.read();
        let got_reader = reading.elapsed().as_nanos() as u64;
        let found = match profiling_fetch {
            false => source_of(searcher, &g, c.addr),
            true => {
                let t = std::time::Instant::now();
                let doc: Option<TantivyDocument> = searcher.doc(c.addr).ok();
                let stored = t.elapsed().as_nanos() as u64;
                let t = std::time::Instant::now();
                let found = doc.and_then(|doc| {
                    let id = doc.get_first(g.fields.id)?.as_str()?.to_string();
                    let src =
                        serde_json::from_str(doc.get_first(g.fields.source)?.as_str()?).ok()?;
                    Some((id, src))
                });
                let parsed = t.elapsed().as_nanos() as u64;
                if let Some((id, _)) = &found {
                    let shard = g.shard_of_doc(id);
                    note_fetch_part(&mut shard_profiles, name, shard, "load_stored_fields", stored);
                    note_fetch_part(&mut shard_profiles, name, shard, "load_source", parsed);
                    // the reader, the visitor and the sub-phases are made
                    // once for each shard the page reads from
                    if readers_seen.insert((c.shard, shard)) {
                        let t = std::time::Instant::now();
                        let _ = searcher.segment_reader(c.addr.segment_ord).max_doc();
                        let visitor = t.elapsed().as_nanos() as u64;
                        for (part, nanos) in [
                            ("get_next_reader", got_reader),
                            ("create_stored_fields_visitor", visitor),
                            ("build_sub_phase_processors", fetch_setup),
                        ] {
                            note_fetch_part(&mut shard_profiles, name, shard, part, nanos);
                        }
                    }
                }
                found
            }
        };
        let Some((id, mut src)) = found else { continue };
        // `_ignored` travels inside the stored source but belongs on the hit
        let ignored = src.as_object_mut().and_then(|o| o.remove("_ignored"));
        let version = g.version_of(&id);
        // a sort collects without scoring; `track_scores` asks for the score
        // as well as the order, and it is worked out for the page alone
        let score = match track_scores {
            true => body
                .get("query")
                .and_then(|q| crate::search::explain::explain_document(&g, q, &id))
                .and_then(|e| e.get("value").and_then(|v| v.as_f64()))
                .map(|v| v as f32)
                .unwrap_or(c.score),
            false => c.score,
        };
        all_hits.push(Hit {
            seq: c.seq,
            shard_idx: c.shard,
            index: name.clone(),
            id,
            score,
            source: src,
            sort: c.sort,
            version,
            ignored,
        });
    }

    // Highlighting a long field means analysing it. Where the index caps how
    // much may be analysed, a field that exceeds the cap is refused rather
    // than silently truncated -- unless the request says how much to analyse,
    // or the field stores offsets and the highlighter can use them.
    if let Some(spec) = body.get("highlight") {
        // A highlighter that cannot read a field refuses the whole request,
        // as upstream does while it sets the highlighter up: `fvh` on a field
        // without term vectors used to be answered as if it were `unified`.
        if let Some(h) = all_hits.first() {
            let g = searchers[h.shard_idx].2.read();
            if let Some(reason) = crate::search::highlight::highlight_refusal(spec, &g.mapping) {
                return Err(search_shard_failure("illegal_argument_exception", &reason, &h.index));
            }
        }
        for h in &all_hits {
            let g = searchers[h.shard_idx].2.read();
            let Some(cap) =
                g.setting("highlight.max_analyzed_offset").and_then(|v| v.parse::<usize>().ok())
            else {
                break;
            };
            let plain = spec.get("type").and_then(|t| t.as_str()) == Some("plain");
            let Some(fields) = spec.get("fields").and_then(|f| f.as_object()) else { break };
            for (name, opts) in fields {
                if opts.get("max_analyzer_offset").is_some() {
                    continue;
                }
                let has_offsets = g
                    .mapping
                    .field_option(name, "index_options")
                    .and_then(|v| v.as_str().map(|s| s == "offsets"))
                    .unwrap_or(false)
                    || g.mapping.field_option(name, "term_vector").is_some();
                if has_offsets && !plain {
                    continue;
                }
                let too_long = h
                    .source
                    .pointer(&format!("/{}", name.replace('.', "/")))
                    .and_then(|v| v.as_str())
                    .map(|t| t.len() > cap)
                    .unwrap_or(false);
                if too_long {
                    return Err(err(
                        StatusCode::BAD_REQUEST,
                        "illegal_argument_exception",
                        format!(
                            "The length of [{name}] field of [{}] doc of [{}] index has exceeded \
                             [{cap}] - maximum allowed to be analyzed for highlighting.",
                            h.id, h.index
                        ),
                    ));
                }
            }
        }
    }

    let suggest = match body.get("suggest") {
        Some(spec) => {
            let typed = p.get("typed_keys").map(|v| v != "false").unwrap_or(false);
            // a suggester reads the term dictionary and the stored values
            // themselves, not the documents the query matched, so a caller
            // whose view narrows either would be offered words out of
            // documents they may not read. Until the suggesters can be
            // narrowed, a narrowed caller is offered nothing.
            let restricted = targets
                .iter()
                .any(|t| views.get(t).map(|v| v.dls.is_some() || v.restricts()).unwrap_or(false));
            match restricted {
                true => Some(json!({})),
                false => Some(build_suggest(store, &targets, spec, typed)?),
            }
        }
        None => None,
    };

    // a clause given a name says so on every hit it matched
    let page_ids: Vec<String> = all_hits.iter().map(|h| h.id.clone()).collect();
    let named = if extras.named {
        matched_names(store, &targets, body, &page_ids)
    } else {
        std::collections::HashMap::new()
    };
    let named_scores = p.get("include_named_queries_score").map(|v| v != "false").unwrap_or(false);
    let mut script_error = None;
    // the order each hit's write arrived in: what a coordinator merging
    // pages from several nodes breaks ties by
    let seqs: Vec<u64> = all_hits.iter().map(|h| h.seq).collect();
    let dressing = std::time::Instant::now();
    let page = write_page(
        store,
        &targets,
        &searchers,
        all_hits,
        body,
        p,
        &query_json,
        &sort_keys,
        &source_sel,
        &stored,
        &field_specs,
        &named,
        named_scores,
        rescored,
        &extras,
        &mut script_error,
    );
    // dressing the page is the fetch's sub-phases, for a profile
    let dressed = dressing.elapsed().as_nanos() as u64;
    for profile in shard_profiles.iter_mut() {
        profile["_dressing"] = json!(dressed);
    }
    if let Some(failed) = script_error {
        return Err(failed);
    }
    let mut page = page;
    // reads of watched fields are written down before anything is narrowed
    if crate::security::audit_reads_watched(store) {
        for hit in page.iter() {
            let idx = hit.get("_index").and_then(|i| i.as_str()).unwrap_or("");
            let id = hit.get("_id").and_then(|i| i.as_str()).unwrap_or("");
            if let Some(src) = hit.get("_source") {
                crate::security::audit_document_read(idx, id, src);
            }
        }
    }
    // a document-level filter is a real query: nothing ends early under it
    let dls_applied =
        views.values().any(|v| v.dls.is_some()) && body.get("terminate_after").is_none();
    if !views.is_empty() {
        for hit in page.iter_mut() {
            let idx = hit.get("_index").and_then(|i| i.as_str()).unwrap_or("").to_string();
            if let Some(view) = views.get(&idx) {
                view.filter_hit(hit);
                // a sort by a hidden field has no values; one by a masked
                // field orders by the hash
                if let Some(Value::Array(sv)) = hit.get_mut("sort") {
                    for (i, k) in sort_keys.iter().enumerate() {
                        if i < sv.len() && view.hidden(&k.field) {
                            sv[i] = Value::Null;
                        } else if i < sv.len() && view.masked(&k.field) {
                            sv[i] = view.mask(&sv[i]);
                        }
                    }
                }
            }
        }
        if let Some(view) = views.values().next()
            && sort_keys.iter().any(|k| view.masked(&k.field))
        {
            let desc: Vec<bool> = sort_keys.iter().map(|k| k.desc).collect();
            page.sort_by(|a, b| {
                let sa = a.get("sort").and_then(|v| v.as_array());
                let sb = b.get("sort").and_then(|v| v.as_array());
                for (i, d) in desc.iter().enumerate() {
                    let x = sa.and_then(|v| v.get(i)).map(|v| v.to_string()).unwrap_or_default();
                    let y = sb.and_then(|v| v.get(i)).map(|v| v.to_string()).unwrap_or_default();
                    let c = if *d { y.cmp(&x) } else { x.cmp(&y) };
                    if c != std::cmp::Ordering::Equal {
                        return c;
                    }
                }
                std::cmp::Ordering::Equal
            });
        }
    }

    // `stored_fields: _none_` asks for hits with no identity on them. The
    // identity is taken off here, once the security pass above has used it:
    // a hit built without an `_index` matched no caller's view, and every
    // hidden field and masked value came back in the clear.
    if asked_for_none(body, &stored) {
        for hit in page.iter_mut() {
            if let Some(o) = hit.as_object_mut() {
                o.remove("_index");
                o.remove("_id");
                o.remove("_routing");
            }
        }
    }

    // see `suggest_only`: the words are the whole answer
    let (total, max_score) = if suggest_only { (0, None) } else { (total, max_score) };

    // a node answering a coordinator stops here: the page, with the order
    // each hit's write arrived in, and the aggregations still intermediate
    if p.contains_key("_native_only") {
        let mut page = page;
        for (hit, seq) in page.iter_mut().zip(seqs.iter()) {
            hit["_seq"] = json!(seq);
        }
        let agg_bytes = agg_acc.as_ref().and_then(|a| postcard::to_allocvec(a).ok());
        let agg_req_json = agg_req.as_ref().and_then(|r| serde_json::to_value(r).ok());
        note_fetch(store, &targets, body, fetch_started, total);
        return Ok(Outcome {
            took_ms: started.elapsed().as_millis() as u64,
            skipped: 0,
            // a search that reached no index searched no shard: a pattern
            // matching nothing reported one that did not exist
            shards: if targets.is_empty() { 0 } else { shards.max(1) },
            total,
            hits: page,
            max_score,
            aggs: None,
            profile: (!shard_profiles.is_empty()).then(|| json!({"shards": shard_profiles})),
            suggest,
            failures,
            filtered: dls_applied,
            timed_out: budget.timed_out(),
            native: Some(NativeParts {
                agg_acc: agg_bytes,
                agg_req: agg_req_json,
                agg_meta: agg_meta.clone(),
                bucket_orders: bucket_orders.clone(),
                weighted,
                empty_shards,
            }),
        });
    }
    finish_search(
        store,
        &targets,
        body,
        p,
        Finish {
            started,
            budget,
            timed_out: false,
            page,
            total,
            max_score,
            shards,
            empty_shards,
            failures,
            suggest,
            agg_acc,
            agg_req,
            agg_json,
            bucket_orders,
            partitions,
            agg_meta,
            weighted,
            filters_aggs,
            bucket_pipelines,
            pipeline_aggs,
            shard_profiles,
            query_json,
            views,
            dls_applied,
            extras,
            named,
            size,
            join_inner_hits,
        },
    )
}

/// What the tail of a search needs from the gathering: the page and the
/// numbers, and the aggregations still intermediate.
pub(crate) struct Finish {
    pub(crate) started: std::time::Instant,
    /// what the search was given to spend, held until its answer is written
    pub(crate) budget: Budget,
    /// a node that answered this coordinator ran out of time; the answer says
    /// so however this node's own clock went
    pub(crate) timed_out: bool,
    pub(crate) page: Vec<Value>,
    pub(crate) total: u64,
    pub(crate) max_score: Option<f32>,
    pub(crate) shards: u64,
    pub(crate) empty_shards: u64,
    pub(crate) failures: Vec<Value>,
    pub(crate) suggest: Option<Value>,
    pub(crate) agg_acc: Option<IntermediateAggregationResults>,
    pub(crate) agg_req: Option<Aggregations>,
    pub(crate) agg_json: Option<Value>,
    pub(crate) bucket_orders: Vec<(String, String, bool)>,
    pub(crate) partitions: Vec<(String, i64, i64, usize)>,
    pub(crate) agg_meta: Vec<(String, Value)>,
    pub(crate) weighted: bool,
    pub(crate) filters_aggs: Vec<(String, Value)>,
    pub(crate) bucket_pipelines: Vec<(Vec<String>, String, Value)>,
    pub(crate) pipeline_aggs: Vec<(String, Value)>,
    pub(crate) shard_profiles: Vec<Value>,
    pub(crate) query_json: Option<Value>,
    pub(crate) views: crate::security::view::Views,
    pub(crate) dls_applied: bool,
    pub(crate) extras: Extras,
    pub(crate) named: std::collections::HashMap<String, Vec<(String, f32)>>,
    pub(crate) size: usize,
    pub(crate) join_inner_hits: Vec<(String, String, Value, Value)>,
}

/// The tail of a search: the engine's own aggregations, the aggregations
/// rendered, pipelines, the profile and the answer. A coordinator that
/// merged pages and intermediates from several nodes ends here too.
pub(crate) fn finish_search(
    store: &Store,
    targets: &[String],
    body: &Value,
    p: &Params,
    f: Finish,
) -> std::result::Result<Outcome, Response> {
    let fetch_started = std::time::Instant::now();
    let Finish {
        started,
        budget,
        timed_out,
        page,
        total,
        max_score,
        shards,
        empty_shards,
        failures,
        suggest,
        agg_acc,
        agg_req,
        agg_json,
        bucket_orders,
        partitions,
        agg_meta,
        weighted,
        filters_aggs,
        bucket_pipelines,
        pipeline_aggs,
        mut shard_profiles,
        query_json,
        views,
        dls_applied,
        extras,
        named,
        size,
        join_inner_hits,
    } = f;
    let targets: Vec<String> = targets.to_vec();
    let profiling = p.get("profile").map(|v| v == "true").unwrap_or(false)
        || body.get("profile").and_then(|v| v.as_bool()).unwrap_or(false);
    // a profile reports each of the engine's own aggregations with its own
    // time, so they are worked out one at a time and each is timed
    let (filters_results, peeled_nanos) = match profiling {
        false => {
            (run_peeled_aggs(store, &targets, &query_json, &filters_aggs, weighted)?, Vec::new())
        }
        true => {
            let (mut results, mut nanos) = (Vec::new(), Vec::new());
            for one in &filters_aggs {
                let t = std::time::Instant::now();
                let answered = run_peeled_aggs(
                    store,
                    &targets,
                    &query_json,
                    std::slice::from_ref(one),
                    weighted,
                )?;
                nanos.push((one.0.clone(), t.elapsed().as_nanos() as u64));
                results.extend(answered);
            }
            (results, nanos)
        }
    };

    let aggs = finalise_aggs(
        store,
        &targets,
        agg_acc,
        agg_req,
        &agg_json,
        &bucket_orders,
        &partitions,
        &agg_meta,
        weighted,
    )?;

    let aggs = if filters_results.is_empty() {
        aggs
    } else {
        let mut base = aggs.unwrap_or_else(|| json!({}));
        for (name, v) in &filters_results {
            base[name.clone()] = v.clone();
        }
        Some(base)
    };

    if profiling {
        own_agg_profiles(
            &filters_aggs,
            &filters_results,
            &peeled_nanos,
            total,
            &query_json,
            &mut shard_profiles,
            store,
            &targets,
        );
    }

    // the profile is written while the aggregation runs, before there are any
    // buckets to count, so the count is filled in from the finished answer
    if let (Some(a), false) = (aggs.as_ref(), shard_profiles.is_empty()) {
        for shard in shard_profiles.iter_mut() {
            let Some(entries) = shard.get_mut("aggregations").and_then(|e| e.as_array_mut()) else {
                continue;
            };
            for entry in entries.iter_mut() {
                let Some(name) = entry.get("description").and_then(|d| d.as_str()) else {
                    continue;
                };
                // a bucket that had to be filled in to close a gap was never
                // built while collecting, so it is not one of the buckets the
                // aggregation counts
                let n = a
                    .get(name)
                    .and_then(|v| v.get("buckets"))
                    .and_then(|b| b.as_array())
                    .map(|b| {
                        b.iter()
                            .filter(|x| {
                                x.get("doc_count").and_then(|c| c.as_u64()).unwrap_or(1) > 0
                            })
                            .count()
                    })
                    .unwrap_or(0);
                if let Some(debug) = entry.get_mut("debug").and_then(|d| d.as_object_mut()) {
                    debug.insert("total_buckets".into(), json!(n));
                }
            }
        }
    }

    let aggs = match aggs {
        Some(mut base) if !bucket_pipelines.is_empty() => {
            for (path, name, def) in &bucket_pipelines {
                apply_bucket_pipeline(&mut base, path, name, def);
            }
            Some(base)
        }
        other => other,
    };
    let mut aggs = if pipeline_aggs.is_empty() {
        aggs
    } else {
        let mut base = aggs.unwrap_or_else(|| json!({}));
        for (name, def) in pipeline_aggs {
            base[name] = run_pipeline_agg(&base, &def)?;
        }
        Some(base)
    };

    if let Some(a) = aggs.as_mut() {
        millis_in_keys(a);
    }

    if let (Some(a), Some(req)) =
        (aggs.as_mut(), body.get("aggs").or_else(|| body.get("aggregations")))
    {
        keep_asked_ranges(req, a);
        whole_metric_values(a, req);
    }

    if let (Some(a), Some(req)) =
        (aggs.as_mut(), body.get("aggs").or_else(|| body.get("aggregations")))
    {
        name_date_metrics(store, &targets, req, a);
    }

    // `search.max_buckets` caps how many buckets one request may build. The
    // limit is counted over the whole answer, sub-buckets included, which is
    // what makes a nested terms aggregation the expensive one.
    // masked keys hashed, hidden hits narrowed, inside every view
    let mut aggs = aggs;
    if !views.is_empty()
        && let (Some(a), Some(req)) =
            (aggs.as_mut(), body.get("aggs").or_else(|| body.get("aggregations")))
    {
        for view in views.values() {
            view.post_aggs(req, a);
        }
    }
    check_max_buckets(store, &aggs)?;
    if let (Some(a), Some(req)) =
        (aggs.as_mut(), body.get("aggs").or_else(|| body.get("aggregations")))
    {
        if !weighted {
            let base = query_json.clone().unwrap_or_else(|| json!({"match_all": {}}));
            shard_terms_bounds(store, &targets, &base, req, a)?;
        }
        order_as_requested(a, req);
    }

    let agg_forces_all = body
        .get("aggs")
        .or_else(|| body.get("aggregations"))
        .map(needs_all_shards)
        .unwrap_or(false);

    let skipped =
        if p.contains_key("pre_filter_shard_size") && query_json.is_some() && !agg_forces_all {
            // one shard is always searched, so that there is an answer to give
            empty_shards.min(shards.max(1) - 1)
        } else {
            0
        };

    // `typed_keys` asks for every aggregation and suggestion to be named after
    // what it produced as well as what it was called
    let (aggs, suggest) = if p.get("typed_keys").map(|v| v != "false").unwrap_or(false) {
        let mut aggs = aggs;
        if let (Some(a), Some(req)) =
            (aggs.as_mut(), body.get("aggs").or_else(|| body.get("aggregations")))
        {
            apply_typed_keys(store, &targets, a, req);
        }
        let mut suggest = suggest;
        if let (Some(sg), Some(req)) = (suggest.as_mut(), body.get("suggest")) {
            apply_typed_keys_suggest(sg, req);
        }
        (aggs, suggest)
    } else {
        (aggs, suggest)
    };

    // `profile` also asks what the fetch cost: reading each hit back, and the
    // sub-phases that filled it in
    if !shard_profiles.is_empty() {
        let dressed = shard_profiles
            .iter()
            .filter_map(|s| s.get("_dressing").and_then(|v| v.as_u64()))
            .max()
            .unwrap_or(0);
        let fetched = if size == 0 { 0 } else { page.len() as u64 };
        fetch_profiles(&mut shard_profiles, body, &extras, &named, dressed, fetched, &peeled_nanos);
        for profile in shard_profiles.iter_mut() {
            if let Some(o) = profile.as_object_mut() {
                o.shift_remove("_dressing");
            }
        }
    }

    let mut page = page;
    if !join_inner_hits.is_empty() {
        attach_join_inner_hits(store, &targets, &mut page, &join_inner_hits);
    }
    if body.get("query").is_some_and(names_a_percolate) {
        attach_percolate_slots(store, &targets, body, &mut page);
    }
    note_fetch(store, &targets, body, fetch_started, total);
    Ok(Outcome {
        took_ms: started.elapsed().as_millis() as u64,
        skipped,
        // a search that reached no index searched no shard
        shards: if targets.is_empty() { 0 } else { shards.max(1) },
        total,
        hits: page,
        // A search that asked for no hits has no best hit to report. The
        // reference answers `null` there whatever the query scored, and this
        // reported the best score it had found while returning nothing to
        // attach it to -- visible wherever a query and `size: 0` met, which
        // is most of the way an aggregation is asked for.
        max_score: if size == 0 { None } else { max_score },
        aggs,
        profile: (!shard_profiles.is_empty())
            .then(|| json!({"shards": split_by_shard(shard_profiles)})),
        suggest,
        failures,
        filtered: dls_applied,
        timed_out: timed_out || budget.timed_out(),
        native: None,
    })
}

/// The fetch phase of a search -- reading back and filling in the page --
/// counted for each index searched and for the groups the search named, and
/// written to the fetch slow log where it took long enough. A page drawn from
/// several indices is one fetch here, and each of them is counted as having
/// done it, as each shard of the reference runs a fetch of its own.
fn note_fetch(
    store: &Store,
    targets: &[String],
    body: &Value,
    started: std::time::Instant,
    total: u64,
) {
    let took = started.elapsed().as_nanos() as u64;
    let groups = crate::search::shard::stats_groups(body);
    for name in targets {
        let Some(st) = store.get(name) else { continue };
        let g = st.read();
        g.counters.search.fetch.add(took);
        for group in &groups {
            g.counters.group(group).fetch.add(took);
        }
        if !g.knobs.slowlog.fetch.is_off() {
            crate::store::slowlog::search(
                &g.knobs.slowlog.fetch,
                "fetch",
                name,
                took,
                total,
                &groups,
                g.shard_count(),
                body,
            );
        }
    }
}

/// Run a search whose body defines derived fields of its own.
fn run_with_derived(
    store: &Store,
    expr: &str,
    body: &Value,
    p: &Params,
    defs: &serde_json::Map<String, Value>,
) -> std::result::Result<Outcome, Response> {
    let targets = store.resolve(expr);
    let Some(first) = targets.first().and_then(|n| store.get(n)) else {
        return run(store, expr, &without_derived(body), p);
    };
    let scratch = Store::scratch();
    let Ok(st) = scratch.ensure("_derived") else {
        return run(store, expr, &without_derived(body), p);
    };
    {
        let mut raw = first.read().mapping.raw.clone();
        let slot = raw
            .as_object_mut()
            .map(|o| o.entry("derived".to_string()).or_insert_with(|| json!({})));
        if let Some(existing) = slot.and_then(|d| d.as_object_mut()) {
            for (k, v) in defs {
                existing.insert(k.clone(), v.clone());
            }
        }
        let settings = first.read().settings.clone();
        let mut g = st.write();
        g.mapping = crate::store::Mapping::from_body(&raw);
        g.settings = settings;
        g.apply_analysis();
    }
    // every document of the index, written into the scratch one as it was
    let all = json!({"query": {"match_all": {}}, "size": 10_000});
    // a copy the server makes for itself is not a page a caller asked for,
    // so the result window does not bound it
    let found = crate::search::as_the_server(|| run(store, expr, &all, &Params::new()))?;
    {
        let mut g = st.write();
        for hit in &found.hits {
            let Some(id) = hit.get("_id").and_then(|v| v.as_str()) else { continue };
            let source = hit.get("_source").cloned().unwrap_or(json!({}));
            let _ = crate::api::write_doc_raw(&mut g, id, source, "index", None);
        }
        let _ = g.refresh();
    }
    let mut out = run(&scratch, "_derived", &without_derived(body), p)?;
    let name = targets.first().cloned().unwrap_or_else(|| expr.to_string());
    for hit in out.hits.iter_mut() {
        hit["_index"] = json!(name);
    }
    Ok(out)
}

fn without_derived(body: &Value) -> Value {
    let mut b = body.clone();
    if let Some(o) = b.as_object_mut() {
        o.remove("derived");
    }
    b
}

/// A decay function's value for one document: 1 at the origin, `decay` at
/// `scale` beyond `offset`, and in between by the curve its name gives.
/// Numbers, dates -- origin and scale as a date and a length of time -- and
/// geo points -- a point and a distance -- are all distances from an origin.
fn decay_value(shape: &str, spec: &Value, source: &Value) -> Option<f32> {
    let (field, params) =
        spec.as_object()?.iter().find(|(k, _)| String::as_str(k) != "multi_value_mode")?;
    let held = source.pointer(&format!("/{}", field.replace('.', "/")))?;
    let decay = params.get("decay").and_then(|v| v.as_f64()).unwrap_or(0.5);
    let length = |v: Option<&Value>, scale_of: &dyn Fn(&str) -> Option<f64>| -> Option<f64> {
        match v? {
            Value::Number(n) => n.as_f64(),
            Value::String(s) => scale_of(s),
            _ => None,
        }
    };
    let metres = |s: &str| -> Option<f64> {
        let s = s.trim();
        let split = s.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(s.len());
        let (n, unit) = s.split_at(split);
        let n: f64 = n.trim().parse().ok()?;
        Some(
            n * match unit {
                "km" => 1000.0,
                "mi" => 1609.344,
                "yd" => 0.9144,
                "ft" => 0.3048,
                "cm" => 0.01,
                "mm" => 0.001,
                "nmi" | "NM" => 1852.0,
                _ => 1.0,
            },
        )
    };
    let millis = |s: &str| crate::search::extras::parse_time_amount(s);
    // how far the value is from the origin, and the scale, in one unit
    let (distance, scale, offset) = if let Some(origin) = params
        .get("origin")
        .and_then(crate::search::geo::read_point)
        .filter(|_| crate::search::geo::read_point(held).is_some())
    {
        let (lat, lon) = crate::search::geo::read_point(held)?;
        let (olat, olon) = origin;
        const R: f64 = 6_371_008.771_4;
        let (p1, p2) = (olat.to_radians(), lat.to_radians());
        let (dp, dl) = ((lat - olat).to_radians(), (lon - olon).to_radians());
        let h = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
        let d = 2.0 * R * h.sqrt().min(1.0).asin();
        (
            d,
            length(params.get("scale"), &metres)?,
            length(params.get("offset"), &metres).unwrap_or(0.0),
        )
    } else if let Some(v) = held.as_f64() {
        let origin = params.get("origin").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let scale = params.get("scale").and_then(|v| v.as_f64())?;
        let offset = params.get("offset").and_then(|v| v.as_f64()).unwrap_or(0.0);
        ((v - origin).abs(), scale, offset)
    } else {
        // a date: both ends read as instants, the lengths as time
        let at = |v: &Value| {
            crate::store::canonical_date(v)
                .and_then(|d| crate::store::parse_date_lenient(&d))
                .map(|d| d.unix_timestamp_nanos() as f64 / 1e6)
        };
        let value = at(held)?;
        let origin = match params.get("origin") {
            Some(o) => at(o)?,
            None => {
                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_millis()
                    as f64
            }
        };
        let scale = length(params.get("scale"), &millis)?;
        let offset = length(params.get("offset"), &millis).unwrap_or(0.0);
        ((value - origin).abs(), scale, offset)
    };
    let d = (distance - offset).max(0.0);
    if scale <= 0.0 {
        return None;
    }
    let value = match shape {
        "gauss" => {
            let sigma_sq = -(scale * scale) / (2.0 * decay.ln());
            (-(d * d) / (2.0 * sigma_sq)).exp()
        }
        "exp" => (decay.ln() / scale * d).exp(),
        _ => {
            let s = scale / (1.0 - decay);
            ((s - d) / s).max(0.0)
        }
    };
    Some(value as f32)
}

/// The point in time an id names, or the refusal of an id that is not one.
fn pit_of(id: &Value) -> std::result::Result<crate::store::PitId, Response> {
    let text = match id {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    crate::store::PitId::decode(&text).ok_or_else(|| {
        err(StatusCode::BAD_REQUEST, "illegal_argument_exception", format!("invalid id: [{text}]"))
    })
}

/// The answer for a point in time that is no longer there -- let go of, run
/// out, or never this caller's: every shard it covered failed for want of the
/// context it was to be read through.
pub(crate) fn pit_missing(id: &crate::store::PitId) -> Response {
    use axum::response::IntoResponse;
    let reason = format!("No search context found for id [{}]", id.token);
    let mut failed = Vec::new();
    for (node, indices) in &id.parts {
        for index in indices {
            let shards = crate::cluster::with_state(|s| {
                s.indices.get(index).map(|m| m.number_of_shards).unwrap_or(1)
            });
            for shard in 0..shards {
                failed.push(json!({
                    "shard": shard, "index": index, "node": node,
                    "reason": {"type": "search_context_missing_exception", "reason": reason},
                }));
            }
        }
    }
    let cause = json!({"type": "search_context_missing_exception", "reason": reason});
    let mut r = (
        StatusCode::NOT_FOUND,
        axum::Json(json!({
            "error": {
                "root_cause": [cause],
                "type": "search_phase_execution_exception",
                "reason": "all shards failed",
                "phase": "query",
                "grouped": true,
                "failed_shards": failed,
            },
            "status": 404,
        })),
    )
        .into_response();
    r.extensions_mut().insert(crate::api::shared::ErrorKind {
        kind: "search_phase_execution_exception".to_string(),
        reason: "all shards failed".to_string(),
    });
    r
}
