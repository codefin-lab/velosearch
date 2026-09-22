//! The page of hits, dressed the way the request asked for it.

use super::*;

mod collapse;
pub(crate) use collapse::*;

pub(crate) fn source_of(
    searcher: &Searcher,
    st: &IdxState,
    addr: DocAddress,
) -> Option<(String, Value)> {
    let doc: TantivyDocument = searcher.doc(addr).ok()?;
    let id = doc.get_first(st.fields.id)?.as_str()?.to_string();
    // read straight out of the stored document: copying it into a string of
    // its own first costs a whole source per hit, and every hit on the page
    // pays it
    let src = serde_json::from_str(doc.get_first(st.fields.source)?.as_str()?).ok()?;
    Some((id, src))
}

/// Whether the request asked for hits with no identity on them.
///
/// `_none_` is the word for it, written on its own or in a list of names --
/// and only when it is the whole of what was asked for. An empty
/// `stored_fields: []` is not it: the reference still answers those hits
/// with their index and id, and its suite asserts as much.
pub(crate) fn asked_for_none(body: &Value, stored: &Option<Vec<String>>) -> bool {
    let named = match body.get("stored_fields") {
        Some(Value::Array(a)) => a.iter().any(|x| x == "_none_"),
        Some(other) => other == "_none_",
        None => false,
    };
    named && stored.as_ref().map(|s| s.is_empty()).unwrap_or(false)
}

/// Write the page of hits the client reads.
///
/// Everything expensive has happened by now: these are the documents that made
/// the page, and this is where each one is dressed -- source selection, the
/// values `fields` asked for, inner hits, highlighting, the names of the
/// clauses it matched.
#[allow(clippy::too_many_arguments)]
pub(crate) fn write_page(
    store: &Store,
    targets: &[String],
    searchers: &Searchers,
    all_hits: Vec<Hit>,
    body: &Value,
    p: &Params,
    query_json: &Option<Value>,
    sort_keys: &[SortKey],
    source_sel: &Option<Value>,
    stored: &Option<Vec<String>>,
    field_specs: &Option<Vec<(String, Option<String>)>>,
    named: &std::collections::HashMap<String, Vec<(String, f32)>>,
    named_scores: bool,
    rescored: bool,
    extras: &Extras,
    script_error: &mut Option<Response>,
) -> Vec<Value> {
    let script_fields: Vec<(String, Value, bool)> = body
        .get("script_fields")
        .and_then(|v| v.as_object())
        .map(|o| {
            o.iter()
                .map(|(name, spec)| {
                    let script = spec.get("script").cloned().unwrap_or(Value::Null);
                    let ignore =
                        spec.get("ignore_failure").and_then(|v| v.as_bool()).unwrap_or(false);
                    (name.clone(), script, ignore)
                })
                .collect()
        })
        .unwrap_or_default();
    // a sort answers with no score unless the request asked for one to be
    // kept: `track_scores` says the documents were scored as well as ordered
    // -- or the sort reads the score itself, which it cannot do without one
    let keep_score = sort_keys.is_empty()
        || sort_keys.iter().any(|k| k.field == "_score")
        || body.get("track_scores").and_then(|v| v.as_bool()).unwrap_or(false);
    all_hits
        .into_iter()
        .map(|h| {
            // `stored_fields: _none_` strips the metadata too
            let none = stored.as_ref().map(|s| s.is_empty()).unwrap_or(false)
                && body
                    .get("stored_fields")
                    .map(|v| match v {
                        // the field list may be written as one name or as a
                        // list of them, and `_none_` may be either
                        Value::Array(a) => a.iter().any(|x| x == "_none_"),
                        other => other == "_none_",
                    })
                    .unwrap_or(false);
            // `stored_fields: _none_` asks for a hit with no identity on it,
            // and the identity is taken off at the end of the search rather
            // than here: the field-level security pass looks a caller's view
            // up by `_index`, so building the hit without one handed back
            // every hidden field and every masked value in the clear.
            let _ = none;
            let mut hit = json!({
                "_index": h.index,
                "_id": h.id,
                "_score": if keep_score { json!(h.score) } else { Value::Null },
            });
            // a document written with a routing says so on every hit
            if let Some(r) = searchers[h.shard_idx].2.read().routing.get(&h.id) {
                hit["_routing"] = json!(r);
            }
            // a selector on the URL is the narrower instruction and wins over
            // one in the body
            let sel = crate::api::source_selector_from_params_pub(p).or_else(|| source_sel.clone());
            let explicit_source = sel.is_some();
            if let Some(names) = &stored {
                let mut out = serde_json::Map::new();
                for name in names.iter().filter(|n| *n != "_source") {
                    if let Some(v) = h.source.pointer(&format!("/{}", name.replace('.', "/"))) {
                        out.insert(
                            name.clone(),
                            match v {
                                Value::Array(a) => Value::Array(a.clone()),
                                other => Value::Array(vec![other.clone()]),
                            },
                        );
                    }
                }
                if !out.is_empty() {
                    hit["fields"] = Value::Object(out);
                }
            }
            // `stored_fields` suppresses `_source` unless it was asked for too,
            // and a mapping may say the document is not kept at all
            let kept = searchers[h.shard_idx].2.read().mapping.raw.pointer("/_source/enabled")
                != Some(&json!(false));
            let want_source = kept
                && (stored.is_none()
                    || explicit_source
                    || stored.as_ref().map(|s| s.iter().any(|n| n == "_source")).unwrap_or(false));
            if want_source {
                let src = match &sel {
                    Some(s) => apply_source_selector(&h.source, s),
                    None => h.source.clone(),
                };
                if !src.is_null() {
                    hit["_source"] = src;
                }
            }
            if let Some(ig) = &h.ignored {
                hit["_ignored"] = ig.clone();
            }
            // `explain` asks where the score came from. What can be said here
            // is the score itself and what it was arrived at by.
            if body.get("explain").and_then(|v| v.as_bool()).unwrap_or(false) {
                let description = if rescored {
                    "sum of the query score and the rescoring query score"
                } else if sort_keys.is_empty() {
                    "score of the query"
                } else {
                    "the query matched; the order comes from the sort"
                };
                // what VeloCore can say about the score, told Lucene's way;
                // where it can say nothing, the score itself stands
                let told = (!rescored && sort_keys.is_empty())
                    .then(|| {
                        let g = searchers[h.shard_idx].2.read();
                        crate::security::with_dls(store, &g.name, query_json.clone())
                            .and_then(|q| explain_document(&g, &q, &h.id))
                    })
                    .flatten();
                hit["_explanation"] = told.unwrap_or_else(|| {
                    json!({
                        "value": h.score,
                        "description": description,
                        "details": [],
                    })
                });
                // an explained hit says which shard answered for it, and which
                // node that shard is on
                let shard = searchers[h.shard_idx].2.read().shard_of_doc(&h.id);
                hit["_shard"] = json!(format!("[{}][{}]", h.index, shard));
                hit["_node"] = json!("node-0");
            }
            if !h.sort.is_empty() {
                // the column holds the number the field reports -- a date is
                // milliseconds, a date_nanos is nanoseconds -- so a sort value
                // goes out as it was read
                hit["sort"] = Value::Array(
                    h.sort
                        .iter()
                        .enumerate()
                        .map(|(at, s)| {
                            // a sort asked to read a field as another width
                            // reports its values at that width
                            match sort_keys.get(at).and_then(|k| k.numeric_type.as_deref()) {
                                Some("long" | "int" | "date" | "date_nanos") => match s.to_json() {
                                    Value::Number(n) => match n.as_f64() {
                                        Some(v) => json!(v as i64),
                                        None => Value::Number(n),
                                    },
                                    other => other,
                                },
                                _ => s.to_json(),
                            }
                        })
                        .collect(),
                );
            }
            if let Some(specs) = field_specs.as_ref() {
                let g = searchers[h.shard_idx].2.read();
                // a flat_object is one value unless the request named a path
                // inside it, in which case it has to be descended
                let is_leaf = |p: &str| {
                    // a point written as an object is one value, not two
                    (g.mapping.is_leaf_type(p) || g.mapping.type_of(p) == Some("geo_point"))
                        && !specs.iter().any(|(n, _)| {
                            n.len() > p.len() && n.starts_with(p) && n.as_bytes()[p.len()] == b'.'
                        })
                };
                // a field without doc values has nothing for `fields` to read
                let names: Vec<String> = specs
                    .iter()
                    .map(|(n, _)| n.clone())
                    .filter(|n| g.mapping.field_option(n, "doc_values") != Some(json!(false)))
                    .collect();
                // doc values are kept sorted, so a field read as doc values
                // comes back in that order
                let docvalue_names: Vec<String> = body
                    .get("docvalue_fields")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| match v {
                                Value::String(s) => Some(s.clone()),
                                Value::Object(o) => {
                                    o.get("field").and_then(|f| f.as_str()).map(|s| s.to_string())
                                }
                                _ => None,
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                // a derived field is not in the source: it is made from it
                let derived_source = names
                    .iter()
                    .any(|n| g.mapping.is_derived(n))
                    .then(|| crate::store::with_derived(&h.source, &g.mapping));
                let read_from = derived_source.as_ref().unwrap_or(&h.source);
                let mut raw = crate::source::extract_fields(read_from, &names, &is_leaf);
                // a derived object asked for by name is the text its script
                // emitted, not the object read out of that text
                for name in &names {
                    if g.mapping.is_derived(name)
                        && g.mapping.type_of(name) == Some("object")
                        && let Some(text) =
                            crate::store::derived_text_of(&h.source, &g.mapping, name)
                    {
                        raw.insert(name.clone(), text);
                    }
                }
                // A field may be asked for more than once, each time with its
                // own format, and each asking adds its values to the one list
                // the field is reported under.
                let mut f = serde_json::Map::new();
                for (name, fmt) in specs.iter() {
                    let mut values = match name.as_str() {
                        // the metadata a document carries is asked for the same
                        // way as its own fields, and is not in the source
                        "_seq_no" => json!([h.seq]),
                        "_index" => json!([h.index.clone()]),
                        "_id" => json!([h.id.clone()]),
                        _ => match raw.get(name) {
                            Some(v) => v.clone(),
                            None => continue,
                        },
                    };
                    if docvalue_names.iter().any(|n| n == name)
                        && let Value::Array(items) = &mut values
                        && body
                            .get("fields")
                            .and_then(|f| f.as_array())
                            .map(|a| !a.iter().any(|v| v.as_str() == Some(name.as_str())))
                            .unwrap_or(true)
                    {
                        items.sort_by(|a, b| match (a.as_f64(), b.as_f64()) {
                            (Some(x), Some(y)) => {
                                x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal)
                            }
                            _ => a.to_string().cmp(&b.to_string()),
                        });
                    }
                    if let (Some(fmt), Value::Array(items)) = (fmt, &mut values) {
                        for v in items.iter_mut() {
                            if let Some(text) = crate::source::format_date(v, fmt) {
                                *v = text;
                            } else if let Some(n) = v.as_f64()
                                && let Some(text) = decimal_format(fmt, n)
                            {
                                *v = json!(text);
                            }
                        }
                    }
                    match (f.get_mut(name.as_str()), values) {
                        (Some(Value::Array(into)), Value::Array(more)) => into.extend(more),
                        (_, values) => {
                            f.insert(name.clone(), values);
                        }
                    }
                }
                // a shape is reported as the GeoJSON it stands for, or as the
                // well-known text a `wkt` format asks for
                for (name, fmt) in specs.iter() {
                    if g.mapping.type_of(name) != Some("geo_shape") {
                        continue;
                    }
                    let Some(Value::Array(items)) = f.get_mut(name.as_str()) else { continue };
                    for value in items.iter_mut() {
                        if let Some(written) = crate::search::shape_as(value, fmt.as_deref()) {
                            *value = written;
                        }
                    }
                }
                // a token_count field stores the text but reports the count
                for (name, vals) in f.iter_mut() {
                    if g.mapping.type_of(name) != Some("token_count") {
                        continue;
                    }
                    if let Value::Array(items) = vals {
                        for v in items.iter_mut() {
                            if let Some(t) = v.as_str() {
                                *v = json!(crate::store::token_count(t));
                            }
                        }
                    }
                }
                // a value the index refused is not a value the field has
                if let Some(Value::Array(ig)) = &h.ignored {
                    for name in ig.iter().filter_map(|v| v.as_str()) {
                        f.remove(name);
                    }
                }
                // `stored_fields` may have filled some in already; both
                // selections share the one `fields` section
                if let Some(Value::Object(existing)) = hit.get("fields") {
                    for (k, v) in existing {
                        f.entry(k.clone()).or_insert_with(|| v.clone());
                    }
                }
                if !f.is_empty() {
                    hit["fields"] = Value::Object(f);
                }
            }
            // a script field is whatever its script returns for the document
            if !script_fields.is_empty() && script_error.is_none() {
                let g = searchers[h.shard_idx].2.read();
                let mut seen = h.source.clone();
                crate::security::narrow_source(store, &h.index, &mut seen);
                let expanded = crate::store::expand_for_indexing(seen, &g.mapping);
                let mut f = match hit.get("fields") {
                    Some(Value::Object(o)) => o.clone(),
                    _ => serde_json::Map::new(),
                };
                for (name, script, ignore) in &script_fields {
                    match crate::painless::contexts::run_on_doc(
                        script,
                        &expanded,
                        &g.mapping,
                        h.score as f64,
                    ) {
                        Ok(v) => {
                            let out = match v.to_json() {
                                Value::Array(a) => Value::Array(a),
                                Value::Null => continue,
                                other => Value::Array(vec![other]),
                            };
                            f.insert(name.clone(), out);
                        }
                        Err(_) if *ignore => {}
                        Err(e) => {
                            *script_error = Some(crate::search::search_script_failure(e, &h.index));
                        }
                    }
                }
                if !f.is_empty() {
                    hit["fields"] = Value::Object(f);
                }
            }
            // a nested query may ask for the objects it matched to be listed,
            // and a nested query inside one asks the same of the objects under
            // those
            if extras.nested_inner_hits {
                let mut clauses = Vec::new();
                if let Some(q) = body.get("query") {
                    collect_nested_inner_hits(q, &mut clauses);
                }
                if !clauses.is_empty() {
                    let g = searchers[h.shard_idx].2.read();
                    let kept = g.mapping.raw.pointer("/_source/enabled") != Some(&json!(false));
                    let groups = nested_inner_hits(
                        &h,
                        &h.source,
                        "",
                        &clauses,
                        kept,
                        query_json,
                        &g.mapping,
                        &g.index,
                        &g.analysis,
                    );
                    if !groups.is_empty() {
                        hit["inner_hits"] = Value::Object(groups);
                    }
                }
            }
            if let Some(hits) = named.get(&h.id) {
                hit["matched_queries"] = if named_scores {
                    let mut m = serde_json::Map::new();
                    for (n, s) in hits {
                        m.insert(n.clone(), json!(s));
                    }
                    Value::Object(m)
                } else {
                    let mut names: Vec<String> = hits.iter().map(|(n, _)| n.clone()).collect();
                    names.sort();
                    json!(names)
                };
            }
            // a collapsed hit says which value it stands for, and may carry
            // the group it was chosen from
            if let Some(field) = body.pointer("/collapse/field").and_then(|v| v.as_str()) {
                let real = searchers[h.shard_idx]
                    .2
                    .read()
                    .mapping
                    .target_of(field)
                    .unwrap_or(field)
                    .to_string();
                let path = format!("/{}", real.replace('.', "/"));
                if let Some(v) = h.source.pointer(&path) {
                    let list = match v {
                        Value::Array(a) => a.clone(),
                        other => vec![other.clone()],
                    };
                    let mut f = match hit.get("fields") {
                        Some(Value::Object(o)) => o.clone(),
                        _ => serde_json::Map::new(),
                    };
                    f.insert(field.to_string(), Value::Array(list.clone()));
                    hit["fields"] = Value::Object(f);
                    // a hit may be asked for its group more than once, each
                    // time with a different name and ordering
                    let asked = match body.pointer("/collapse/inner_hits") {
                        Some(Value::Array(a)) => a.clone(),
                        Some(other) => vec![other.clone()],
                        None => Vec::new(),
                    };
                    let mut groups = serde_json::Map::new();
                    for inner in &asked {
                        let Some(group) = collapsed_group(
                            store,
                            targets,
                            query_json.as_ref(),
                            field,
                            list.first().unwrap_or(&Value::Null),
                            inner,
                            p,
                        ) else {
                            continue;
                        };
                        let name =
                            inner.get("name").and_then(|n| n.as_str()).unwrap_or("inner_hits");
                        groups.insert(name.to_string(), group);
                    }
                    if !groups.is_empty() {
                        hit["inner_hits"] = Value::Object(groups);
                    }
                }
            }
            if let Some(spec) = body.get("highlight") {
                let g = searchers[h.shard_idx].2.read();
                if let Some(hl) =
                    build_highlight(spec, &h.source, query_json, &g.mapping, &g.index, &g.analysis)
                {
                    hit["highlight"] = hl;
                }
            }
            if body.get("version").and_then(|v| v.as_bool()).unwrap_or(false) {
                hit["_version"] = json!(h.version);
            }
            if body_or_param(body, p, "seq_no_primary_term")
                .map(|v| v == json!(true) || v == json!("true"))
                .unwrap_or(false)
            {
                hit["_seq_no"] = json!(h.seq);
                hit["_primary_term"] = json!(searchers[h.shard_idx].2.read().term_of(&h.id));
            }
            hit
        })
        .collect()
}

/// Read them off the request, complaining where the request asks for something
/// the mapping cannot answer.
pub(crate) fn output_specs(
    store: &Store,
    targets: &[String],
    body: &Value,
    p: &Params,
) -> std::result::Result<OutputSpecs, Response> {
    // `fields` reads values back out of the stored source; without one there
    // is nothing to read, and a date format asks a field that holds no dates
    // to answer in a shape it has no values for
    if let Some(specs) = body.get("fields").and_then(|v| v.as_array()) {
        for name in targets.iter() {
            let Some(st) = store.get(name) else { continue };
            let g = st.read();
            if g.mapping.raw.pointer("/_source/enabled") == Some(&json!(false)) {
                return Err(err(
                    StatusCode::BAD_REQUEST,
                    "illegal_argument_exception",
                    format!(
                        "Unable to retrieve the requested [fields] since _source is disabled \
                         in the mappings for index [{name}]"
                    ),
                ));
            }
            for spec in specs {
                let (Some(f), Some(_)) =
                    (spec.get("field").and_then(|v| v.as_str()), spec.get("format"))
                else {
                    continue;
                };
                // a shape names its own formats, and a number the pattern its
                // digits are written with; the rest have only dates to format
                if !matches!(
                    g.mapping.type_of(f),
                    None | Some(
                        "date"
                            | "date_nanos"
                            | "date_range"
                            | "geo_shape"
                            | "geo_point"
                            | "long"
                            | "integer"
                            | "short"
                            | "byte"
                            | "double"
                            | "float"
                            | "half_float"
                            | "scaled_float"
                            | "unsigned_long"
                    )
                ) {
                    return Err(err(
                        StatusCode::BAD_REQUEST,
                        "illegal_argument_exception",
                        format!("error fetching [{f}]: field has no date formatter"),
                    ));
                }
            }
        }
    }
    // a spec is either a bare field name or an object naming a format
    let spec_list = |v: Option<&Value>| -> Option<Vec<(String, Option<String>)>> {
        v.and_then(|v| v.as_array()).map(|a| {
            a.iter()
                .filter_map(|x| match x {
                    Value::String(s) => Some((s.clone(), None)),
                    Value::Object(o) => o.get("field").and_then(|f| f.as_str()).map(|s| {
                        (
                            s.to_string(),
                            o.get("format").and_then(|f| f.as_str()).map(|s| s.to_string()),
                        )
                    }),
                    _ => None,
                })
                .collect()
        })
    };
    // `docvalue_fields` may also be named on the URL, as a comma-separated list
    let param_docvalues: Option<Value> = p
        .get("docvalue_fields")
        .filter(|v| !v.is_empty())
        .map(|v| Value::Array(v.split(',').map(|f| json!(f.trim())).collect()));
    let body_docvalues = body.get("docvalue_fields").cloned().or(param_docvalues);
    let fields = match (spec_list(body.get("fields")), spec_list(body_docvalues.as_ref())) {
        (Some(mut a), Some(b)) => {
            a.extend(b);
            Some(a)
        }
        (a, b) => a.or(b),
    };
    let stored: Option<Vec<String>> = match body.get("stored_fields") {
        // `_none_` asks for no fields at all, written either way
        Some(Value::String(s)) if s == "_none_" => Some(vec![]),
        Some(Value::Array(a)) if a.iter().any(|x| x == "_none_") => Some(vec![]),
        Some(Value::Array(a)) => {
            Some(a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
        }
        Some(Value::String(s)) => Some(vec![s.clone()]),
        _ => None,
    };
    Ok(OutputSpecs { source: body.get("_source").cloned(), fields, stored })
}

/// Every clause of a query that was given a `_name`, paired with the clause
/// itself so it can be asked about one document at a time.
pub(crate) fn named_clauses(node: &Value, out: &mut Vec<(String, Value)>) {
    match node {
        Value::Object(o) => {
            for (k, v) in o {
                if k == "_name" {
                    continue;
                }
                let named = v
                    .get("_name")
                    .and_then(|n| n.as_str())
                    // `match: {field: {query, _name}}` puts the name beside the
                    // field's options rather than beside the clause
                    .or_else(|| {
                        v.as_object()
                            .filter(|o| o.len() == 1)
                            .and_then(|o| o.values().next())
                            .and_then(|inner| inner.get("_name"))
                            .and_then(|n| n.as_str())
                    });
                if let Some(name) = named {
                    out.push((name.to_string(), json!({k.clone(): v.clone()})));
                }
                named_clauses(v, out);
            }
        }
        Value::Array(a) => {
            for v in a {
                named_clauses(v, out);
            }
        }
        _ => {}
    }
}

/// Take the `_name` markers out of a clause.
pub(crate) fn strip_names(node: &mut Value) {
    match node {
        Value::Object(o) => {
            o.remove("_name");
            for (_, v) in o.iter_mut() {
                strip_names(v);
            }
        }
        Value::Array(a) => {
            for v in a {
                strip_names(v);
            }
        }
        _ => {}
    }
}

/// Which named clauses each document on the page matched, and with what score.
pub(crate) fn matched_names(
    store: &Store,
    targets: &[String],
    body: &Value,
    ids: &[String],
) -> std::collections::HashMap<String, Vec<(String, f32)>> {
    let mut out: std::collections::HashMap<String, Vec<(String, f32)>> =
        std::collections::HashMap::new();
    let mut clauses = Vec::new();
    if let Some(q) = body.get("query") {
        named_clauses(q, &mut clauses);
    }
    for r in body.get("rescore").into_iter().flat_map(|r| match r {
        Value::Array(a) => a.clone(),
        other => vec![other.clone()],
    }) {
        named_clauses(&r, &mut clauses);
    }
    if clauses.is_empty() || ids.is_empty() {
        return out;
    }
    for (name, mut clause) in clauses {
        // the clause is asked about on its own, and must not carry the name
        // that would make it a named clause all over again
        strip_names(&mut clause);
        let probe = json!({
            "query": {"bool": {"must": [clause], "filter": [{"terms": {"_id": ids}}]}},
            "size": ids.len(),
        });
        let Ok(answer) = run(store, &targets.join(","), &probe, &Params::new()) else { continue };
        for hit in answer.hits {
            let Some(id) = hit.get("_id").and_then(|v| v.as_str()) else { continue };
            let score = hit.get("_score").and_then(|v| v.as_f64()).unwrap_or(1.0) as f32;
            out.entry(id.to_string()).or_default().push((name.clone(), score));
        }
    }
    out
}

/// Scores written the way the reference writes them: a score is a 32-bit
/// float, and Java prints the shortest text that reads back as that float.
/// Widened to 64 bits first, `0.50652754` came out as `0.5065275430679321`
/// on every hit, inner hit and explanation.
pub(crate) fn shorten_scores(v: &mut Value) {
    fn short(x: &mut Value) {
        if let Some(f) = x.as_f64()
            && x.is_f64()
            && let Ok(back) = format!("{}", f as f32).parse::<f64>()
            && let Some(n) = serde_json::Number::from_f64(back)
        {
            *x = Value::Number(n);
        }
    }
    match v {
        Value::Object(o) => {
            for (k, child) in o.iter_mut() {
                match k.as_str() {
                    "_score" | "max_score" => short(child),
                    "_explanation" => shorten_explanation(child),
                    _ => shorten_scores(child),
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(shorten_scores),
        _ => {}
    }
    fn shorten_explanation(e: &mut Value) {
        if let Some(o) = e.as_object_mut() {
            if let Some(value) = o.get_mut("value") {
                short(value);
            }
            if let Some(Value::Array(details)) = o.get_mut("details") {
                details.iter_mut().for_each(shorten_explanation);
            }
        }
    }
}

/// Assemble the `hits` envelope, honouring track_total_hits and the
/// `rest_total_hits_as_int` compatibility switch.
pub(crate) fn envelope(out: Outcome, body: &Value, p: &Params) -> Value {
    let out_shards = out.shards;
    let out_timed_out = out.timed_out;
    let out_failures = out.failures.clone();
    let out_skipped = out.skipped;
    let out_took = out.took_ms;
    let brs = p
        .get("batched_reduce_size")
        .and_then(|v| v.parse::<u64>().ok())
        .or_else(|| body.get("batched_reduce_size").and_then(|v| v.as_u64()))
        .unwrap_or(512);
    let num_reduce_phases =
        if brs > 1 && out_shards > 1 { out_shards.saturating_sub(1).div_ceil(brs - 1) } else { 1 };
    let track = body.get("track_total_hits").cloned().or_else(|| {
        p.get("track_total_hits").map(|v| match v.as_str() {
            "true" => json!(true),
            "false" => json!(false),
            other => other.parse::<u64>().map(|n| json!(n)).unwrap_or(json!(true)),
        })
    });

    let mut hits_obj = json!({
        "max_score": out.max_score.map(|s| json!(s)).unwrap_or(Value::Null),
        "hits": out.hits,
    });
    shorten_scores(&mut hits_obj);

    let as_int = p.get("rest_total_hits_as_int").map(|v| v == "true").unwrap_or(false);
    let disabled = matches!(track, Some(Value::Bool(false)))
        || matches!(&track, Some(Value::String(s)) if s == "false");
    if disabled {
        // the int form reports -1 for "not tracked"; the object form is omitted
        if as_int {
            hits_obj["total"] = json!(-1);
        }
    } else {
        let limit = match &track {
            Some(Value::Bool(true)) => u64::MAX,
            Some(Value::Number(n)) => n.as_u64().unwrap_or(DEFAULT_TRACK_TOTAL_HITS),
            _ => DEFAULT_TRACK_TOTAL_HITS,
        };
        let (value, relation) = if out.total > limit { (limit, "gte") } else { (out.total, "eq") };
        hits_obj["total"] =
            if as_int { json!(value) } else { json!({"value": value, "relation": relation}) };
    }

    let mut resp = json!({
        "took": out_took,
        "timed_out": out_timed_out,
        "_shards": {
            "total": out_shards,
            "successful": out_shards.saturating_sub(out_failures.len() as u64),
            "skipped": out_skipped,
            "failed": out_failures.len(),
        },
        "hits": hits_obj,
    });
    // the number of reductions is said only when there was more than one
    if num_reduce_phases > 1 {
        resp["num_reduce_phases"] = json!(num_reduce_phases);
    }
    // a search that stopped before it had seen everything says so: one told
    // how far to look, and one asking only for aggregations, which needs no
    // hits at all
    let asked_size = body
        .get("size")
        .and_then(|v| v.as_u64())
        .or_else(|| p.get("size").and_then(|v| v.parse::<u64>().ok()));
    let aggregating = body.get("aggs").or_else(|| body.get("aggregations")).is_some();
    if body.get("terminate_after").is_some()
        || p.contains_key("terminate_after")
        || (asked_size == Some(0) && aggregating && !out.filtered)
    {
        resp["terminated_early"] = json!(true);
    }
    if !out_failures.is_empty() {
        resp["_shards"]["failures"] = Value::Array(out_failures);
    }
    if let Some(a) = out.aggs {
        resp["aggregations"] = a;
    }
    if let Some(pr) = out.profile {
        resp["profile"] = pr;
    }
    if let Some(sg) = out.suggest {
        resp["suggest"] = sg;
    }
    resp
}
