# A node refuses before it dies

A node had no ceiling of any kind. The circuit breakers were reported with
limits computed from the machine, `tripped` as the literal `0` and nothing
consulting either; every request ran the moment it arrived, however many were
already running; and `timeout` on a search was parsed, checked for being a
time value, and ignored.

What that means for an operator is one behaviour with three names. An
aggregation over a high-cardinality field, a hundred bulks arriving together,
a query with no end over a large index -- each is a node that was answering
and then is not, killed by the kernel with nothing in the log that says which
request did it. Every other request in flight dies with it, and the copies on
the other nodes recover it, which is the expensive way to survive a request
that should have been refused in a millisecond.

Refusing is not the same as being slow. A client can read a 429 and back off;
it cannot do anything with a request that merely takes four minutes, and
neither can the gateway in front of it.

## What was considered

**Measuring nothing and documenting the limits.** Saying in the operations
guide what a node can take, and leaving the operator to keep traffic under it
with a proxy. This is what the absence of a decision already was. It puts the
ceiling somewhere that cannot see what the node is holding -- a proxy counts
requests, not the memory an aggregation is about to build -- and it is a
ceiling that has to be re-derived for every machine size.

**Sampling memory and shedding load when it is high.** A watchdog that reads
the process every second and starts refusing above a threshold. It needs no
accounting anywhere, which is its appeal. But it refuses whatever arrives
next rather than what is expensive, it cannot tell a search that will build
ten buckets from one that will build ten million, and by the time the
allocator has the memory the request that took it has already taken it.

**Accounting every allocation.** A breaker consulted at each point memory is
taken, as the reference does through its own object accounting. The most
accurate, and in this engine the allocation points are inside VeloCore rather
than here, so it means either a fork that reports every allocation or a
guess at each call site -- both a cost on every search to bound the rare one.

**A budget per request, out of a node-wide breaker, enforced by the engine.**
VeloCore already counts what an aggregation is building and fails it past a
ceiling; it was simply being handed its own default, per search, with nothing
counting how many searches were running. Turning that ceiling into a share of
a node-wide breaker makes the engine's own accounting the enforcement.

## The decision

**A request is given what the node can afford before it runs, and is refused
when the node cannot afford it.** Three ceilings, each the reference's own
where the reference has one:

1. **The memory.** An aggregating search reserves a share of
   `indices.breaker.request.limit` before it starts and holds it until its
   answer is written; that share is the ceiling VeloCore is held to while it
   collects. The share is the limit divided by how many searches the node
   runs at once, so what every aggregating search holds between them cannot
   pass the limit, and the search that arrives with nothing left is refused
   with `circuit_breaking_exception` rather than admitted. A body is counted
   against the in-flight breaker the same way. The parent breaker reads the
   memory the process actually holds -- `use_real_memory`, which is the
   reference's word for it -- because most of what a node holds was never
   reserved by anything; it is consulted before a request is admitted and
   again by a search already walking.
2. **The concurrency.** Each bounded pool runs so many requests at once and
   queues the rest, and answers `rejected_execution_exception` when the queue
   is full. How many at once is not the reference's thread count, because a
   request here gives its worker up whenever it waits: the default is twice
   the size the reference gives the pool, and it is a node setting because it
   is a property of the machine.
3. **The time.** A search carries its deadline into the walk, which reads it
   every few thousand documents and at each segment, and answers with what it
   had collected and `"timed_out": true`. `search.default_search_timeout`
   gives one to the searches that ask for none.

## What follows from it

**A node now refuses requests it used to accept.** That is the point, and it
is also a change in behaviour for anyone who was relying on a node attempting
whatever it was given. The limits are settings, every one of them, and
`VELOSEARCH_THREAD_POOL_ADMISSION=off` takes the concurrency ceiling away
entirely.

**A budget is a share, not a measurement.** A search that would have built
little still reserves its share while it runs, so a node can refuse a search
it would in fact have had room for. The alternative -- admitting it and
finding out -- is the behaviour this replaces. The share is settable
(`indices.breaker.request.per_search`) for a deployment that knows its own
shape.

**A timed-out search answers rather than fails.** The answer is missing
documents that match, which is only sound because the caller asked for a
deadline and is told it was reached. A walk stopped by the parent breaker is
*not* answered -- nobody asked for that -- and is a refusal.

**An inner search does not take a second share.** A search runs searches of
its own -- an aggregation answered a bucket at a time, a `collapse`, a terms
lookup -- and only the outermost is given a budget; the share it holds covers
what the inner ones build under it.

`tools/limits_check.py` puts a node in each of these states and reads what it
answers, so the ceilings are checked rather than asserted.
