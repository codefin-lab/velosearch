<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/assets/velosearch-logo-dark.png">
    <img src="docs/assets/velosearch-logo.png" alt="VeloSearch" width="560">
  </picture>
</p>

# VeloSearch

[![ci](https://github.com/codefin-lab/velosearch/actions/workflows/ci.yml/badge.svg)](https://github.com/codefin-lab/velosearch/actions/workflows/ci.yml)

A search engine written in Rust, on top of
[VeloCore](https://github.com/codefin-lab/velocore) (a fork of tantivy), with
the OpenSearch interface.

The REST API is the same one: the same requests, the same JSON back, the same
words in its errors. So are the tests -- it runs OpenSearch's own conformance
suite, and asks a running OpenSearch the same questions and compares the
answers, so that a client, a dashboard or a script written for that API works
with it unchanged.

## Where it stands

Everything below is produced by a script in `tools/`, so it can be checked
rather than believed.

| | | how |
|---|---|---|
| OpenSearch's core suite | **1,428 of 1,428** not skipped, over all 410 files of it (77 skipped) | `tools/yaml_runner.py --manifest tools/phase3_manifest.json` |
| its module and plugin suites | **880 of 890**, 4 skipped -- with the geoip databases and the Beider-Morse rules in place; without them 871, the difference being what is on the disk rather than what the code does (`docs/geoip.md`, `docs/phonetic.md`) | `tools/module_gate.py` |
| the same answer as OpenSearch 3.1.0 | **166 of 183** canonical requests: the answer identical, the bookkeeping around it (timings, ids) scrubbed; `--strict` compares the whole response body rather than only the answer inside it, still scrubbed | `tools/compat_audit.py replay` |
| REST endpoints routed | **167 of 167** APIs on every path and method they name | `tools/endpoint_gate.py` |
| the bench matrix | **17 of 18 dimensions quicker or lighter** | `tools/bench_matrix.py` |
| beside OpenSearch 3.1.0, on the same machine, 34 dimensions | **quicker or lighter on all 34**, the full table with how it was measured in [docs/performance.md](docs/performance.md) | `tools/bench.py`, `tools/bench_gate.py` |
| this build against this repository's own last numbers | 34 dimensions, nothing allowed to fall more than 5% past the machine's own spread; the run also reports the kept OpenSearch measurement (**34 of 34**) | `tools/bench_gate.py` |
| who may reach what | **1,882 answers** over 395 routes and five callers | `tools/auth_matrix.py` |
| a refused write leaves the document alone | **30 refusals** through five write paths, counted as the check makes them rather than written into it | `tools/refusal_check.py` |
| every acknowledged write survives `kill -9` | **10,001 writes**, five index shapes | `tools/restart_check.py` |
| one node worked steadily for half an hour | **2.53 million writes** acknowledged over 2.93 million requests; every sampled one there afterwards and after a restart, nothing refused, and an index that does not grow answered in 0.7 ms at the start and 0.7 ms at the end | `tools/soak_check.py` |
| a disk with no room left | **250 writes** against a full volume, 100 of them refused with `No space left on device`; everything acknowledged survives the filling and a `kill -9` | `tools/disk_fault_check.py` |
| a backup restored against what went in | **2,500 documents** compared one by one, routing and all; a file cut short, one with a spoiled line, and one taken away are each refused rather than half restored | `tools/snapshot_check.py` |
| the container's probe tells healthy from unready | **9 checks** over four nodes: security off, authentication on, TLS on, and one of a cluster with no cluster manager | `tools/health_check.py` |
| the built image's own healthcheck | **9 checks** over four containers, Docker running the HEALTHCHECK and the verdict read from `docker inspect`: security off, authentication on, TLS on, and one of a cluster with no cluster manager | `tools/docker_health_check.py` |
| the security surface, against OpenSearch's own | **40 questions** asked of both engines as an administrator and as a filtered user -- what the filter hides, what the caller may not reach, and what the security API answers to a role, user or mapping that is wrong | `tools/security_replay.py` |
| a TLS deployment, verified rather than waved through | **13 checks**: the chain and the hostname checked against a pinned CA, plain http refused on the TLS port, no credentials and wrong passwords refused, a user held to the one role it was given | `tools/tls_auth_check.py` |
| what a node refuses to spend | **33 checks** over three ceilings: the memory a request may take, how many the node runs at once, and how long one may walk -- each put in the state and the refusal read ([docs/limits.md](docs/limits.md)) | `tools/limits_check.py` |
| malformed input at everything that parses | **2,000 probes**, node still answering | `tools/fuzz_check.py` |
| OpenSearch Dashboards' own API suite, against the console's server | **146 of 166**, none failed that the Node server passes (it scores 140) | `tools/dashboards_gate.py` |
| three nodes, faults, and every acknowledged write | **200 runs of 200 clean**: ninety seconds each of isolations, SIGTERM restarts, SIGSTOP pauses and heals under a write load, then every acknowledged document checked on every copy -- none lost, none behind, the copies agreeing | `tools/cluster_chaos.py` |
| the same on Linux | the build, the corpus, the examples, the checks and twenty-five chaos runs on an eight-core Ubuntu 24.04 machine: the same answers as on the Mac | the scripts above, on a GCE `n2-standard-8` |
| twenty-six worked examples | each a project of its own -- product search, logs, facets, geo, vectors, security, ingest, scripting, nine languages, SQL, joins, snapshots, failover, reindex, deep paging, relevance, percolation, data streams, search pipelines, background jobs, service accounts, clause search, attachments, analytics, a runbook, routing -- run against a real node with every answer checked | `examples/run-all.sh` |

Of the ten module sections that do not pass, the stempel (Polish) and
Ukrainian analysis sections need dictionaries that are not redistributed here,
and one asserts that its plugin is the only one installed, which a single
binary cannot be. Five more are set aside as tests of the test framework
rather than of a server, and `tools/module_gate.py` prints those and why on
every run.

## Performance

Both engines measured on the same machine, on the same day, with the same
corpus and the same client: a Google Compute Engine `n2-standard-8` (eight
vCPUs, Ubuntu 24.04, SSD), 200,000 web-log documents, `tools/bench.py` driving
each in turn with nothing else running -- OpenSearch 3.1.0 from its official
image with security off, VeloSearch as this repository builds it. Five runs
each, the median shown. The same interface, so the same thirty-four
measurements apply to both; VeloSearch measured quicker or lighter on each.

| dimension | unit | OpenSearch 3.1.0 | VeloSearch | better by |
|---|---|---|---|---|
| queries a second, one client | q/s | 134.9 | 378.7 | +181% |
| memory, idle | MB | 1,501 | 37.7 | +97% |
| memory, after the search run | MB | 1,664 | 204.1 | +88% |
| memory, after indexing 200k | MB | 1,614 | 216.5 | +87% |
| agg nested p50 c1 | ms | 6.6 | 1.2 | +82% |
| agg date hist p50 c1 | ms | 6.5 | 1.2 | +81% |
| time range agg p50 c1 | ms | 6.4 | 1.4 | +79% |
| agg terms p50 c1 | ms | 6.0 | 1.4 | +77% |
| term numeric p50 c1 | ms | 7.5 | 1.9 | +75% |
| match all p50 c1 | ms | 6.6 | 1.8 | +73% |
| term keyword p50 c1 | ms | 6.5 | 1.8 | +72% |
| latency p50, one client | ms | 7.0 | 2.2 | +69% |
| match text p50 c1 | ms | 8.5 | 2.6 | +69% |
| time range p50 c1 | ms | 6.6 | 2.7 | +59% |
| sort paged p50 c1 | ms | 8.7 | 3.6 | +58% |
| queries a second, eight clients | q/s | 425.8 | 665.9 | +56% |
| range numeric p50 c1 | ms | 7.2 | 3.6 | +50% |
| latency p90, one client | ms | 9.1 | 4.7 | +48% |
| bool filter p50 c1 | ms | 9.1 | 4.8 | +46% |
| agg date hist p50 c8 | ms | 16.3 | 8.8 | +46% |
| term numeric p50 c8 | ms | 16.6 | 9.1 | +45% |
| match text p50 c8 | ms | 18.5 | 10.5 | +43% |
| time range agg p50 c8 | ms | 14.1 | 8.1 | +42% |
| term keyword p50 c8 | ms | 15.8 | 9.2 | +41% |
| match all p50 c8 | ms | 14.8 | 8.8 | +40% |
| agg terms p50 c8 | ms | 14.7 | 9.2 | +37% |
| latency p50, eight clients | ms | 16.4 | 10.3 | +37% |
| agg nested p50 c8 | ms | 15.3 | 9.6 | +37% |
| time range p50 c8 | ms | 16.1 | 10.1 | +37% |
| range numeric p50 c8 | ms | 16.9 | 11.4 | +33% |
| latency p90, eight clients | ms | 23.9 | 16.4 | +31% |
| sort paged p50 c8 | ms | 17.6 | 12.6 | +29% |
| bool filter p50 c8 | ms | 18.6 | 14.5 | +22% |
| indexing throughput | docs/s | 21,725 | 24,921 | +15% |

Memory is the resident set of the one server process, the container's own
figure for OpenSearch. On an Apple M4 Max the same comparison reads higher for
both -- VeloSearch indexes 92,000 documents a second there and answers 1,391
queries a second on one client -- and the ratios are of the same shape. How
the numbers are taken, and what not to read into them, is in
[docs/performance.md](docs/performance.md); every change is held to this
repository's own last numbers by `tools/bench_gate.py`.

## What it does

| | |
|---|---|
| **Search** | every query the suite names, aggregations, sorting, three highlighters, collapse, nested and parent-join, spans and intervals, percolation, the `hybrid` query with score normalisation, point-in-time, search templates, `rank_eval`, suggesters, asynchronous search, profiling |
| **Writing** | documents, bulk, update, `_update_by_query`, `_delete_by_query` and `_reindex` -- including from another cluster over HTTP -- run as real background tasks, throttled, sliced, listed and cancellable through `_tasks` |
| **Analysis** | the built-in analyzers token for token, ICU, Japanese, Korean and Chinese by dictionary, phonetic and phone-number filters, Thai segmentation |
| **Scripting** | Painless — lexer, parser and evaluator — in every context the suite uses, plus Lucene expressions and Mustache |
| **Ingest** | the thirty processors the corpus names, grok and dissect, geoip, user-agent, text out of HTML, RTF, PDF, Word, Excel, PowerPoint, OpenDocument and EPUB, and search pipelines with request, response and phase processors |
| **Cluster** | consensus, allocation, replication, peer recovery, cross-node search, routing that narrows a search to its shards; checked in a seeded simulation and against real nodes with real partitions, two hundred fault runs of three nodes with nothing lost |
| **Security** | TLS, users and roles, API keys, document- and field-level security inside the query rather than in front of it, SAML, OIDC, LDAP, the audit log |
| **Snapshots** | filesystem, URL, S3, Google Cloud Storage and Azure repositories, and the data streams a snapshot holds come back with it |
| **Index management** | ISM policies, transitions, rollover, snapshot management, transforms, rollups and searching a rollup index |
| **Vector search** | six distance spaces, exact and HNSW, filtered search, the k-NN API |
| **SQL and PPL** | both languages, in jdbc, json, csv, raw and table shapes, with `fetch_size` paging a SQL result through a cursor |

`_cat/plugins` lists what it answers for, because a client asking whether it
may use `icu_tokenizer` deserves a true answer.

## Running it

```bash
cargo build --release
./target/release/velosearch
```

It listens on `127.0.0.1:9200`. In Docker:

```bash
docker build -t velosearch .
docker run -p 9200:9200 -v velosearch-data:/var/lib/velosearch velosearch
```

Or the built image: every commit on `main` the gates pass on is pushed to
Google Artifact Registry by `.github/workflows/image.yml`, after it has been
started and asked to write and find a document -- `:latest` is the newest
such commit, `:<sha>` any of them, and a release tag `v1.2.3` is `:1.2.3`.

```bash
docker pull asia-southeast1-docker.pkg.dev/codefin-lab/velosearch/velosearch:latest
```

The settings that matter most:

| | |
|---|---|
| `VELOSEARCH_ADDR` | where to listen (default `127.0.0.1:9200`) |
| `VELOSEARCH_DATA` | where indices live, mmapped and surviving a restart; unset keeps everything in RAM |
| `VELOSEARCH_CONFIG` | where `velosearch.yml` and the plugins' data directories live |
| `VELOSEARCH_PATH_REPO` | where filesystem snapshot repositories may live (default `<data>/repo`) |

Everything else is a setting in `config/velosearch.yml`, spelled the way
OpenSearch spells it, and readable from the environment as
`VELOSEARCH_` + the dotted name upper-cased. `docs/settings.md` lists them.

## The console

OpenSearch Dashboards is two things: a browser application and a Node server
it boots from. VeloSearch provides the server and leaves the application as
it is -- the same bundles, served from a Dashboards distribution you point it
at, talking to a VeloSearch (or OpenSearch) engine:

```bash
VELOSEARCH_CONSOLE_PATH=/usr/share/opensearch-dashboards \
VELOSEARCH_ENGINE=http://127.0.0.1:9200 \
./target/release/console
```

It listens on `127.0.0.1:5601`. Discover, Visualize, dashboards, saved
objects and their migrations, the sample data, index patterns, the Dev Tools
and the Index Management page work through it; it starts in 45 ms and holds
14 MiB against the Node server's several hundred. `docs/console.md` says how
it is built and what it does not carry.

## The dictionaries

Japanese, Korean and Chinese are read with a dictionary rather than split on
spaces, and those dictionaries are built into the binary the way OpenSearch's
kuromoji, nori and smartcn plugins carry theirs. They are most of what the
binary weighs — 188 MB with them, 20 MB without:

```bash
cargo build --release --no-default-features
```

A build without them answers everything else the same way; the three analyzers
that need them find no words.

Three more sets of data are **not** vendored, because they are somebody else's
to redistribute: the MaxMind GeoLite2 databases (`docs/geoip.md`), the
Beider-Morse rule files for the phonetic filter (`docs/phonetic.md`), and the
Ukrainian dictionary the `ukrainian` analyzer lemmatises with
(`docs/ukrainian.md`). Without them those filters say so rather than guessing.
The Polish stemmer's table is vendored, because without it there is no
stemmer at all rather than one that finds nothing (`docs/polish.md`).

## Checking a workload you already have

The interface is the same, so the check is to ask both the same questions.
`docs/upgrading.md` has the whole procedure. In short:

```bash
# what your cluster actually uses, and whether this answers all of it
python3 tools/compat_audit.py inventory --cluster $OPENSEARCH --engine $VELOSEARCH

# the same requests to both, compared answer by answer
python3 tools/compat_audit.py corpus
python3 tools/compat_audit.py replay --requests compat-corpus.ndjson \
    --a $OPENSEARCH --b $VELOSEARCH --scores
```

The first says whether anything your indices use is unanswered. The second
asks both engines the same 183 requests and diffs the JSON.

## Running the conformance suite

The suite is OpenSearch's own, so it has to be fetched:

```bash
git clone --depth 1 https://github.com/opensearch-project/OpenSearch study/OpenSearch
```

Then start the node the suites expect and run them:

```bash
tools/gate_node.sh &
python3 tools/yaml_runner.py --url http://127.0.0.1:9213 --manifest tools/phase3_manifest.json
```

`tools/gate_node.sh` is the one way to start it: the suites read back a node
attribute, the geoip databases, the phonetic rules, where a URL repository may
be read from and which clusters a reindex may read from, and a node started
without those fails sections that have nothing wrong with them.

The module suites need a second node with no ingest role, because one of them
is written against a cluster that has none — `tools/module_gate.py` runs both
passes and adds them up.

## What it is not

- **Not an OpenSearch product**, and not endorsed by the OpenSearch project. It
  implements the same HTTP API and says so.
- **Not the console's front end.** The browser application is OpenSearch
  Dashboards' own, served unchanged from a distribution; only the server
  behind it is this project's. The other plugins' server halves -- alerting,
  anomaly detection, observability -- are not written, and their pages say so.
- **Not tested at every scale.** The cluster is checked in simulation across
  ten thousand seeds and on real nodes with real partitions, and the bench
  numbers above come from one eight-core cloud machine and one laptop, not
  from a fleet.

## How it is built

- one VeloCore index per index, documents stored whole and written into views:
  tokenized for `match`, untouched for `term`, and a third for `fielddata`
- aggregations VeloCore can parse run inside it; the rest are peeled off the
  request and computed a bucket at a time through the ordinary query path
- dates are numbers, the way OpenSearch stores them: milliseconds for a `date`,
  nanoseconds for a `date_nanos`
- routing hashes the way `Murmur3HashFunction` does, so a document lands on the
  shard OpenSearch would put it on

`docs/adr/` records the decisions that were hard to reverse and why.

## The documents

| | |
|---|---|
| [CHANGELOG.md](CHANGELOG.md) | what each version provides, and how it is measured |
| [docs/settings.md](docs/settings.md) | every setting, the server's and the console's |
| [docs/upgrading.md](docs/upgrading.md) | moving an existing workload onto it, and moving between versions of this |
| [docs/console.md](docs/console.md) | the console's server: what it serves, what it pins, what it leaves out |
| [docs/geoip.md](docs/geoip.md), [docs/phonetic.md](docs/phonetic.md), [docs/ukrainian.md](docs/ukrainian.md) | the three that read data this does not ship |
| [docs/polish.md](docs/polish.md) | Polish, and the stemmer table this one does ship |
| [docs/performance.md](docs/performance.md) | both engines measured on the same machine, and what to read into it |
| [docs/velocore.md](docs/velocore.md) | what was changed in the fork of tantivy, and why |
| [docs/limits.md](docs/limits.md) | what a node refuses to spend: its memory, its concurrency and its time |
| [docs/adr/](docs/adr/) | the nine decisions that were hard to reverse |
| [CONTRIBUTING.md](CONTRIBUTING.md) | where things are, and what to run before you push |
| [CONTEXT.md](CONTEXT.md) | what the words mean |

## Licence

Dual licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option. Unless you say otherwise, any contribution you send in is
licensed the same way, with no further conditions.

VeloCore, the engine underneath, is MIT, as the tantivy it forked is. The
Snowball stemmers for Catalan, Basque, Irish, Lithuanian, Estonian and
Armenian, and the original Porter algorithm, are generated from the Snowball
project's own definitions by its compiler and used under the BSD 3-clause
licence in `LICENSE-SNOWBALL`.

A few read paths answer with a catalogue that describes the OpenSearch
interface itself, and those catalogues are OpenSearch's own text, taken from a
3.8.0 node and used under the Apache 2.0 licence the project publishes them
under: the machine-learning tool descriptors in
`src/api/plugins/ml_tools.json`, the workflow step catalogue in
`src/api/flow_framework.rs`, the security-analytics rule categories in
`src/api/plugins/security_analytics.rs` and the notification channel types in
`src/api/notifications.rs`. Each one names what a request field may say, so
matching it is what makes a client's own validation come out the same here as
it does there.
