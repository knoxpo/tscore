# TypeScript Core

A multicore-native TypeScript runtime. Research prototype.

> Make parallel execution in TypeScript as natural as asynchronous execution
> became with async/await.

```typescript
const results = await parallel.map(records, processRecord);
// runtime handles: partitioning, work stealing, core placement, isolation
```

**Status:** M1 "Multicore Proof" complete — custom bytecode interpreter
(register machine, per-realm isolated heaps, mark-sweep GC) + work-stealing
scheduler, ARM64 macOS/Linux.

## M1 results (Apple M5 Max, median of runs)

| workload | 2 cores | 4 cores | 8 cores | target (2/4/8) |
|---|---|---|---|---|
| primes | 1.86× | 3.58× | 5.95× | ≥1.7 / 3.0 / 5.5 |
| fnv | 1.97× | 3.93× | 6.37× | ✓ |
| mandelbrot | 1.98× | 3.72× | 5.99× | ✓ |

Output is bit-identical at every worker count (clone-at-spawn semantics).
Single-core absolute speed vs V8 is an explicit non-goal at M1 — the claim
under test is the multicore programming model; see
[benchmark methodology](docs/architecture/benchmark-methodology.md).

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
