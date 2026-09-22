#!/usr/bin/env python3
"""What a node refuses to spend: its memory, its concurrency and its time.

Three ceilings a node has to have before it can be left alone with real
traffic, and none of them could be checked before they existed:

  the breakers   `_nodes/stats` reported four of them with limits derived from
                 the machine, `tripped` as the literal 0 and nothing consulting
                 either. A search that asked for more memory than the node had
                 did not fail -- the node died. Here an aggregating search is
                 given a budget out of the `request` breaker, a body is held
                 against `in_flight_requests`, and both refuse with the
                 reference's `circuit_breaking_exception` and 429.

  the pools      every request ran the moment it arrived, however many were
                 already running, so a node under more load than it could carry
                 answered all of it slowly instead of refusing some of it
                 quickly. Here each bounded pool runs so many at once, queues
                 what it can, and answers `rejected_execution_exception` when
                 the queue is full -- which a client's back-off can read.

  the clock      `timeout` was parsed, checked, and ignored: every answer said
                 `"timed_out": false` because the field was written rather than
                 measured, and a query with no end walked to the end of the
                 index. Here the walk reads the deadline as it goes and answers
                 with what it had when the time ran out.

Every check is made rather than asserted from a table: the node is started, put
in the state, asked, and its answer read.

    tools/limits_check.py             # starts the nodes it needs
    tools/limits_check.py --url http://127.0.0.1:9200   # only what one node
                                                        # already running can
                                                        # be asked
"""

import argparse
import concurrent.futures
import json
import os
import pathlib
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

ROOT = pathlib.Path(__file__).resolve().parent.parent
DOCS = 50_000


def call(url, method, path, body=None, ndjson=None, timeout=60):
    if ndjson is not None:
        data, kind = ndjson.encode(), "application/x-ndjson"
    elif body is not None:
        data, kind = json.dumps(body).encode(), "application/json"
    else:
        data, kind = None, "application/json"
    req = urllib.request.Request(
        url + path, data=data, method=method, headers={"content-type": kind}
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout) as answer:
            return answer.status, json.loads(answer.read() or b"{}")
    except urllib.error.HTTPError as e:
        raw = e.read()
        try:
            return e.code, json.loads(raw or b"{}")
        except json.JSONDecodeError:
            return e.code, {"raw": raw[:200].decode("utf-8", "replace")}
    except Exception as e:
        return 0, {"no answer": str(e)[:160]}


def start_node(binary, port, transport, env_extra):
    data = tempfile.mkdtemp(prefix="velo-limits-")
    env = dict(os.environ)
    env.update(
        {
            "VELOSEARCH_ADDR": f"127.0.0.1:{port}",
            "VELOSEARCH_DATA": data,
            "VELOSEARCH_TRANSPORT_PORT": str(transport),
        }
    )
    env.update(env_extra)
    log = open(pathlib.Path(data) / "node.log", "w")
    node = subprocess.Popen([binary], env=env, stdout=log, stderr=subprocess.STDOUT)
    url = f"http://127.0.0.1:{port}"
    for _ in range(60):
        status, _ = call(url, "GET", "/")
        if status:
            time.sleep(1)  # the node elects itself a moment after it listens
            return node, data, url
        if node.poll() is not None:
            break
        time.sleep(1)
    node.kill()
    print(f"the node did not start; its log is in {data}/node.log")
    sys.exit(2)


def stop_node(node, data):
    if node is None:
        return
    node.send_signal(signal.SIGTERM)
    try:
        node.wait(timeout=10)
    except subprocess.TimeoutExpired:
        node.kill()
    shutil.rmtree(data, ignore_errors=True)


def corpus(url, index="limits", docs=DOCS):
    """An index with enough in it that a search over it is work."""
    call(url, "DELETE", f"/{index}")
    call(
        url,
        "PUT",
        f"/{index}",
        {"mappings": {"properties": {"n": {"type": "integer"}, "k": {"type": "keyword"}}}},
    )
    lines = []
    for i in range(docs):
        lines.append(json.dumps({"index": {"_index": index}}))
        lines.append(json.dumps({"n": i, "k": f"k{i % 2000}"}))
    status, answer = call(url, "POST", "/_bulk", ndjson="\n".join(lines) + "\n", timeout=300)
    if status != 200 or answer.get("errors"):
        print(f"the corpus would not go in: {status}")
        sys.exit(2)
    call(url, "POST", f"/{index}/_refresh")


def transiently(url, settings):
    call(url, "PUT", "/_cluster/settings", {"transient": settings})


def breakers(url):
    status, answer = call(url, "GET", "/_nodes/stats/breaker")
    if status != 200:
        return {}
    return list(answer["nodes"].values())[0]["breakers"]


def pool_stats(url, name):
    status, answer = call(url, "GET", "/_nodes/stats/thread_pool")
    if status != 200:
        return {}
    return list(answer["nodes"].values())[0]["thread_pool"].get(name, {})


class Checks:
    """What was asked, and what of it was wrong."""

    def __init__(self):
        self.made = 0
        self.bad = []

    def that(self, what, ok, saw=None):
        self.made += 1
        if not ok:
            self.bad.append(f"{what}: {saw}")


# ----------------------------------------------------------------- the clock


def the_clock(url, c):
    """A search is held to the time it was given."""
    agg = {
        "size": 0,
        "aggs": {"by": {"terms": {"field": "k", "size": 2000, "order": {"_key": "asc"}}}},
    }
    status, answer = call(url, "POST", "/limits/_search", {"size": 1, "query": {"match_all": {}}})
    c.that(
        "a search with time to spare does not say it ran out",
        status == 200 and answer.get("timed_out") is False,
        f"{status} {answer.get('timed_out')}",
    )

    # a deadline already past: the walk stops at the first thing it reads
    status, answer = call(url, "POST", "/limits/_search?timeout=1nanos", {"size": 1})
    c.that(
        "a deadline already past is reported as one",
        status == 200 and answer.get("timed_out") is True,
        f"{status} {answer.get('timed_out')}",
    )
    c.that(
        "a search that ran out of time still answers the shape of a search",
        status == 200 and "hits" in answer and "_shards" in answer,
        sorted(answer.keys()),
    )

    # the deadline in the body, which is where a client that is not writing a
    # URL puts it
    status, answer = call(url, "POST", "/limits/_search", dict(agg, timeout="1nanos"))
    c.that(
        "a deadline in the body is read as well",
        status == 200 and answer.get("timed_out") is True,
        f"{status} {answer.get('timed_out')}",
    )
    c.that(
        "an aggregation that ran out of time answers what it had",
        status == 200 and answer.get("aggregations", {}).get("by", {}).get("buckets") == [],
        json.dumps(answer.get("aggregations", {}))[:120],
    )

    # a real deadline over real work: the answer comes back near it rather
    # than when the walk happens to end
    slow = {
        "size": 0,
        "query": {"match_all": {}},
        "aggs": {
            "by": {
                "terms": {"field": "k", "size": 2000, "order": {"_key": "asc"}},
                "aggs": {"n": {"stats": {"field": "n"}}},
            }
        },
    }
    started = time.monotonic()
    status, answer = call(url, "POST", "/limits/_search?timeout=1ms", slow)
    took = time.monotonic() - started
    c.that(
        "a deadline of a millisecond is answered in well under a second",
        status == 200 and took < 1.0,
        f"{status} in {took:.3f}s",
    )

    # what the cluster asks of the searches that ask for nothing
    transiently(url, {"search.default_search_timeout": "1nanos"})
    status, answer = call(url, "POST", "/limits/_search", {"size": 1})
    c.that(
        "a cluster-wide deadline reaches a search that named none",
        status == 200 and answer.get("timed_out") is True,
        f"{status} {answer.get('timed_out')}",
    )
    transiently(url, {"search.default_search_timeout": None})
    status, answer = call(url, "POST", "/limits/_search", {"size": 1})
    c.that(
        "taking the cluster-wide deadline away takes it away",
        status == 200 and answer.get("timed_out") is False,
        f"{status} {answer.get('timed_out')}",
    )

    # a request that asks for no deadline is not given one
    status, answer = call(url, "POST", "/limits/_search?timeout=-1", {"size": 1})
    c.that(
        "`-1` is how a request says it wants no deadline",
        status == 200 and answer.get("timed_out") is False,
        f"{status} {answer.get('timed_out')}",
    )


# -------------------------------------------------------------- the breakers


def the_breakers(url, c):
    """A node refuses the memory it does not have."""
    agg = {"size": 0, "aggs": {"by": {"terms": {"field": "k", "size": 2000}}}}

    reported = breakers(url)
    c.that(
        "every breaker the reference reports is reported",
        set(reported) == {"request", "fielddata", "in_flight_requests", "parent"},
        sorted(reported),
    )
    c.that(
        "the parent breaker counts the memory the node actually holds",
        reported.get("parent", {}).get("estimated_size_in_bytes", 0) > 0,
        reported.get("parent", {}).get("estimated_size_in_bytes"),
    )

    # a breaker with nothing left refuses the search that would spend it
    before = reported["request"]["tripped"]
    transiently(url, {"indices.breaker.request.limit": "0b"})
    status, answer = call(url, "POST", "/limits/_search", agg)
    error = answer.get("error", {})
    c.that(
        "an aggregating search with no room is refused",
        status == 429 and error.get("type") == "circuit_breaking_exception",
        f"{status} {error.get('type')}",
    )
    c.that(
        "the refusal says what was asked for and what was allowed",
        "bytes_wanted" in error and "bytes_limit" in error and "durability" in error,
        sorted(error),
    )
    c.that(
        "the refusal is counted against the breaker that made it",
        breakers(url)["request"]["tripped"] > before,
        breakers(url)["request"]["tripped"],
    )

    # and the search that does not aggregate is not touched by it
    status, answer = call(url, "POST", "/limits/_search", {"size": 1})
    c.that(
        "a search that builds nothing is not refused by the request breaker",
        status == 200,
        status,
    )

    # a budget too small to finish in stops the aggregation where it is
    transiently(url, {"indices.breaker.request.limit": "4kb"})
    status, answer = call(url, "POST", "/limits/_search", agg)
    error = answer.get("error", {})
    c.that(
        "an aggregation that runs past its budget is refused, not answered",
        status == 429 and error.get("type") == "circuit_breaking_exception",
        f"{status} {error.get('type')}",
    )

    # raising the limit is enough: the same request is answered
    transiently(url, {"indices.breaker.request.limit": None})
    status, answer = call(url, "POST", "/limits/_search", agg)
    c.that(
        "the same search is answered once the breaker has room again",
        status == 200 and answer.get("aggregations", {}).get("by", {}).get("buckets"),
        status,
    )
    c.that(
        "what a finished search reserved is given back",
        breakers(url)["request"]["estimated_size_in_bytes"] == 0,
        breakers(url)["request"]["estimated_size_in_bytes"],
    )

    # a body larger than what the node has for bodies
    big = "\n".join(
        json.dumps(x)
        for i in range(2000)
        for x in ({"index": {"_index": "limits"}}, {"n": i, "k": "big"})
    )
    transiently(url, {"network.breaker.inflight_requests.limit": "1kb"})
    status, answer = call(url, "POST", "/_bulk", ndjson=big + "\n")
    error = answer.get("error", {})
    c.that(
        "a body larger than the node has room for is refused",
        status == 429 and error.get("type") == "circuit_breaking_exception",
        f"{status} {error.get('type')}",
    )
    transiently(url, {"network.breaker.inflight_requests.limit": None})
    status, answer = call(url, "POST", "/_bulk", ndjson=big + "\n")
    c.that(
        "the same body is taken once there is room for it",
        status == 200 and not answer.get("errors"),
        status,
    )
    c.that(
        "what a finished body reserved is given back",
        breakers(url)["in_flight_requests"]["estimated_size_in_bytes"] == 0,
        breakers(url)["in_flight_requests"]["estimated_size_in_bytes"],
    )

    # a node that refused is a node that is still there
    status, answer = call(url, "GET", "/_cluster/health")
    c.that(
        "the node is healthy after everything it refused",
        status == 200 and answer.get("status") in ("green", "yellow"),
        f"{status} {answer.get('status')}",
    )


# ----------------------------------------------------------------- the pools


def flood(url, path, body, how_many):
    """`how_many` requests at once; what each was answered."""
    with concurrent.futures.ThreadPoolExecutor(max_workers=how_many) as pool:
        futures = [
            pool.submit(call, url, "POST", path, body, None, 120) for _ in range(how_many)
        ]
        return [f.result() for f in futures]


def the_pools(url, c, bounded):
    """A node refuses the work it cannot get to.

    `bounded` says the node was started with one place for a search and no
    queue: the state an operator would reach under load, reached here by
    configuration rather than by finding enough load.
    """
    agg = {
        "size": 0,
        "aggs": {
            "by": {
                "terms": {"field": "k", "size": 2000, "order": {"_key": "asc"}},
                "aggs": {"n": {"stats": {"field": "n"}}},
            }
        },
    }
    before = pool_stats(url, "search").get("rejected", 0)
    answers = flood(url, "/limits/_search", agg, 16)
    codes = [s for s, _ in answers]
    refused = [b for s, b in answers if s == 429]
    if bounded:
        c.that(
            "a flood larger than the node will run at once is partly refused",
            429 in codes and 200 in codes,
            sorted(set(codes)),
        )
        c.that(
            "what was refused is refused the way the reference refuses it",
            refused
            and all(
                b.get("error", {}).get("type") == "rejected_execution_exception"
                for b in refused
            ),
            {b.get("error", {}).get("type") for b in refused},
        )
        c.that(
            "the pool counts what it refused",
            pool_stats(url, "search").get("rejected", 0) >= before + len(refused),
            pool_stats(url, "search"),
        )
        c.that(
            "what was not refused was answered",
            all(
                b.get("aggregations", {}).get("by", {}).get("buckets")
                for s, b in answers
                if s == 200
            ),
            codes,
        )
    else:
        c.that(
            "a node with a queue in front of its pool answers the whole flood",
            set(codes) == {200},
            sorted(set(codes)),
        )

    stats = pool_stats(url, "search")
    c.that(
        "the pool is empty again once the flood is over",
        stats.get("active") == 0 and stats.get("queue") == 0,
        stats,
    )
    c.that(
        "the pool counted what it ran",
        stats.get("completed", 0) >= len([s for s in codes if s == 200]),
        stats,
    )

    # the pools are separate: a flood of searches does not refuse a write
    line = json.dumps({"index": {"_index": "limits"}})
    doc = json.dumps({"n": 1, "k": "written under load"})
    with concurrent.futures.ThreadPoolExecutor(max_workers=17) as pool:
        searches = [
            pool.submit(call, url, "POST", "/limits/_search", agg, None, 120) for _ in range(16)
        ]
        write = pool.submit(call, url, "POST", "/_bulk", None, f"{line}\n{doc}\n", 120)
        status, answer = write.result()
        for f in searches:
            f.result()
    c.that(
        "a write is not refused by a pool full of searches",
        status == 200 and not answer.get("errors"),
        f"{status} {answer.get('errors')}",
    )


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--url", default="", help="a node already running; the pool section is "
                                              "then only run if that node is bounded")
    ap.add_argument("--binary", default=str(ROOT / "target" / "release" / "velosearch"))
    ap.add_argument("--port", type=int, default=9271)
    ap.add_argument("--transport", type=int, default=9371)
    ap.add_argument("--docs", type=int, default=DOCS)
    args = ap.parse_args()

    c = Checks()
    node = data = None
    try:
        # the clock and the breakers, on a node sized as a node is
        if args.url:
            url = args.url
        else:
            node, data, url = start_node(args.binary, args.port, args.transport, {})
        corpus(url, docs=args.docs)
        the_clock(url, c)
        the_breakers(url, c)
        the_pools(url, c, bounded=False)
        stop_node(node, data)
        node = data = None

        # and the pools, on a node with one place and nowhere to wait: the
        # state a node under load reaches, without needing the load
        if not args.url:
            node, data, url = start_node(
                args.binary,
                args.port + 1,
                args.transport + 1,
                {
                    "VELOSEARCH_THREAD_POOL_SEARCH_SIZE": "1",
                    "VELOSEARCH_THREAD_POOL_SEARCH_QUEUE_SIZE": "0",
                },
            )
            corpus(url, docs=args.docs)
            the_pools(url, c, bounded=True)
    finally:
        stop_node(node, data)

    print(f"  {c.made} checks over the clock, the breakers and the pools")
    for row in c.bad:
        print(f"    {row}")
    if c.bad:
        print("\nRESULT a ceiling did not hold")
        return 1
    print("\nRESULT every ceiling held: the memory, the concurrency and the time")
    return 0


if __name__ == "__main__":
    sys.exit(main())
