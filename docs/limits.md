# What a node refuses

A search engine that accepts everything it is given is not slower than one
that refuses some of it -- it is less available. The requests it accepted all
get slower together, the memory they hold between them is more than the
machine has, and the kernel picks one process to kill. What the operator sees
is a node that was answering and then was not, with nothing in the log that
says why.

So a node has three ceilings, and this is what each one is, what a request
past it is told, and how each is checked. The settings themselves are in
[settings.md](settings.md); the numbers below are the defaults.

## The memory: the circuit breakers

`_nodes/stats` reported four breakers long before anything enforced them: the
limits were computed from the machine for the answer, `tripped` was the
literal `0`, and a request larger than the node could hold was not refused
anywhere. The four are now accounted, and three of them refuse.

| breaker | what it holds | default limit |
|---|---|---|
| `request` | what the aggregations of the searches running at this moment may build | 60% of the machine |
| `in_flight_requests` | the bodies being read at this moment, at the reference's overhead of 2 | 100% |
| `parent` | everything, the allocator's own memory included | 95% |
| `fielddata` | nothing: there is no fielddata cache in this engine | 40% |

**An aggregating search is given a budget.** It reserves a share of the
`request` breaker before it runs and holds it until its answer is written, and
that share is the ceiling VeloCore is held to while it collects. The share is
the breaker's limit divided by how many searches the node runs at once, so
however many arrive, what they hold between them cannot pass the limit -- and
the search that arrives with nothing left is refused rather than admitted:

```
$ curl -s localhost:9200/logs/_search -d '{"size":0,"aggs":{...}}'
{"error":{"type":"circuit_breaking_exception",
          "reason":"[request] Data too large, data for [<agg [search]>] ...",
          "bytes_wanted":...,"bytes_limit":...,"durability":"TRANSIENT"},
 "status":429}
```

A search that was admitted and then runs past its budget is refused the same
way, by the engine reaching the ceiling rather than by the node handing one
out. Either way it is a 429 a client can back off from, not a dead node.

**A body is held against `in_flight_requests`.** `http.max_content_length`
bounds one body; it cannot see a hundred of them arriving together, which is
10GB of a 100MB limit before a single document is indexed. The breaker counts
the bodies being read at this moment and refuses the one that does not fit.

**The parent breaker reads the allocator.** Most of what a node holds was
never reserved by anything -- index buffers, merges, the readers a search
opens -- so a breaker that counted only its own reservations would be a
number that is always small on a node that is always full. `use_real_memory`
is what the reference calls reading the process instead, and here it reads
what mimalloc has committed. It is consulted before a search or a write is
admitted, and again by a search that is already walking: a walk that runs into
a node past its parent limit gives up and is refused, rather than finishing
and taking the node with it.

Raising a limit takes effect on the next request:

```
PUT _cluster/settings
{"transient": {"indices.breaker.request.limit": "80%"}}
```

## The concurrency: the pools

Every request used to run the moment it arrived, however many were already
running. `_nodes/stats` counted them into the pool OpenSearch would have given
them -- `search`, `write`, `get` -- and the `queue` column was 0 because
nothing queued, not because nothing waited.

Each bounded pool now has a number of requests it runs at once and a queue for
the ones waiting. A request of that kind takes a place before it reaches its
handler and gives it back when the answer is written; with the pool full and
the queue full, it is refused:

```
{"error":{"type":"rejected_execution_exception",
          "reason":"rejected execution of coordinating operation
                    [thread_pool_name = search, queue_capacity = 1000, ...]"},
 "status":429}
```

How many at once is not OpenSearch's thread count. A thread there is busy for
the whole of a request; a request here gives its worker up whenever it waits
for a disk or another node, so the default is twice the size the reference
gives the pool. The queues are the reference's: 1,000 for `search`, 10,000 for
`write`, 1,000 for `get`, 16 for `analyze`.

The pools are separate, which is the point of having more than one: a node
refusing searches is still taking writes.

## The time: the clock

`timeout` was read out of a search, checked for being a time value, and then
ignored. Every answer said `"timed_out": false` because the field was written
rather than measured, and a query that walked a billion documents walked all
of them, whatever the caller -- or the gateway in front of it -- was prepared
to wait.

A search now carries the deadline it asked for, and the walk reads it: every
few thousand documents, and again before each segment. Past the deadline it
stops collecting, and the answer is what it had gathered by then, with
`"timed_out": true` -- which is what OpenSearch answers, and what a dashboard
behind a thirty-second gateway needs it to do.

```
$ curl -s 'localhost:9200/logs/_search?timeout=50ms' -d '{...}'
{"took":51,"timed_out":true,"hits":{...}}
```

The deadline can be in the query string or in the body. A cluster that wants
one for the searches that ask for nothing sets
`search.default_search_timeout`; a request's own stands over it, and `-1` asks
for none. A node answering a coordinator reports its own clock, so a search
across a cluster says it timed out if any node it reached did.

## How this is checked

`tools/limits_check.py` starts nodes, puts each ceiling in a state an operator
would reach under load, and reads what the node answers -- 33 checks over the
three:

```
$ tools/limits_check.py
  33 checks over the clock, the breakers and the pools

RESULT every ceiling held: the memory, the concurrency and the time
```

It checks what each refusal says as well as that it happened: the type, the
status, the bytes named in a breaker's refusal, that the reservation is given
back afterwards, that a refusal is counted in `tripped` and in the pool's
`rejected`, that a search which does not aggregate is not refused by a full
`request` breaker, that a write is not refused by a pool full of searches, and
that the node is healthy after all of it.
