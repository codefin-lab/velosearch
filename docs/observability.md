# Watching a node

A node that refuses work nobody can see it refusing is a node nobody is
watching. [limits.md](limits.md) gave this one three ceilings -- the memory a
request may take, how many run at once, how long one may walk -- and every one
of them is a refusal an operator needs to know about before a client tells
them. This is where those refusals, and everything else the node knows about
itself, can be read from.

## The metrics: `GET /_prometheus/metrics`

The numbers are the ones `_nodes/stats` and `_cluster/health` already answer.
They are not worked out a second time here: the endpoint asks the node for its
own statistics the way any caller would and renders them, so there is one
source of truth in two formats.

The names are the ones OpenSearch's `prometheus-exporter` plugin publishes --
`opensearch_` and all -- so a Grafana dashboard written for OpenSearch reads
this node without being rewritten. Every series carries `cluster`, `node` and
`nodeid`.

```
$ curl -s localhost:9200/_prometheus/metrics | head -4
# HELP velosearch_build_info The build this node is running, as labels.
# TYPE velosearch_build_info gauge
velosearch_build_info{cluster="velosearch",node="n1",nodeid="...",version="0.1.0",opensearch_version="3.9.0",build_hash="..."} 1
```

What is there:

| | |
|---|---|
| `opensearch_cluster_status` | 0 green, 1 yellow, 2 red -- with the node, shard and pending-task counts beside it |
| `opensearch_os_*`, `opensearch_process_*`, `opensearch_fs_total_*` | the machine, the process and the disk |
| `opensearch_jvm_mem_heap_used_bytes` | there is no JVM; this is the memory the allocator holds for the node's own data, which is what a heap gauge is watched for |
| `opensearch_threadpool_threads_number`, `_tasks_number{type="active"\|"queue"}` | what each pool is running and holding back |
| `opensearch_threadpool_tasks_count{type="rejected"}` | **what a pool refused.** A number climbing here is a node past what it can carry |
| `opensearch_circuitbreaker_{estimated,limit}_bytes`, `_tripped_count` | what each breaker holds, may hold, and has refused |
| `opensearch_indices_*` | documents, store size, writes, queries, refreshes, merges, segments, translog |
| `opensearch_index_*{index="..."}` | the same per index; `?indices=false` leaves them out for a node holding too many to name |

The two worth alerting on, because they are the node saying it is past its
limit rather than a number that is merely large:

```
rate(opensearch_threadpool_tasks_count{type="rejected"}[5m]) > 0
increase(opensearch_circuitbreaker_tripped_count[5m]) > 0
```

A scrape costs what `_nodes/stats` costs: a reading of the operating system
and a walk of the open indices.

The endpoint is a route like any other, so the security plugin decides who may
reach it. It is judged as what it is -- a read of the cluster's statistics,
`cluster:monitor/stats` -- so a role that already grants monitoring reaches it
and a caller with no permissions does not:

```yaml
scraper:
  cluster_permissions:
    - "cluster:monitor/stats"
```

## The log

It was `WARN`, compiled in, with nothing that could change it -- so the one
thing wanted at the moment something is wrong, more detail, needed a rebuild
and a redeployment to get.

| | |
|---|---|
| `VELOSEARCH_LOG` | a filter in the usual form: a level (`info`), or per-module levels (`warn,velosearch::cluster=debug,velocore=info`). `RUST_LOG` is read if it is not set, because that is what anyone will try first. Default `warn`. |
| `VELOSEARCH_LOG_FORMAT` | `json` writes each line as an object -- timestamp, level, target, message and fields -- for a collector that would otherwise parse prose. Anything else is the readable form. |

```
$ VELOSEARCH_LOG=info VELOSEARCH_LOG_FORMAT=json velosearch
{"timestamp":"...","level":"INFO","fields":{"message":"restored [shop] from [r1:s1] with 1 documents"},"target":"velosearch::api::snapshot"}
```

A filter that cannot be parsed leaves the default in place and says so on
standard error, rather than starting the node silent or refusing to start it.

The slow logs are separate and unchanged: they are per index, written to
`path.logs`, and configured with the reference's
`index.search.slowlog.threshold.*` settings.

## How this is checked

`tools/metrics_check.py` starts nodes, scrapes the endpoint and parses it the
way Prometheus would -- a family with no type, a series with no family, a
value that is not a number and a label that broke out of its quotes are each a
scrape that fails in Prometheus and passes a test that only grepped for a word
-- then makes the node do things and reads the numbers back:

```
$ tools/metrics_check.py
  23 checks over the exporter, its numbers and the log

RESULT the node's numbers are readable by a scraper, and the log by an operator
```

Among them: a document written moves the document gauge, a search moves the
query counter, a breaker that refused moves its tripped counter, a pool that
refused moves its rejected counter, `?indices=false` drops the per-index
series and keeps the rest, the log writes json when asked, a node told nothing
stays as quiet as it was, and a filter naming one module raises that module.
