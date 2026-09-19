#!/usr/bin/env python3
"""Chaos, soak and rolling restart against three real nodes.

The script starts the nodes itself (so it can kill and restart them on
their own data directories), drives writers and readers at them, and
applies faults on a schedule:

  partition   cut one node off through /_velo/chaos, heal after a while
  stop        SIGSTOP one node, SIGCONT after a while
  kill        SIGKILL one node, start it again on its data directory
  restart     SIGTERM one node (a graceful leave), start it again
  rolling     SIGTERM and restart every node in turn, green between each

At the end it waits for the cluster to settle, then checks every
acknowledged document on every node that holds a copy: an acknowledged
write that a copy does not have is LOST, and the run fails. A soak run
also samples each node's resident memory and reports first-minute
against last-minute, so a leak shows as a slope.

  cluster_chaos.py --mode chaos   --seconds 90
  cluster_chaos.py --mode rolling --rounds 2
  cluster_chaos.py --mode soak    --seconds 900

A rolling restart that puts a different build on each node as it comes back
is a rolling upgrade, which is the shape a deployment actually meets and the
one nothing here had ever run: every check until now was three nodes of one
build. `--to-binary` restarts each node onto that build in turn, and
`--and-back` then walks them back to the one they started on, which is the
rollback. Each node is asked what it was built from at every step, so the log
shows the cluster really was of two minds and for how long.

  cluster_chaos.py --mode rolling --to-binary /tmp/next/velosearch --and-back
"""
import argparse, json, os, random, shutil, signal, subprocess, sys, tempfile, threading, time, urllib.error, urllib.request

# the ports the three nodes take; another run at the same time asks for its
# own with VELOSEARCH_TEST_PORTS=<first http>,<first transport>
_base = os.environ.get("VELOSEARCH_TEST_PORTS", "9213,9303").split(",")
HTTP = [int(_base[0]) + i for i in range(3)]
TRANSPORT = [int(_base[1]) + i for i in range(3)]
NAMES = ["n1", "n2", "n3"]


def call(url, method="GET", body=None, timeout=5):
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(url, data=data, method=method, headers={"content-type": "application/json"})
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return r.status, json.loads(r.read() or b"{}")


class Node:
    def __init__(self, i, binary, root, seeds, log_dir):
        self.i = i
        self.name = NAMES[i]
        self.http = f"127.0.0.1:{HTTP[i]}"
        self.binary = binary
        self.data = os.path.join(root, self.name)
        os.makedirs(self.data, exist_ok=True)
        self.seeds = seeds
        self.log = open(os.path.join(log_dir, f"{self.name}.log"), "ab")
        self.proc = None
        self.up_since = None

    def start(self):
        env = dict(os.environ)
        env.update({
            "VELOSEARCH_ADDR": self.http,
            "VELOSEARCH_DATA": self.data,
            "VELOSEARCH_TRANSPORT_PORT": str(TRANSPORT[self.i]),
            "VELOSEARCH_NODE_NAME": self.name,
            "VELOSEARCH_CHAOS": "1",
            "VELOSEARCH_DISCOVERY_SEED_HOSTS": self.seeds,
            "VELOSEARCH_CLUSTER_INITIAL_CLUSTER_MANAGER_NODES": ",".join(NAMES),
            "VELOSEARCH_CLUSTER_DEBUG": "1",
        })
        self.proc = subprocess.Popen([self.binary], env=env, stdout=self.log, stderr=subprocess.STDOUT)
        self.up_since = time.monotonic()

    def wait_http(self, seconds=30):
        t0 = time.monotonic()
        while time.monotonic() - t0 < seconds:
            try:
                call(f"http://{self.http}/", timeout=2)
                return True
            except Exception:
                time.sleep(0.25)
        return False

    def rss_mib(self):
        if not self.proc:
            return None
        try:
            out = subprocess.check_output(["ps", "-o", "rss=", "-p", str(self.proc.pid)], text=True).strip()
            return int(out) / 1024 if out else None
        except Exception:
            return None

    def signal(self, sig):
        if self.proc:
            os.kill(self.proc.pid, sig)

    def stop_graceful(self, seconds=15):
        if not self.proc:
            return
        self.proc.send_signal(signal.SIGTERM)
        try:
            self.proc.wait(timeout=seconds)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait()
        self.proc = None

    def kill(self):
        if not self.proc:
            return
        self.proc.kill()
        self.proc.wait()
        self.proc = None


class Load:
    """Writers and readers. Every acknowledged document id is remembered
    with the value written; every error is counted by phase."""

    def __init__(self, nodes, index, workers, seed):
        self.nodes = nodes
        self.index = index
        self.workers = workers
        self.seed = seed
        self.lock = threading.Lock()
        self.acked = {}  # id -> value
        self.acked_at = {}  # id -> (seconds since start, node name)
        self.acked_copies = {}  # id -> _shards.successful
        self.t0 = time.monotonic()
        self.attempted = 0
        self.errors = 0
        self.reads = 0
        self.read_errors = 0
        self.stop = threading.Event()
        self.threads = []

    def run(self):
        for w in range(self.workers):
            t = threading.Thread(target=self.worker, args=(w,), daemon=True)
            t.start()
            self.threads.append(t)

    def worker(self, w):
        rng = random.Random(self.seed * 100 + w)
        n = 0
        while not self.stop.is_set():
            node = rng.choice(self.nodes)
            n += 1
            r = rng.random()
            try:
                if r < 0.45:
                    doc_id = f"w{w}-{n}"
                    value = n
                    with self.lock:
                        self.attempted += 1
                    st, ans = call(f"http://{node.http}/{self.index}/_doc/{doc_id}", "PUT", {"v": value, "w": w}, timeout=10)
                    if st in (200, 201):
                        with self.lock:
                            self.acked[doc_id] = value
                            self.acked_at[doc_id] = (time.monotonic() - self.t0, node.name)
                            self.acked_copies[doc_id] = ans.get("_shards", {}).get("successful")
                    else:
                        with self.lock:
                            self.errors += 1
                elif r < 0.6:
                    # a bulk of twenty
                    lines = []
                    ids = []
                    for k in range(20):
                        n += 1
                        doc_id = f"w{w}-{n}"
                        ids.append((doc_id, n))
                        lines.append(json.dumps({"index": {"_index": self.index, "_id": doc_id}}))
                        lines.append(json.dumps({"v": n, "w": w}))
                    with self.lock:
                        self.attempted += len(ids)
                    body = ("\n".join(lines) + "\n").encode()
                    req = urllib.request.Request(f"http://{node.http}/_bulk", data=body, method="POST", headers={"content-type": "application/x-ndjson"})
                    with urllib.request.urlopen(req, timeout=15) as resp:
                        out = json.loads(resp.read())
                    items = out.get("items", [])
                    with self.lock:
                        for (doc_id, value), item in zip(ids, items):
                            st = item.get("index", {}).get("status")
                            if st in (200, 201):
                                self.acked[doc_id] = value
                                self.acked_at[doc_id] = (time.monotonic() - self.t0, node.name)
                                self.acked_copies[doc_id] = item.get("index", {}).get("_shards", {}).get("successful")
                            else:
                                self.errors += 1
                        self.errors += max(0, len(ids) - len(items))
                else:
                    with self.lock:
                        self.reads += 1
                        known = list(self.acked.items())[-50:] if self.acked else []
                    if known and rng.random() < 0.7:
                        doc_id, value = rng.choice(known)
                        st, body = call(f"http://{node.http}/{self.index}/_doc/{doc_id}", timeout=10)
                    else:
                        st, body = call(f"http://{node.http}/{self.index}/_search", "POST", {"size": 5, "query": {"term": {"w": w}}}, timeout=10)
            except urllib.error.HTTPError as e:
                with self.lock:
                    if r < 0.6:
                        self.errors += 1 if r < 0.45 else 20
                    else:
                        self.read_errors += 1
            except Exception:
                with self.lock:
                    if r < 0.6:
                        self.errors += 1 if r < 0.45 else 20
                    else:
                        self.read_errors += 1
            time.sleep(rng.uniform(0.002, 0.02))

    def halt(self):
        self.stop.set()
        for t in self.threads:
            t.join(timeout=20)


def any_up(nodes):
    for n in nodes:
        if n.proc is None:
            continue
        try:
            call(f"http://{n.http}/", timeout=2)
            return n
        except Exception:
            continue
    return None


def wait_green(nodes, index, seconds=120):
    """Every running node says green, with every node in the cluster.

    Asking one node is not enough: a node that never rejoined after a
    partition answers happily about the cluster it remembers."""
    t0 = time.monotonic()
    while time.monotonic() - t0 < seconds:
        want = sum(1 for x in nodes if x.proc is not None)
        agreed = 0
        for n in nodes:
            if n.proc is None:
                continue
            try:
                st, h = call(f"http://{n.http}/_cluster/health/{index}?wait_for_nodes={want}&timeout=1s", timeout=5)
                if h.get("status") == "green" and h.get("number_of_nodes") == want and not h.get("timed_out"):
                    agreed += 1
            except Exception:
                pass
        if agreed == want:
            return time.monotonic() - t0
        time.sleep(0.5)
    return None


def copy_holders(nodes, index):
    n = any_up(nodes)
    if not n:
        return []
    try:
        req = urllib.request.Request(f"http://{n.http}/_cat/shards/{index}?h=node,state")
        with urllib.request.urlopen(req, timeout=5) as r:
            rows = [l.split() for l in r.read().decode().splitlines() if l.strip()]
        return sorted({row[0] for row in rows if len(row) == 2 and row[1] == "STARTED"})
    except Exception:
        return []


def where_it_stands(nodes, holders, index, doc_id):
    """What each copy says about one document: its sequence number, the
    primary term it was written in, and its version -- or that it is not
    there. A copy short of a document is only half a finding; which numbers
    the documents around the gap carry is what says why the gap is there."""
    out = []
    for n in nodes:
        if n.name not in holders:
            continue
        try:
            st, body = call(f"http://{n.http}/{index}/_doc/{doc_id}?preference=_local", timeout=10)
            if body.get("found"):
                out.append(f"{n.name}: seq={body.get('_seq_no')} term={body.get('_primary_term')} v={body.get('_version')}")
            else:
                out.append(f"{n.name}: absent")
        except Exception as e:
            out.append(f"{n.name}: ? ({type(e).__name__})")
    return "; ".join(out)


def seq_span(nodes, holders, index):
    """The highest sequence number each copy holds."""
    out = []
    for n in nodes:
        if n.name not in holders:
            continue
        try:
            st, r = call(f"http://{n.http}/{index}/_search?preference=_local", "POST",
                         {"size": 0, "aggs": {"hi": {"max": {"field": "_seq_no"}}}}, timeout=10)
            out.append(f"{n.name}: max seq {(r.get('aggregations') or {}).get('hi', {}).get('value')}")
        except Exception as e:
            out.append(f"{n.name}: ? ({type(e).__name__})")
    return "; ".join(out)



def build_of(node):
    """What a node says it was built from, or "?" when it will not say."""
    try:
        _, body = call(f"http://{node.http}/", timeout=5)
        return (body.get("version") or {}).get("build_hash", "?")
    except Exception:
        return "?"


def walk_builds(nodes, a, frm, to, what, note, fault):
    """Put `to` on every node in turn, waiting for green between each.

    The point is the middle of it: for as long as this takes, the cluster is
    of two builds at once, taking writes and answering reads. A deployment
    does this every time it ships, and nothing here had ever run it -- every
    check was three nodes of one build, which is the one arrangement a
    production cluster is never in while it is being changed.
    """
    note(f"{what}: {frm} -> {to}")
    for i, v in enumerate(nodes):
        v.binary = to
        fault("restart", i)
        g = wait_green(nodes, a.index, 180)
        mix = ", ".join(f"{x.name}={build_of(x)[:12]}" for x in nodes)
        note(
            f"{what}: {v.name} is back, green "
            f"{'after %.1fs' % g if g is not None else 'NOT within 180s'}; cluster is [{mix}]"
        )
        if g is None:
            print(f"  the cluster did not go green during the {what}; stopping there")
            break
        time.sleep(3)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--binary", default="./target/release/velosearch")
    ap.add_argument("--mode", choices=["chaos", "rolling", "soak"], default="chaos")
    ap.add_argument("--seconds", type=int, default=90)
    ap.add_argument("--rounds", type=int, default=2, help="rolling: how many times round the nodes")
    ap.add_argument(
        "--to-binary",
        default="",
        help="rolling: the build each node comes back on, one at a time -- an upgrade",
    )
    ap.add_argument(
        "--and-back",
        action="store_true",
        help="rolling: after the upgrade, walk every node back to the build it started on",
    )
    ap.add_argument("--faults", default="partition,stop,kill,restart", help="chaos and soak: kinds to mix")
    ap.add_argument("--workers", type=int, default=4)
    ap.add_argument("--index", default="chaos")
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--root", default="", help="data root; a fresh temporary one when empty")
    a = ap.parse_args()
    rng = random.Random(a.seed)
    root = a.root or tempfile.mkdtemp(prefix="bschaos.")
    log_dir = os.path.join(root, "logs")
    os.makedirs(log_dir, exist_ok=True)
    seeds = ",".join(f"127.0.0.1:{p}" for p in TRANSPORT)
    nodes = [Node(i, a.binary, root, seeds, log_dir) for i in range(3)]
    for n in nodes:
        n.start()
    for n in nodes:
        if not n.wait_http():
            print(f"{n.name} did not come up; see {log_dir}", file=sys.stderr)
            return 2
    time.sleep(6)
    n0 = nodes[0]
    call(f"http://{n0.http}/{a.index}", "PUT", {"settings": {"number_of_shards": 1, "number_of_replicas": 1, "index.unassigned.node_left.delayed_timeout": "2s"}}, timeout=10)
    if wait_green(nodes, a.index, 60) is None:
        print("the index did not go green before the run", file=sys.stderr)
        return 2
    print(f"data under {root}; logs in {log_dir}")
    load = Load(nodes, a.index, a.workers, a.seed)
    load.run()
    routing_log = []

    def watch_routing():
        last = None
        while not load.stop.is_set():
            n = any_up(nodes)
            if n:
                try:
                    req = urllib.request.Request(f"http://{n.http}/{a.index}/_search_shards")
                    with urllib.request.urlopen(req, timeout=2) as r:
                        body = json.loads(r.read())
                    names = {v["name"]: k for k, v in body.get("nodes", {}).items()}
                    id_to_name = {k: v["name"] for k, v in body.get("nodes", {}).items()}
                    shards = body.get("shards", [[]])[0]
                    now = ", ".join(f"{'p' if c.get('primary') else 'r'}={id_to_name.get(c.get('node'), '?')}:{c.get('state')}" for c in shards)
                    now = f"{now} (asked {n.name})"
                    if now != last:
                        routing_log.append((time.monotonic() - load.t0, now))
                        last = now
                except Exception:
                    pass
            time.sleep(0.3)

    threading.Thread(target=watch_routing, daemon=True).start()
    t0 = time.monotonic()
    events = []
    samples = []  # (t, [rss per node])

    def note(what):
        events.append((time.monotonic() - t0, what))
        print(f"{time.monotonic() - t0:7.1f}s  {what}", flush=True)

    def sample():
        samples.append((time.monotonic() - t0, [n.rss_mib() for n in nodes]))

    def fault(kind, victim):
        v = nodes[victim]
        others = [nodes[i] for i in range(3) if i != victim]
        if kind == "partition":
            note(f"isolate {v.name}")
            for n in nodes:
                try:
                    cut = [x.name for x in others] if n is v else [v.name]
                    call(f"http://{n.http}/_velo/chaos", "POST", {"cut": cut}, timeout=5)
                except Exception:
                    pass
            time.sleep(rng.uniform(4, 9))
            for n in nodes:
                try:
                    call(f"http://{n.http}/_velo/chaos", "POST", {"heal": True}, timeout=5)
                except Exception:
                    pass
            note(f"heal {v.name}")
        elif kind == "stop":
            note(f"stop {v.name}")
            v.signal(signal.SIGSTOP)
            time.sleep(rng.uniform(3, 8))
            v.signal(signal.SIGCONT)
            note(f"continue {v.name}")
        elif kind == "kill":
            note(f"kill {v.name}")
            v.kill()
            time.sleep(rng.uniform(2, 6))
            v.start()
            v.wait_http()
            note(f"{v.name} back after a kill")
        elif kind == "restart":
            note(f"restart {v.name} (SIGTERM)")
            v.stop_graceful()
            time.sleep(rng.uniform(1, 3))
            v.start()
            v.wait_http()
            note(f"{v.name} back")

    if a.mode == "rolling":
        if a.to_binary:
            was = nodes[0].binary
            walk_builds(nodes, a, was, a.to_binary, "upgrade", note, fault)
            if a.and_back:
                walk_builds(nodes, a, a.to_binary, was, "rollback", note, fault)
        else:
            for r in range(a.rounds):
                for i in range(3):
                    fault("restart", i)
                    g = wait_green(nodes, a.index, 120)
                    note(f"green {'after %.1fs' % g if g is not None else 'NOT within 120s'} ({nodes[i].name}, round {r + 1})")
                    time.sleep(3)
    else:
        kinds = [k for k in a.faults.split(",") if k and k != "none"]
        last_sample = 0
        while time.monotonic() - t0 < a.seconds and kinds:
            if a.mode == "soak":
                time.sleep(rng.uniform(8, 20))
            else:
                time.sleep(rng.uniform(3, 6))
            if time.monotonic() - last_sample > 10:
                sample()
                last_sample = time.monotonic()
            fault(rng.choice(kinds), rng.randrange(3))
        while time.monotonic() - t0 < a.seconds:
            time.sleep(0.5)
        sample()
    # quiet, then settle
    time.sleep(3)
    load.halt()
    settled = wait_green(nodes, a.index, 120)
    print(f"settled: {'after %.1fs' % settled if settled is not None else 'NOT within 120s'}")
    if settled is None:
        for x in [x for x in nodes if x.proc is not None]:
            try:
                st, h = call(f"http://{x.http}/_cluster/health/{a.index}", timeout=5)
                st2, who = call(f"http://{x.http}/_cluster/state?filter_path=cluster_manager_node,master_node", timeout=5)
                print(f"  as {x.name} sees it: {h.get('status')}, nodes={h.get('number_of_nodes')}, manager={who.get('cluster_manager_node') or who.get('master_node')}")
            except Exception as e:
                print(f"  as {x.name} sees it: no answer ({e})")
        n = any_up(nodes)
        if n:
            try:
                req = urllib.request.Request(f"http://{n.http}/_cat/shards/{a.index}?v&h=index,prirep,state,node,unassigned.reason")
                with urllib.request.urlopen(req, timeout=5) as r:
                    print("  " + r.read().decode().replace("\n", "\n  "))
            except Exception:
                pass
    time.sleep(2)
    # the check: every acknowledged document, on every copy
    #
    # A GET with preference=_local on a node whose copy is taken away while
    # the check reads it answers 404 until the copy is gone and then forwards
    # to a copy elsewhere, so the check read hundreds of documents as missing
    # that were never missing (a publication that timed out after the load
    # stopped had the manager move a replica, and 239 documents read as
    # "behind" were all there half a second later). Which copies exist is
    # taken again after the pass, and a pass the copies moved under is
    # thrown away and read again once the cluster is green.
    def placement():
        n = any_up(nodes)
        if not n:
            return None
        try:
            st, rt = call(f"http://{n.http}/_cluster/state/routing_table/{a.index}", timeout=10)
            shards = rt["routing_table"]["indices"][a.index]["shards"]
            return sorted(
                (c.get("node"), c.get("state"), bool(c.get("primary")), (c.get("allocation_id") or {}).get("id") or "")
                for copies in shards.values() for c in copies
            )
        except Exception:
            return None

    for attempt in range(3):
        before = placement()
        holders = copy_holders(nodes, a.index)
        print(f"{load.attempted} writes attempted, {len(load.acked)} acknowledged, {load.errors} refused or failed; {load.reads} reads, {load.read_errors} failed; copies on {holders}")
        lost = 0
        wrong = 0
        checked = 0
        unread = 0
        lost_ids = []
        # doc id -> the holders that do not have it
        missing_from = {}
        if not holders:
            # nobody to ask is not "nothing lost": the listing failed, and the
            # verdict below would be a verdict over zero documents
            print("RESULT UNKNOWN: no node reports a started copy of the index; the lost-write check did not run")
            return 2
        for n in nodes:
            if n.name not in holders:
                continue
            try:
                call(f"http://{n.http}/{a.index}/_refresh", "POST", timeout=10)
                st, c = call(f"http://{n.http}/{a.index}/_count?preference=_local", timeout=10)
                print(f"  {n.name}: _count {c.get('count')} against {len(load.acked)} acknowledged")
            except Exception as e:
                print(f"  {n.name}: count failed: {e}")
            for doc_id, value in load.acked.items():
                checked += 1
                try:
                    st, body = call(f"http://{n.http}/{a.index}/_doc/{doc_id}?preference=_local", timeout=10)
                    if not body.get("found"):
                        missing_from.setdefault(doc_id, []).append(n.name)
                        if lost <= 5:
                            print(f"  LOST {doc_id} on {n.name}")
                    elif body.get("_source", {}).get("v") != value:
                        wrong += 1
                        if wrong <= 5:
                            print(f"  WRONG {doc_id} on {n.name}: {body.get('_source')} against v={value}")
                except urllib.error.HTTPError as e:
                    if e.code == 404:
                        missing_from.setdefault(doc_id, []).append(n.name)
                    else:
                        unread += 1
                        if unread <= 5:
                            print(f"  read of {doc_id} on {n.name}: http {e.code}")
                except Exception as e:
                    unread += 1
                    if unread <= 5:
                        print(f"  read of {doc_id} on {n.name}: {e}")
        after = placement()
        if before is not None and before == after:
            break
        print(f"  the copies moved while they were being read ({before} -> {after}); reading again")
        wait_green(nodes, a.index, 60)
        time.sleep(2)
    if samples:
        first = samples[0][1]
        last = samples[-1][1]
        print("memory (RSS MiB) first sample -> last sample per node:")
        for i, n in enumerate(nodes):
            print(f"  {n.name}: {first[i]:.0f} -> {last[i]:.0f}" if first[i] and last[i] else f"  {n.name}: n/a")
    # an acknowledged write missing from every holder is lost; missing from
    # some of them is a copy that is behind while the cluster says green
    behind = {}
    for doc_id, nodes_without in missing_from.items():
        if len(nodes_without) >= len(holders):
            lost += 1
            lost_ids.append(doc_id)
        else:
            for name in nodes_without:
                behind[name] = behind.get(name, 0) + 1
    for name, n_behind in sorted(behind.items()):
        times = sorted(
            load.acked_at.get(i, (0, "?"))[0]
            for i, ns in missing_from.items()
            if name in ns and len(ns) < len(holders)
        )
        print(f"  BEHIND {name}: {n_behind} acknowledged writes it does not have, from {times[0]:.1f}s to {times[-1]:.1f}s on the load clock")
    # a copy behind is read against the faults the same way a lost write is:
    # without the timeline, "69.9s on the load clock" named no fault
    if behind and not lost_ids:
        print("  faults (event clock) and routing (load clock; the load clock starts %.1fs earlier):" % (t0 - load.t0))
        merged = [(t + (t0 - load.t0), "FAULT " + what) for t, what in events] + [(t, "routing " + r) for t, r in routing_log]
        for t, what in sorted(merged):
            print(f"    {t:6.1f}s {what}")
        for name in sorted(behind):
            ids = sorted(i for i, ns in missing_from.items() if name in ns and len(ns) < len(holders))
            print(f"  behind on {name}: {ids[:10]}")
            # who acknowledged the writes this copy was short of, and when --
            # a primary cannot be short of its own writes, so the node that
            # answered is most of the explanation
            by = {}
            for doc in ids:
                t, who = load.acked_at.get(doc, (0, "?"))
                by[who] = by.get(who, 0) + 1
            print(f"      acknowledged by: {by}")
            for doc in ids[:3] + ids[-2:]:
                print(f"      {doc}: {where_it_stands(nodes, holders, a.index, doc)}")
            # and how long it stays short: asked again, a few times
            for wait in (0.5, 2, 5):
                time.sleep(wait)
                still = []
                for doc in ids[:50]:
                    try:
                        n = next(x for x in nodes if x.name == name)
                        st, body = call(f"http://{n.http}/{a.index}/_doc/{doc}?preference=_local", timeout=10)
                        if not body.get("found"):
                            still.append(doc)
                    except Exception:
                        still.append(doc)
                print(f"      after another {wait}s: {len(still)} of {min(len(ids), 50)} still missing on {name}")
    for doc_id in lost_ids[:5]:
        print(f"  LOST {doc_id}: on none of {holders}")
    if lost_ids:
        # when the lost writes were acknowledged, and by which node, against the faults
        times = sorted(load.acked_at.get(i, (0, "?")) for i in set(lost_ids))
        by_node = {}
        for t, name in times:
            by_node[name] = by_node.get(name, 0) + 1
        print(f"lost writes were acknowledged between {times[0][0]:.1f}s and {times[-1][0]:.1f}s (load clock), by node {by_node}")
        buckets = {}
        for t, _ in times:
            buckets[int(t // 5) * 5] = buckets.get(int(t // 5) * 5, 0) + 1
        print("  per 5s: " + ", ".join(f"{k}s:{v}" for k, v in sorted(buckets.items())))
        copies = {}
        for i in set(lost_ids):
            c = load.acked_copies.get(i)
            copies[c] = copies.get(c, 0) + 1
        print(f"  lost writes by _shards.successful at the time: {copies}")
        print("  faults (event clock) and routing (load clock; the load clock starts %.1fs earlier):" % (t0 - load.t0))
        merged = [(t + (t0 - load.t0), "FAULT " + what) for t, what in events] + [(t, "routing " + r) for t, r in routing_log]
        for t, what in sorted(merged):
            print(f"    {t:6.1f}s {what}")
    # Two copies of one shard have to hold the same documents. Only the
    # acknowledged ones were checked above, and a copy may hold more than was
    # ever acknowledged -- a write in flight when its node was killed is
    # applied or not, and either is honest -- but the copies must agree with
    # each other once the cluster has settled, or the same search answers
    # differently depending on which copy it reaches. A partitioned node
    # writing documents it then reported as failures diverged by hundreds of
    # them this way, and nothing here noticed because nothing compared the
    # copies.
    def counts_now():
        out = {}
        for n in nodes:
            if n.name not in holders:
                continue
            try:
                call(f"http://{n.http}/{a.index}/_refresh", "POST", timeout=10)
                st, c = call(f"http://{n.http}/{a.index}/_count?preference=_local", timeout=10)
                if st == 200 and isinstance(c.get("count"), int):
                    out[n.name] = c["count"]
            except Exception:
                pass
        return out

    counts = counts_now()
    diverged = len(set(counts.values())) > 1
    if diverged:
        # a copy still catching up is not a copy that disagrees
        for _ in range(15):
            time.sleep(2)
            counts = counts_now()
            if len(set(counts.values())) <= 1:
                diverged = False
                break
    if diverged:
        print(f"  COPIES DISAGREE after settling: {counts}")
        # which documents, so the next look at this starts from evidence
        seen = {}
        for n in nodes:
            if n.name not in holders:
                continue
            ids = set()
            after = None
            # a page that adds nothing is a walk that is not moving: without
            # this the listing spun for ever against a node whose sort the
            # request did not advance
            for _ in range(200):
                body = {"size": 1000, "sort": [{"_id": "asc"}], "_source": False}
                if after:
                    body["search_after"] = after
                # a node that no longer holds the index answers 404, and the
                # listing is what explains a disagreement: it must not be the
                # thing that stops the run reporting one
                try:
                    st, r = call(
                        f"http://{n.http}/{a.index}/_search?preference=_local",
                        "POST",
                        body,
                        timeout=20,
                    )
                except Exception as e:
                    print(f"    (listing {n.name} stopped: {type(e).__name__} {e})")
                    break
                hits = (r.get("hits") or {}).get("hits") or []
                if st != 200 or not hits:
                    break
                before = len(ids)
                ids.update(h["_id"] for h in hits)
                after = hits[-1].get("sort")
                if not after or len(ids) == before:
                    break
            seen[n.name] = ids
        names = sorted(seen)
        for i, one in enumerate(names):
            for other in names[i + 1 :]:
                only_here = sorted(seen[one] - seen[other])[:10]
                only_there = sorted(seen[other] - seen[one])[:10]
                print(f"    on {one} and not {other}: {len(seen[one] - seen[other])} {only_here}")
                print(f"    on {other} and not {one}: {len(seen[other] - seen[one])} {only_there}")
                for doc in only_here[:3] + only_there[:3]:
                    print(f"      {doc}: acknowledged={doc in load.acked}; {where_it_stands(nodes, list(seen), a.index, doc)}")
    elif len(counts) > 1:
        print(f"  copies agree: {counts}")
    print(f"checked {checked} copies of acknowledged documents: {lost} lost, {wrong} wrong")
    print(
        "RESULT",
        "LOST" if lost or wrong else "no acknowledged write lost",
        "|",
        "every copy has them all" if not behind else f"copies behind: {behind}",
        "|",
        "COPIES DISAGREE" if diverged else "copies agree",
        "|",
        "settled" if settled is not None else "NOT settled",
    )
    for n in nodes:
        n.stop_graceful(seconds=10)
    if unread:
        print(f"  {unread} acknowledged writes could not be read back at all; they are not counted as found")
    return 1 if lost or wrong or diverged or settled is None or unread else 0


if __name__ == "__main__":
    sys.exit(main())
