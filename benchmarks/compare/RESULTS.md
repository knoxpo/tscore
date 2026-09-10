# Engine comparison: tscore vs node (V8) vs bun (JSC)

- date: 2026-09-10 15:15
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
| startup (hello.ts) | 1.7 (0.04x) | 38.3 (1.00x) | 4.4 (0.11x) | **tscore** |
| parse+compile (~50k LOC) | 9.1 (0.09x) | 101.1 (1.00x) | 11.5 (0.11x) | **tscore** |

## Steady-state (shared sources, in-program TIME_MS)

| benchmark | tscore | node | bun | winner | peak RSS MB (tscore / node / bun) |
|---|---|---|---|---|---|
| objects | 51.8 (2.17x) | 23.8 (1.00x) | 23.0 (0.96x) | **bun** | 5 / 75 / 22 |
| closures | 59.6 (1.73x) | 34.5 (1.00x) | 49.1 (1.43x) | **node** | 22 / 83 / 45 |
| alloc | 40.6 (0.93x) | 43.5 (1.00x) | 48.0 (1.10x) | **tscore** | 21 / 79 / 32 |
| gc_churn | 75.5 (1.35x) | 56.0 (1.00x) | 45.8 (0.82x) | **bun** | 370 / 499 / 355 |
| promises | 56.7 (0.90x) | 63.4 (1.00x) | 60.3 (0.95x) | **tscore** | 30 / 83 / 34 |

## Async (per-engine variants, same algorithm)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| timer storm (2000x sleep 1ms) | 10.2 (0.87x) | 11.8 (1.00x) | 11.7 (1.00x) | **tscore** |
| channel 100k msgs (vs worker postMessage) | 5.4 (0.15x) | 36.2 (1.00x) | 28.8 (0.80x) | **tscore** |

## Long-running (30s sustained mixed compute+alloc, single run)

| metric | tscore | node | bun | winner |
|---|---|---|---|---|
| throughput (ops/sec) | 789520 (0.80x) | 984600 (1.00x) | 791818 (0.80x) | **node** |
| stability (last/first decile) | 1.005 | 1.017 | 0.997 | |

## Multicore (tscore parallel.map vs node/bun worker_threads)

### primes

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 51.1 (0.85x) | 59.9 (1.00x) | 35.2 (0.59x) | **bun** |
| 2 | 27.2 (0.63x) | 43.3 (1.00x) | 24.7 (0.57x) | **bun** |
| 4 | 14.6 (0.48x) | 30.6 (1.00x) | 16.5 (0.54x) | **tscore** |
| 8 | 8.7 (0.34x) | 25.7 (1.00x) | 14.4 (0.56x) | **tscore** |

### mandelbrot

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 24.8 (0.74x) | 33.7 (1.00x) | 30.6 (0.91x) | **tscore** |
| 2 | 12.9 (0.54x) | 24.0 (1.00x) | 20.2 (0.84x) | **tscore** |
| 4 | 7.1 (0.32x) | 22.3 (1.00x) | 18.3 (0.82x) | **tscore** |
| 8 | 4.3 (0.19x) | 22.7 (1.00x) | 16.3 (0.72x) | **tscore** |

## Scoreboard

| engine | wins |
|---|---|
| tscore | 12 |
| node | 2 |
| bun | 4 |

Win = fastest median (highest throughput for long-running) on that row.


## HTTP hello (wrk -t2 -c32 -d5s, best of 3)

| engine | req/s | cores | req/s per core |
|---|---|---|---|
| tscore workers:1 | 204,910 | 1.0 | 205,485 **best/core** |
| bun x1 | 183,929 | 1.0 | 178,876 |
| bun x8 reusePort (darwin: 1 active, 7 idle) | 183,138 | 1.0 | 176,392 |
| node x1 | 140,797 | 1.0 | 138,836 |
| tscore workers:8 | 235,450 | 1.8 | 129,941 |
| node cluster x8 | 166,594 | 4.0 | 41,795 |

Total throughput is capped by this machine's loopback stack, not
by any engine: four independent servers with four independent
clients total the same as one. Compare the per-core column.

Each engine at its best of 3 sweeps. Contention only ever
costs throughput, so the maximum is the estimate that converges on
the engine while a median would track how busy the box was. The
spread says how much it moved underneath the table:

| engine | min | max |
|---|---|---|
| tscore workers:1 | 196,779 | 204,910 |
| bun x1 | 177,084 | 183,929 |
| bun x8 reusePort (darwin: 1 active, 7 idle) | 178,312 | 183,138 |
| node x1 | 138,506 | 140,797 |
| tscore workers:8 | 204,689 | 235,450 |
| node cluster x8 | 165,883 | 166,594 |

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
