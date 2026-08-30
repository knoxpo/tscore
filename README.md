# TypeScript Core

A multicore-native TypeScript runtime. Research prototype.

> Make parallel execution in TypeScript as natural as asynchronous execution
> became with async/await.

```typescript
const results = await parallel.map(records, processRecord);
// runtime handles: partitioning, work stealing, core placement, isolation
```

**Status:** M4 "GC + Memory Runtime" complete — generational GC (O(young)
minor pauses), memory limits, runtime telemetry, and a live `tscore top`
monitor (M1 Multicore Proof, M2 Actors, M3 incl. async/await before it) — custom bytecode interpreter (register machine, per-realm
isolated heaps, mark-sweep GC), work-stealing scheduler, actor runtime.
ARM64 macOS/Linux.

## M1 results (Apple M5 Max, median of runs)

| workload | 2 cores | 4 cores | 8 cores | target (2/4/8) |
|---|---|---|---|---|
| primes | 1.88× | 3.73× | 6.79× | ≥1.7 / 3.0 / 5.5 |
| fnv | 1.84× | 3.65× | 5.69× | ✓ |
| mandelbrot | 1.97× | 3.71× | 6.43× | ✓ |

Output is bit-identical at every worker count (clone-at-spawn semantics).
Single-core absolute speed vs V8 is an explicit non-goal at M1 — the claim
under test is the multicore programming model; see
[benchmark methodology](docs/architecture/benchmark-methodology.md).

## Actors (M2)

```typescript
const counter = actor(() => {
    let n = 0;
    return {
        add: (msg) => { n = n + msg.by; },
        get: () => n,
    };
});
counter.post({ type: "add", by: 2 });   // fire-and-forget
counter.send({ type: "get" });          // blocking reply
```

Each actor owns an isolated realm (heap + GC) on a dedicated thread with a
bounded mailbox; messages cross realms as structured clones, handler
failures stay inside the actor. See
[actors spec](docs/specifications/actors-m2.md).

## Async/await + channels (M3 completion)

```typescript
const hashes = await parallel.map(seeds, hashStream);   // spec API, for real
const reply  = await db.send({ type: "get-user", id: 42 });
const ch = Channel.create({ capacity: 1024 });
await Channel.send(ch, event);                          // backpressure: parks when full
await sleep(100);
```

Top-level await, async functions/arrows, async actor handlers, channels
portable across realms. See [async spec](docs/specifications/async-await-m3.md).

## Structured concurrency (M3)

```typescript
const total = task.scope((scope) => {
    const a = scope.spawn(() => heavyA());
    const b = scope.spawn(() => heavyB());
    return a.join() + b.join();      // scope exits only when ALL children did
}, { timeout: 5000 });
```

Child errors cancel siblings and propagate to the parent; cancellation is
cooperative at interpreter safepoints (`handle.cancel()`, scope timeouts,
`Runtime.checkCancellation()`), and cascades through nested scopes. See
[structured concurrency spec](docs/specifications/structured-concurrency-m3.md).

## GC + observability (M4)

Sticky generational mark-sweep: minor pauses are O(young) and independent
of heap size (measured ~2ms minors vs ~10ms majors on a 2M-object live
set). `runtime.gc.stats()`, `--max-heap` limits with clean OOM errors,
`tscore run --stats`, and `tscore top` — a live table of every running
tscore process. See [GC spec](docs/specifications/gc-m4.md).

## Try it

```bash
cargo run --release -p tscore -- run examples/parallel_primes.ts
```

```bash
./benchmarks/run.sh
```

## Docs

- [Architecture overview](docs/architecture/00-overview.md)
- [M1 language subset](docs/specifications/language-subset-m1.md)
- [Bytecode](docs/specifications/bytecode.md)
- [Concurrency semantics](docs/specifications/concurrency-semantics.md)
- [Memory & ownership](docs/specifications/memory-ownership.md)
- [Benchmark methodology](docs/architecture/benchmark-methodology.md)
