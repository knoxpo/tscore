# Engine comparison: tscore vs node (V8) vs bun (JSC)

- date: 2026-09-07 02:40
- machine: Apple M5 Max, 18 logical cpus (6P + 12E)
- tscore: tscore 0.1.0, node: v24.17.0, bun: 1.4.0
- steady-state cells: median TIME_MS of 5 runs, first run discarded; in-program warmup pass before timing
- peak RSS: maximum resident set size of one run per engine (/usr/bin/time -l), MB
- ratios: engine_ms / node_ms — lower is better, <1.00x is faster than node
- methodology: shared benchmarks are byte-identical sources in the tscore language subset;
  node/bun run non-idiomatic code (no Array.map, classes, etc.) — this compares engines on
  identical programs, not idiomatic-per-engine programs

## Startup + parse/compile (end-to-end, hyperfine)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| startup (hello.ts) | 1.9 (0.05x) | 38.9 (1.00x) | 4.8 (0.12x) | **tscore** |
| parse+compile (~50k LOC) | 8.8 (0.08x) | 107.7 (1.00x) | 13.3 (0.12x) | **tscore** |

## Steady-state (shared sources, in-program TIME_MS)

| benchmark | tscore | node | bun | winner | peak RSS MB (tscore / node / bun) |
|---|---|---|---|---|---|
| objects | 61.7 (2.55x) | 24.2 (1.00x) | 24.2 (1.00x) | **node** | 5 / 74 / 22 |
| closures | 64.4 (1.83x) | 35.3 (1.00x) | 49.0 (1.39x) | **node** | 22 / 83 / 44 |
| alloc | 53.6 (1.22x) | 43.9 (1.00x) | 48.3 (1.10x) | **node** | 21 / 79 / 32 |
| gc_churn | 92.2 (1.64x) | 56.2 (1.00x) | 46.1 (0.82x) | **bun** | 370 / 490 / 355 |
| promises | 59.7 (0.89x) | 67.1 (1.00x) | 63.8 (0.95x) | **tscore** | 30 / 83 / 34 |

## Async (per-engine variants, same algorithm)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| timer storm (2000x sleep 1ms) | 10.3 (0.79x) | 13.0 (1.00x) | 11.8 (0.91x) | **tscore** |
| channel 100k msgs (vs worker postMessage) | 5.5 (0.15x) | 36.6 (1.00x) | 29.9 (0.82x) | **tscore** |

## Long-running (30s sustained mixed compute+alloc, single run)

| metric | tscore | node | bun | winner |
|---|---|---|---|---|
| throughput (ops/sec) | 786148 (0.78x) | 1003309 (1.00x) | 736818 (0.73x) | **node** |
| stability (last/first decile) | 1.011 | 1.011 | 1.055 | |

## Multicore (tscore parallel.map vs node/bun worker_threads)

### primes

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 52.0 (0.86x) | 60.6 (1.00x) | 34.5 (0.57x) | **bun** |
| 2 | 27.1 (0.65x) | 41.9 (1.00x) | 24.3 (0.58x) | **bun** |
| 4 | 14.3 (0.47x) | 30.1 (1.00x) | 16.6 (0.55x) | **tscore** |
| 8 | 8.5 (0.35x) | 24.5 (1.00x) | 13.9 (0.57x) | **tscore** |

### mandelbrot

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 24.9 (0.74x) | 33.7 (1.00x) | 30.8 (0.91x) | **tscore** |
| 2 | 12.9 (0.55x) | 23.6 (1.00x) | 20.1 (0.85x) | **tscore** |
| 4 | 6.9 (0.31x) | 22.2 (1.00x) | 18.4 (0.83x) | **tscore** |
| 8 | 4.4 (0.19x) | 22.5 (1.00x) | 16.6 (0.74x) | **tscore** |

## Scoreboard

| engine | wins |
|---|---|
| tscore | 11 |
| node | 4 |
| bun | 3 |

Win = fastest median (highest throughput for long-running) on that row.


## HTTP hello (wrk -t2 -c32 -d5s, best of 3)

| engine | req/s | cores | req/s per core |
|---|---|---|---|
| tscore workers:1 | 204,429 | 1.0 | 206,311 **best/core** |
| bun x8 reusePort (darwin: 1 active, 7 idle) | 187,914 | 1.0 | 181,655 |
| bun x1 | 185,648 | 1.0 | 180,675 |
| node x1 | 143,216 | 1.0 | 140,926 |
| tscore workers:8 | 233,211 | 1.9 | 121,133 |
| node cluster x8 | 188,415 | 3.9 | 47,868 |

Total throughput is capped by this machine's loopback stack, not
by any engine: four independent servers with four independent
clients total the same as one. Compare the per-core column.

Each engine at its best of 3 sweeps. Contention only ever
costs throughput, so the maximum is the estimate that converges on
the engine while a median would track how busy the box was. The
spread says how much it moved underneath the table:

| engine | min | max |
|---|---|---|
| tscore workers:1 | 183,163 | 204,429 |
| bun x8 reusePort (darwin: 1 active, 7 idle) | 120,542 | 187,914 |
| bun x1 | 162,191 | 185,648 |
| node x1 | 123,156 | 143,216 |
| tscore workers:8 | 156,744 | 233,211 |
| node cluster x8 | 178,387 | 188,415 |

If an engine changes places between runs, the per-core ordering is
an artefact of when the sweep landed and the numbers are not usable.

bun has no working multi-core HTTP on darwin: SO_REUSEPORT there
permits the shared bind but does not distribute, so one of the 8
processes serves every connection and the rest idle. Its row is a
second bun x1 measurement, not a multi-core one. node's cluster
distributes in userspace via the primary, and tscore's workers
share one listener with a kqueue per thread; both scale here.
## Runtime surface

| category | status |
|---|---|
| modules | SHIPPED — full ESM: static + dynamic import, cycles (TDZ), TLA; bare specifiers via tscore.json and node_modules (package.json exports/main); .js/.mjs/.cjs specifiers resolve to TS sources; import.meta.url/filename/dirname |
| networking | SHIPPED — runtime.net TCP (kqueue reactor, connect) + runtime.http |
| async file I/O | SHIPPED — runtime.fs (promise-native, dedicated I/O pool) + bytes |
| http throughput | see the HTTP section above (benchmarks/compare/http_bench.py) |
| async I/O | proxied by timer-storm + channel benchmarks above |
