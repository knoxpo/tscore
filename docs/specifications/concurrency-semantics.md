# Concurrency Semantics (M1)

## Task model

A task = *shared immutable code* + *owned portable data*:

```rust
struct Task { proto: Arc<FunctionProto>, captures: PortableValue,
              input: PortableValue, result_tx: Sender<...> }
```

10,000 tasks are 10,000 small structs in deques — never 10,000 OS threads.

## Scheduler contract

- One pinned `std::thread` worker per logical core (`--workers N` overrides).
- Chase-Lev deque per worker (`crossbeam-deque`), global injector, classic
  loop: pop-local → steal-random-victim → check injector → park (epoch
  counter to avoid lost wakeups).
- No tokio (I/O reactor, wrong tool for a CPU pool), no rayon (hides the
  scheduler this project exists to demonstrate).

## `parallel.map(items, fn)` semantics — clone at spawn

Given isolated heaps, data crosses realms only by **structured clone** into a
realm-independent `PortableValue` tree:

1. Caller's native `parallel.map` deep-clones `fn`'s captured upvalues and
   each input chunk out of the caller realm. Cycles or non-cloneable values →
   runtime error (`cannot capture X across realms`).
2. Worker allocates a fresh scratch realm (bump arena), rehydrates captures +
   chunk, runs the interpreter over its slice.
3. Results are cloned back and rehydrated into the caller's result array.
   Caller **blocks and participates in stealing** until all chunks finish —
   no async machinery at M1.

**Semantic consequence (deliberate):** mutation of captured objects inside
the callback does not leak to the caller or between chunks. `parallel.map`
is deterministic: output at `--workers 1` and `--workers 8` is identical.

## Partitioning

Static split into `4 × workers` chunks; work stealing absorbs moderate
per-item skew. No minimum chunk size: per-chunk overhead (scratch realm
setup + clone) is microseconds, so over-chunking small heavy inputs costs
less than under-chunking them. Rayon-style lazy binary splitting is the
designed upgrade if benchmarks show extreme skew starving workers.

`--workers N` means N total executors: N−1 pool threads plus the calling
thread, which helps execute while it waits. N=1 is therefore truly serial —
the honest baseline for `speedup(N)`.

## Deferred

Cancellation, task scopes, timeouts → M3. Actors, channels, backpressure →
M2/M3. Priorities, NUMA, affinity → M10 phase. The scheduler design must not
preclude them (victim selection and queue structure stay pluggable).
