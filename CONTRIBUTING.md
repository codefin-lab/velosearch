# Working on VeloSearch

## Where things are

```
src/
  main.rs          the routing table: every endpoint, in one list
  lib.rs           the same code as a library, so benchmarks drive what the server does

  api/             one module per thing an endpoint is about
    doc/           writing and reading documents
      mod          one document: index, get, delete, and the version checks
      bulk  update  many  termvectors
    source         what of a document goes back, and in what shape
    search_api     _search and everything beside it: msearch, scroll, field_caps, analyze
    indices/       an index as a thing
      mod          made, opened, closed, refreshed, deleted
      resize       cloned, shrunk, split, rolled over
      shards       what it is made of, and the state those parts are in
    cat/           the same answers, as columns
      mod          dispatch      render  columns into text     tables  per endpoint
    mapping        settings         alias         template
    cluster        nodes            stats
    snapshot       ingest           datastream    tasks
    shared         errors, parameters, and the shapes of an answer

  search/          answering a search
    mod            the pipeline: run, and the types the rest of it passes around
    shard          one index's share of a search
    limits         what a request may ask for
    candidates     rescoring and index boosts, once every shard has answered
    page  sort  nested  geo  highlight  suggest  extras  lookup  profile  routing  calendar
    aggs/
      plan/        who answers which aggregation
        mod        the plan itself      check  what may be asked      rewrite  reshaping
      bucket/      the aggregations that make buckets
        terms  ranges  filters
      composite  histogram  metric  pipeline  format

  query/           the query DSL as VeloCore queries
    mod            Ctx, and what a field name resolves to
    dispatch       one query name to one VeloCore query
    text  range  terms  bool  pattern  analyze

  store/           what an index is, and what it holds
    mod            the types: Fields, Mapping, IdxState, Store
    registry       the indices this node has
    objects        scrolls, templates, repositories, snapshots, pipelines, data streams
    translog  writer  ids  settings  mapping  coerce  dates  net

  analysis/        the analysis chain: tokenizers, filters, the built-in analysers, the
                   stemmers, the dictionary-backed languages, phonetic and phone numbers
  ingest/          the ingest processors: grok, dissect, geoip, user-agent, attachment
  painless/        Painless: lexer, parser, evaluator, the whitelist, the contexts
  security/        TLS, users and roles, the caller in the query path, SAML/OIDC/LDAP, audit
  cluster/         the cluster: transport, state, consensus, allocation, replication,
                   recovery, the coordinator -- and the simulation it is checked in
  snapshot/        repositories: filesystem, URL, S3, GCS, Azure
  ism/             index state management: policies and their actions
  knn/             vector search: the spaces, the store, the HNSW graph
  sql/             SQL and PPL: lexer, parser, planner, the row shapes
  console/         the console's server: the shell, settings, saved objects and their
                   migrations, the searches, sample data, the plugin routes it answers for
  bin/console.rs   the console's routing table, the way main.rs is the server's
  blockstats.rs    per-block statistics, so a range scan can skip runs
  hdr.rs  tz.rs    percentile sketches, and the zone database
```

A type lives in its `mod.rs`; the functions that work on it live in the module
named after what they are for. A child module can see its parent's private
items, which is why moving a function rarely means widening anything.

## Before you push

```bash
cargo build --release          # no warnings
cargo clippy --all-targets     # no warnings
cargo fmt --check              # no diff
cargo deny check               # advisories, licences and where a crate came from
```

A change that touches what a node refuses, or what it reports about itself,
has two gates of its own -- each starts the nodes it needs:

```bash
python3 tools/limits_check.py    # the memory, the concurrency and the time
python3 tools/metrics_check.py   # the exporter, its numbers, and the log
```

and the corpus, which is the point of the whole thing:

```bash
VELOSEARCH_NODE_ATTRS=testattr=test ./target/release/velosearch &
python3 tools/yaml_runner.py --manifest tools/phase1_manifest.json    # 398/398
python3 tools/yaml_runner.py --manifest tools/phase3_manifest.json    # 1,428/1,428
python3 tools/module_gate.py                                          # 880/890
```

A change to the console is gated the same way, against OpenSearch
Dashboards' own suite: `tools/dashboards_gate.py`, then `tools/console_diff.py`
against a running Dashboards (`docs/console.md` says how to start one).

CI runs all of it, twice: once with indices in memory, once on disk.

The toolchain is pinned in `rust-toolchain.toml`. Bumping it is a change like
any other: a newer clippy finds new things, and they get fixed in the commit
that moves the pin, not in whichever commit happens to be pushed afterwards.

## The tools

Every number in the README is produced by one of these.

| | |
|---|---|
| `tools/yaml_runner.py` | OpenSearch's own YAML tests against this server |
| `tools/module_gate.py` | the module and plugin suites, run the way they were written (two nodes) |
| `tools/gate_node.sh` | the node the suites expect, with everything they read back |
| `tools/compat_audit.py` | what a cluster uses, and where two engines answer differently |
| `tools/bench_matrix.py` | every bench dimension, both engines, same corpus, same machine |
| `tools/cloud_bench_gcp.sh` | the same matrix on a machine rented from GCP (Terraform in `tools/cloud_bench/`) |
| `tools/gen_dataset.py` | the http-log corpus the benchmarks use |
| `tools/cluster_chaos.py`, `tools/linearize.py`, `tools/rolling_upgrade.py` | three real nodes: chaos and soak, linearizability, a rolling upgrade |
| `tools/knn_check.py`, `tools/sql_check.py`, `tools/ism_check.py`, `tools/object_store_check.py` | end-to-end checks of the plugins' APIs |
| `tools/dashboards_gate.py` | OpenSearch Dashboards' own API suite against the console's server, with the Node server's baseline |
| `tools/dashboards_check.py`, `tools/console_diff.py` | what that suite never asks about, and the shell compared field by field with the Node server's |
| `tools/osd_pin.py`, `tools/osd_sample_data.js` | what the console pins from a running Dashboards |
| `tools/limits_check.py` | the three ceilings a node refuses at, each put in the state and its refusal read |
| `tools/metrics_check.py` | the Prometheus endpoint parsed as a scraper parses it, and the log |

## What to read first

`CONTEXT.md` is what the words mean. `docs/adr/` is why eight decisions were
made the way they were -- read 0001 before touching analysis, 0002 before
touching the cluster, and 0005 before touching anything a search can read.
`CHANGELOG.md` is what is provided and how it is measured.

## The rules that are not style

- **No `unwrap()` on a request path.** If an invariant really holds, `expect`
  it and say why in the message.
- **A comment says why, not what.** The code says what.
- **A change that costs performance says so in its commit message.** CI
  measures every commit against the last one and goes red at a 5% fall.
- **The corpus is the specification.** If OpenSearch's test says a response
  looks a certain way, that is the answer, whatever seems more sensible.
