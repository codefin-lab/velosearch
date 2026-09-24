# Changelog

## Versioning

Two numbers matter to somebody running this, and they are not the same number.

- **The VeloSearch version** is this project's own, and follows semantic
  versioning: a patch fixes something, a minor adds something, a major changes
  an answer somebody could have depended on.
- **The OpenSearch version it answers as** is what `GET /` reports, and is the
  version of the API this speaks. It moves when the API this targets moves,
  which is a different event from a release of this project.

An index written by one version is read by the next, and two adjacent versions
can be in one cluster at once — which is what makes a rolling upgrade
possible. [docs/upgrading.md](docs/upgrading.md) is the procedure, and
downgrading is not one of the things it can do.

## Unreleased

The first release has not been cut. VeloSearch implements the OpenSearch REST
API and is checked with OpenSearch's own tests; every number below is produced
by a script in `tools/`, and [README.md](README.md) says which.

### How it is measured

- **1,428 of 1,428** non-skipped sections of OpenSearch's core conformance
  suite, over all 410 files of it
- **880 of 890** sections of its module and plugin suites. Of the ten that do
  not pass, the stempel and Ukrainian analysis sections need dictionaries that
  are not redistributed here, and one asserts that its plugin is the only one
  installed, which a single binary cannot be
- **166 of 183** canonical requests answered identically to OpenSearch 3.1.0
  once ids and timings are scrubbed (`tools/compat_audit.py replay`)
- **167 of 167** REST APIs routed on every path and method their spec names
  (`tools/endpoint_gate.py`)
- **33 of 33** checks of the three ceilings a node is held to -- the memory a
  request may take, how many requests it runs at once, and how long one may
  walk (`tools/limits_check.py`, [docs/limits.md](docs/limits.md))
- **23 of 23** checks of what an operator can see: the Prometheus endpoint
  parsed the way a scraper parses it, its numbers moved by writing, searching
  and being refused, and the log turned up without a rebuild
  (`tools/metrics_check.py`, [docs/observability.md](docs/observability.md))
- **quicker or lighter on all 34 dimensions**, measured beside OpenSearch 3.1.0
  on the same Google Compute Engine `n2-standard-8`, with the same corpus and
  the same client ([docs/performance.md](docs/performance.md))
- **200 of 200** three-node fault runs -- isolations, restarts, pauses and
  heals under a write load -- with no acknowledged write lost
  (`tools/cluster_chaos.py`)
- **146 of 166** cases of OpenSearch Dashboards' own API suite against the
  console's server, none failed that the Node server passes; 14 MiB resident,
  ready in 45 ms ([docs/console.md](docs/console.md))

### What it provides

The search API, the analyzers, Painless, ingest, security including document-
and field-level, a cluster with replication and recovery, snapshots, index
management, vector search, SQL and PPL, and a server for OpenSearch
Dashboards' browser application. Recently added:

- highlighting with fragments, and the `fvh` highlighter
- span and interval queries
- percolation, with document slots and highlights
- the `hybrid` query with score normalisation
- search pipelines with request, response and phase processors
- `_update_by_query`, `_delete_by_query` and `_reindex` as background tasks:
  throttled, sliced, listed and cancellable through `_tasks`
- asynchronous search
- transforms and rollups, including searching a rollup index
- data streams created from index templates, and carried through a snapshot:
  one taken of a stream records it, and a restore puts the stream back in
  front of the backing indices it brings home
- snapshot management policies: `_plugins/_sm/policies`, with the schedule
  that takes a snapshot, the condition that throws old ones away, and
  `_explain` to say where each half has got to
- `fetch_size` on a SQL query, which pages the result through a cursor
- attachment extraction from HTML, RTF, PDF, Office, OpenDocument and EPUB
- routing that narrows a search to the shards it names
- service accounts and on-behalf-of tokens
- scheduled refresh
- weighted routing and decommissioning by awareness attribute, kept in cluster
  metadata; the dangling-index, remote-store and stored-task-result endpoints,
  each answering as a node without the feature answers
- `_nodes` and `_cluster/stats` narrowed to the nodes and metrics a path names
- the console is VeloSearch's: its name, its wordmark, its mark and its
  favicon come out of the branding block the front end already reads, and its
  colours out of the theme itself. The marks are compiled into the binary, so
  there is nothing a deployment can forget; the green that is text is a darker
  one than the green that is the mark, because the mark's reads at 2.3 against
  white. `VELOSEARCH_CONSOLE_BRANDING=opensearch` leaves the distribution's own
  in place ([docs/console.md](docs/console.md))
- the console's theme is moved rather than skinned: every stylesheet it serves
  is read on the way out and the shades that are the primary blue -- a hue
  between 197 and 210 at a saturation of 0.55 or more, which is where all of
  them and none of the blue-greys, the danger red or the chart palette fall --
  are replaced. Each replacement keeps the colour's relative luminance, so
  every contrast ratio the theme was built with comes out unchanged; the
  primary `#0268BC` lands two units from the brand's own `#00753C`. Each
  stylesheet is transformed once and kept ([docs/console.md](docs/console.md))
- circuit breakers that refuse rather than only report: an aggregating search
  is given a budget out of `indices.breaker.request.limit` and held to it, a
  body is counted against the in-flight breaker, and the parent breaker reads
  the memory the process actually holds -- consulted before a request is
  admitted and again by a search already walking
- bounded thread pools: each runs so many requests at once, queues what it
  can, and answers `429 rejected_execution_exception` when the queue is full,
  so a node under more load than it can carry refuses some of it quickly
  rather than accepting all of it slowly
- `timeout` on a search, enforced: the walk reads the deadline as it goes and
  answers with what it had, `"timed_out": true`, and
  `search.default_search_timeout` sets one for the searches that ask for none
- `GET /_prometheus/metrics`: the numbers `_nodes/stats` and `_cluster/health`
  already answer, rendered in the exposition format under the metric names
  OpenSearch's exporter plugin publishes, so a dashboard written for
  OpenSearch reads this node unchanged -- including what each pool refused and
  what each breaker holds
- a log that can be turned up without a rebuild: `VELOSEARCH_LOG` takes a
  filter (per module, as usual) and `VELOSEARCH_LOG_FORMAT=json` writes lines
  a collector can read. It was `WARN`, compiled in
- the dependencies are checked rather than trusted: `cargo deny` gates
  advisories, licences and where each crate came from on every run, the
  release publishes a bill of materials beside each binary, and the images are
  scanned before they are pushed and signed with the workflow's own identity
- the tag and the manifest have to agree on the version before a release
  builds anything, and `--version` reports this project's version, the
  OpenSearch version it answers as, and the commit
