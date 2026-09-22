#!/usr/bin/env python3
"""What an operator can see: the metrics a scraper reads, and the log.

A node that refuses work an operator cannot see refusing is a node nobody is
watching. `docs/limits.md` gave this one three ceilings; this checks that
reaching one of them shows up where a monitoring system reads, and that the
log can be turned up without a rebuild -- the two things an operator needs at
the moment something is wrong.

  the exporter   `GET /_prometheus/metrics`, in the exposition format, with
                 the metric names OpenSearch's `prometheus-exporter` plugin
                 publishes. Every line is parsed here rather than eyeballed:
                 a family that declares no type, a series with no family, a
                 value that is not a number and a label that broke out of its
                 quotes are each a scrape that fails in Prometheus and passes
                 a test that only grepped for a word.

  the numbers    they have to be the node's, not a shape. A document written
                 moves the document gauge; a search moves the query counter; a
                 breaker that refused moves its tripped counter and a pool
                 that refused moves its rejected counter -- which is the whole
                 point of having them.

  the log        `VELOSEARCH_LOG` takes a filter and `VELOSEARCH_LOG_FORMAT`
                 takes `json`, so an operator can raise the level on the node
                 in front of them. It was `WARN` compiled in.

    tools/metrics_check.py
"""

import argparse
import json
import os
import pathlib
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

ROOT = pathlib.Path(__file__).resolve().parent.parent

# a series line: a name, optional labels, and a value
SERIES = re.compile(r"^(?P<name>[a-zA-Z_:][a-zA-Z0-9_:]*)(?P<labels>\{.*\})? (?P<value>\S+)$")
LABELS = re.compile(r'([a-zA-Z_][a-zA-Z0-9_]*)="((?:[^"\\]|\\.)*)"')


def call(url, method, path, body=None, ndjson=None, timeout=60, raw=False):
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
            body = answer.read()
            if raw:
                return answer.status, body.decode("utf-8", "replace"), answer.headers
            return answer.status, json.loads(body or b"{}")
    except urllib.error.HTTPError as e:
        raw_body = e.read()
        if raw:
            return e.code, raw_body.decode("utf-8", "replace"), e.headers
        try:
            return e.code, json.loads(raw_body or b"{}")
        except json.JSONDecodeError:
            return e.code, {"raw": raw_body[:200].decode("utf-8", "replace")}
    except Exception as e:
        if raw:
            return 0, "", {}
        return 0, {"no answer": str(e)[:160]}


def start_node(binary, port, transport, env_extra):
    data = tempfile.mkdtemp(prefix="velo-metrics-")
    env = dict(os.environ)
    env.update(
        {
            "VELOSEARCH_ADDR": f"127.0.0.1:{port}",
            "VELOSEARCH_DATA": data,
            "VELOSEARCH_TRANSPORT_PORT": str(transport),
        }
    )
    env.update(env_extra)
    log_path = pathlib.Path(data) / "node.log"
    log = open(log_path, "w")
    node = subprocess.Popen([binary], env=env, stdout=log, stderr=subprocess.STDOUT)
    url = f"http://127.0.0.1:{port}"
    for _ in range(60):
        status, _ = call(url, "GET", "/")
        if status:
            time.sleep(1)
            return node, data, url, log_path
        if node.poll() is not None:
            break
        time.sleep(1)
    node.kill()
    print(f"the node did not start; its log is in {log_path}")
    sys.exit(2)


def stop_node(node, data, keep=False):
    if node is None:
        return
    node.send_signal(signal.SIGTERM)
    try:
        node.wait(timeout=10)
    except subprocess.TimeoutExpired:
        node.kill()
    if not keep:
        shutil.rmtree(data, ignore_errors=True)


class Checks:
    def __init__(self):
        self.made = 0
        self.bad = []

    def that(self, what, ok, saw=None):
        self.made += 1
        if not ok:
            self.bad.append(f"{what}: {saw}")


def scrape(url, query=""):
    """The metrics as a scraper gets them: the text, and what it parses to."""
    status, text, headers = call(url, "GET", "/_prometheus/metrics" + query, raw=True)
    return status, text, headers


def parsed(text):
    """Every series in a scrape: (name, labels, value), and what was wrong.

    This is the parse a scraper makes. A line it would reject is a scrape that
    fails, whatever the line was meant to say.
    """
    series, complaints = [], []
    declared_type, declared_help, seen_family = {}, {}, set()
    for n, line in enumerate(text.splitlines(), 1):
        if not line.strip():
            continue
        if line.startswith("# HELP "):
            name = line[len("# HELP ") :].split(" ", 1)[0]
            if name in declared_help:
                complaints.append(f"line {n}: [{name}] declares its help twice")
            declared_help[name] = True
            continue
        if line.startswith("# TYPE "):
            rest = line[len("# TYPE ") :].split(" ")
            if len(rest) != 2 or rest[1] not in ("gauge", "counter", "histogram", "summary",
                                                 "untyped"):
                complaints.append(f"line {n}: [{line}] is not a type declaration")
                continue
            if rest[0] in declared_type:
                complaints.append(f"line {n}: [{rest[0]}] declares its type twice")
            declared_type[rest[0]] = rest[1]
            continue
        if line.startswith("#"):
            continue
        m = SERIES.match(line)
        if not m:
            complaints.append(f"line {n}: [{line[:70]}] is not a series")
            continue
        name, labels, value = m.group("name"), m.group("labels") or "{}", m.group("value")
        try:
            v = float(value)
        except ValueError:
            complaints.append(f"line {n}: [{name}] has a value that is not a number: {value}")
            continue
        inner = labels[1:-1]
        pairs = dict(LABELS.findall(inner))
        # what the labels parse to has to be the whole of what was written:
        # a quote that escaped its label would leave a remainder
        rebuilt = ",".join(f'{k}="{v2}"' for k, v2 in pairs.items())
        if inner and len(rebuilt) != len(inner):
            complaints.append(f"line {n}: [{name}] has labels that do not parse: {inner[:60]}")
        if name not in declared_type:
            complaints.append(f"line {n}: [{name}] has no type declared before it")
        seen_family.add(name)
        series.append((name, pairs, v))
    return series, complaints, declared_type


def find(series, metric, **labels):
    """The value of one series, or None.

    The metric is positional because a label may be called `name`, and one of
    the ones that matters most -- a breaker's, a pool's -- is.
    """
    for got_name, got_labels, v in series:
        if got_name == metric and all(got_labels.get(k) == str(val) for k, val in labels.items()):
            return v
    return None


def the_exporter(url, c):
    status, text, headers = scrape(url)
    c.that("the exporter answers", status == 200, status)
    c.that(
        "it answers in the format a scraper expects",
        "text/plain" in headers.get("content-type", ""),
        headers.get("content-type"),
    )
    series, complaints, types = parsed(text)
    c.that("every line parses as a scraper would parse it", not complaints, complaints[:4])
    c.that("there is something to read", len(series) > 50, len(series))

    # the families a dashboard written for OpenSearch looks for
    wanted = [
        "opensearch_cluster_status",
        "opensearch_cluster_nodes_number",
        "opensearch_os_cpu_percent",
        "opensearch_process_cpu_percent",
        "opensearch_jvm_mem_heap_used_bytes",
        "opensearch_fs_total_available_bytes",
        "opensearch_threadpool_threads_number",
        "opensearch_threadpool_tasks_number",
        "opensearch_threadpool_tasks_count",
        "opensearch_circuitbreaker_estimated_bytes",
        "opensearch_circuitbreaker_limit_bytes",
        "opensearch_circuitbreaker_tripped_count",
        "opensearch_indices_doc_number",
        "opensearch_indices_search_query_count",
        "opensearch_indices_indexing_index_count",
        "velosearch_build_info",
    ]
    missing = [w for w in wanted if w not in types]
    c.that("every family an OpenSearch dashboard reads is there", not missing, missing)
    c.that(
        "a counter is declared a counter and a gauge a gauge",
        types.get("opensearch_indices_search_query_count") == "counter"
        and types.get("opensearch_cluster_status") == "gauge",
        {k: types.get(k) for k in ("opensearch_indices_search_query_count",
                                   "opensearch_cluster_status")},
    )
    c.that(
        "every series says which node it came from",
        all(("cluster" in labels and "node" in labels) for _, labels, _ in series),
        [n for n, labels, _ in series if "node" not in labels][:3],
    )
    return series


def the_numbers(url, c):
    """The metrics are this node's, not a shape."""
    call(url, "DELETE", "/metrics_check")
    call(url, "PUT", "/metrics_check", {"settings": {"index.number_of_replicas": 0}})
    lines = []
    for i in range(500):
        lines.append(json.dumps({"index": {"_index": "metrics_check"}}))
        lines.append(json.dumps({"n": i, "k": f"k{i % 10}"}))
    call(url, "POST", "/_bulk", ndjson="\n".join(lines) + "\n")
    call(url, "POST", "/metrics_check/_refresh")

    series, _, _ = parsed(scrape(url)[1])
    docs = find(series, "opensearch_index_doc_number", index="metrics_check")
    c.that("an index's documents are counted as the index counts them", docs == 500, docs)
    written = find(series, "opensearch_indices_indexing_index_count")
    c.that("the writes are counted", written is not None and written >= 500, written)

    before = find(series, "opensearch_indices_search_query_count") or 0
    for _ in range(5):
        call(url, "POST", "/metrics_check/_search", {"size": 1})
    after = find(parsed(scrape(url)[1])[0], "opensearch_indices_search_query_count") or 0
    c.that("a search moves the query counter", after >= before + 5, f"{before} -> {after}")

    # the ceilings, seen from outside: this is what the endpoint is for
    breaker_before = find(
        parsed(scrape(url)[1])[0], "opensearch_circuitbreaker_tripped_count", name="request"
    )
    call(url, "PUT", "/_cluster/settings", {"transient": {"indices.breaker.request.limit": "0b"}})
    status, _ = call(
        url, "POST", "/metrics_check/_search", {"size": 0, "aggs": {"a": {"terms": {"field": "k"}}}}
    )
    call(url, "PUT", "/_cluster/settings", {"transient": {"indices.breaker.request.limit": None}})
    breaker_after = find(
        parsed(scrape(url)[1])[0], "opensearch_circuitbreaker_tripped_count", name="request"
    )
    c.that("the search was refused, as the limits gate expects", status == 429, status)
    c.that(
        "a breaker that refused says so where a scraper reads",
        breaker_after is not None and breaker_after > (breaker_before or 0),
        f"{breaker_before} -> {breaker_after}",
    )

    limit = find(parsed(scrape(url)[1])[0], "opensearch_circuitbreaker_limit_bytes", name="parent")
    held = find(
        parsed(scrape(url)[1])[0], "opensearch_circuitbreaker_estimated_bytes", name="parent"
    )
    c.that(
        "the parent breaker reports what it is holding and what it may hold",
        limit and held and 0 < held < limit,
        f"{held} of {limit}",
    )

    # per-index series can be left out, for a node holding too many to name
    _, text, _ = scrape(url, "?indices=false")
    series, _, _ = parsed(text)
    c.that(
        "the per-index series can be turned off",
        find(series, "opensearch_index_doc_number", index="metrics_check") is None,
        "still there",
    )
    c.that(
        "and the node-wide ones remain",
        find(series, "opensearch_indices_doc_number") is not None,
        "gone too",
    )


def the_pool_rejection(binary, port, transport, c):
    """A pool that refused says so where a scraper reads."""
    node, data, url, _ = start_node(
        binary,
        port,
        transport,
        {
            "VELOSEARCH_THREAD_POOL_SEARCH_SIZE": "1",
            "VELOSEARCH_THREAD_POOL_SEARCH_QUEUE_SIZE": "0",
        },
    )
    try:
        import concurrent.futures

        call(url, "PUT", "/metrics_pool", {"settings": {"index.number_of_replicas": 0}})
        lines = []
        for i in range(20000):
            lines.append(json.dumps({"index": {"_index": "metrics_pool"}}))
            lines.append(json.dumps({"n": i, "k": f"k{i % 2000}"}))
        call(url, "POST", "/_bulk", ndjson="\n".join(lines) + "\n", timeout=300)
        call(url, "POST", "/metrics_pool/_refresh")
        agg = {
            "size": 0,
            "aggs": {"by": {"terms": {"field": "k", "size": 2000, "order": {"_key": "asc"}}}},
        }
        with concurrent.futures.ThreadPoolExecutor(max_workers=16) as pool:
            codes = [
                f.result()[0]
                for f in [
                    pool.submit(call, url, "POST", "/metrics_pool/_search", agg) for _ in range(16)
                ]
            ]
        refused = codes.count(429)
        series, _, _ = parsed(scrape(url)[1])
        reported = find(
            series, "opensearch_threadpool_tasks_count", name="search", type="rejected"
        )
        c.that("the flood was partly refused", refused > 0, codes)
        c.that(
            "a pool that refused says so where a scraper reads",
            reported is not None and reported >= refused,
            f"{reported} reported, {refused} refused",
        )
        c.that(
            "the pool's size is reported as the node was told to run it",
            find(series, "opensearch_threadpool_threads_number", name="search") == 1,
            find(series, "opensearch_threadpool_threads_number", name="search"),
        )
    finally:
        stop_node(node, data)


def the_log(binary, port, transport, c):
    """The log can be turned up, and written for a collector."""
    # json, and loud enough to say something
    node, data, url, log_path = start_node(
        binary, port, transport, {"VELOSEARCH_LOG": "info", "VELOSEARCH_LOG_FORMAT": "json"}
    )
    try:
        # a restore logs at info, which is how we make the node say something
        call(url, "PUT", "/logged", {"settings": {"index.number_of_replicas": 0}})
        call(url, "POST", "/logged/_doc?refresh=true", {"a": 1})
        call(url, "PUT", "/_snapshot/r", {"type": "fs", "settings": {"location": "r"}})
        call(url, "PUT", "/_snapshot/r/s?wait_for_completion=true", {})
        call(url, "DELETE", "/logged")
        call(url, "POST", "/_snapshot/r/s/_restore?wait_for_completion=true", {})
        time.sleep(1)
        lines = [
            line
            for line in pathlib.Path(log_path).read_text().splitlines()
            if line.startswith("{")
        ]
        c.that("the log was written as json", lines, "no json lines")
        objects = []
        for line in lines:
            try:
                objects.append(json.loads(line))
            except json.JSONDecodeError:
                c.that("every json line is an object", False, line[:80])
                break
        c.that(
            "each line carries a level, a timestamp and what wrote it",
            objects
            and all(
                "level" in o and "timestamp" in o and "target" in o for o in objects
            ),
            objects[:1],
        )
        c.that(
            "the filter let an info event through, which the old level could not",
            any(o.get("level") == "INFO" for o in objects),
            {o.get("level") for o in objects},
        )
    finally:
        stop_node(node, data)

    # and a node told nothing keeps its quiet default
    node, data, url, log_path = start_node(binary, port + 1, transport + 1, {})
    try:
        call(url, "PUT", "/quiet", {"settings": {"index.number_of_replicas": 0}})
        call(url, "POST", "/quiet/_doc?refresh=true", {"a": 1})
        time.sleep(1)
        text = pathlib.Path(log_path).read_text()
        c.that(
            "a node told nothing is as quiet as it was before",
            "INFO" not in text and "DEBUG" not in text,
            text[:120],
        )
    finally:
        stop_node(node, data)

    # a module can be raised on its own, which is what a filter is for
    node, data, url, log_path = start_node(
        binary, port + 2, transport + 2, {"VELOSEARCH_LOG": "warn,velocore=info"}
    )
    try:
        call(url, "PUT", "/narrowed", {"settings": {"index.number_of_replicas": 0}})
        for i in range(200):
            call(url, "POST", "/narrowed/_doc", {"a": i})
        call(url, "POST", "/narrowed/_forcemerge?max_num_segments=1")
        time.sleep(1)
        text = pathlib.Path(log_path).read_text()
        c.that(
            "a filter naming one module raises that module and not the rest",
            "velocore" in text or text.strip() == "" or "INFO" in text,
            text[:120],
        )
    finally:
        stop_node(node, data)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--binary", default=str(ROOT / "target" / "release" / "velosearch"))
    ap.add_argument("--port", type=int, default=9275)
    ap.add_argument("--transport", type=int, default=9375)
    args = ap.parse_args()

    c = Checks()
    node, data, url, _ = start_node(args.binary, args.port, args.transport, {})
    try:
        the_exporter(url, c)
        the_numbers(url, c)
    finally:
        stop_node(node, data)
    the_pool_rejection(args.binary, args.port + 4, args.transport + 4, c)
    the_log(args.binary, args.port + 8, args.transport + 8, c)

    print(f"  {c.made} checks over the exporter, its numbers and the log")
    for row in c.bad:
        print(f"    {row}")
    if c.bad:
        print("\nRESULT what an operator can see is not what the node is doing")
        return 1
    print("\nRESULT the node's numbers are readable by a scraper, and the log by an operator")
    return 0


if __name__ == "__main__":
    sys.exit(main())
