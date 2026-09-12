# Engine comparison: tscore vs node (V8) vs bun (JSC)

- date: 2026-09-12 12:52
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
| startup (hello.ts) | 1.8 (0.05x) | 38.5 (1.00x) | 4.4 (0.11x) | **tscore** |
| parse+compile (~50k LOC) | 9.1 (0.09x) | 101.3 (1.00x) | 11.6 (0.11x) | **tscore** |

## Steady-state (shared sources, in-program TIME_MS)

| benchmark | tscore | node | bun | winner | peak RSS MB (tscore / node / bun) |
|---|---|---|---|---|---|
| objects | 32.9 (1.37x) | 24.1 (1.00x) | 23.2 (0.96x) | **bun** | 5 / 74 / 22 |
| closures | 57.7 (1.67x) | 34.5 (1.00x) | 50.2 (1.46x) | **node** | 23 / 83 / 44 |
| alloc | 37.3 (0.87x) | 43.0 (1.00x) | 46.9 (1.09x) | **tscore** | 21 / 79 / 32 |
| gc_churn | 73.7 (1.33x) | 55.2 (1.00x) | 43.0 (0.78x) | **bun** | 370 / 500 / 355 |
| promises | 54.7 (0.87x) | 63.1 (1.00x) | 58.5 (0.93x) | **tscore** | 30 / 83 / 34 |

## Async (per-engine variants, same algorithm)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| timer storm (2000x sleep 1ms) | 10.3 (0.78x) | 13.2 (1.00x) | 12.0 (0.91x) | **tscore** |
| channel 100k msgs (vs worker postMessage) | 5.1 (0.15x) | 34.9 (1.00x) | 29.5 (0.85x) | **tscore** |

## Long-running (30s sustained mixed compute+alloc, single run)

| metric | tscore | node | bun | winner |
|---|---|---|---|---|
| throughput (ops/sec) | 816531 (0.81x) | 1012539 (1.00x) | 810491 (0.80x) | **node** |
| stability (last/first decile) | 1 | 1.005 | 1.015 | |

## Multicore (tscore parallel.map vs node/bun worker_threads)

### primes

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 48.5 (0.83x) | 58.5 (1.00x) | 34.6 (0.59x) | **bun** |
| 2 | 26.7 (0.63x) | 42.1 (1.00x) | 23.9 (0.57x) | **bun** |
| 4 | 14.2 (0.47x) | 30.4 (1.00x) | 16.6 (0.55x) | **tscore** |
| 8 | 7.9 (0.30x) | 25.9 (1.00x) | 14.5 (0.56x) | **tscore** |

### mandelbrot

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 24.9 (0.75x) | 33.3 (1.00x) | 32.0 (0.96x) | **tscore** |
| 2 | 13.0 (0.53x) | 24.7 (1.00x) | 20.2 (0.82x) | **tscore** |
| 4 | 7.3 (0.32x) | 22.9 (1.00x) | 18.4 (0.80x) | **tscore** |
| 8 | 5.2 (0.23x) | 22.6 (1.00x) | 17.2 (0.76x) | **tscore** |

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
| tscore workers:1 | 206,282 | 1.0 | 207,322 **best/core** |
| bun x8 reusePort (darwin: 1 active, 7 idle) | 188,674 | 1.0 | 182,505 |
| bun x1 | 183,413 | 1.0 | 178,730 |
| node x1 | 142,490 | 1.0 | 139,886 |
| tscore workers:8 | 239,676 | 1.9 | 125,637 |
| node cluster x8 | 189,202 | 4.0 | 47,880 |

Total throughput is capped by this machine's loopback stack, not
by any engine: four independent servers with four independent
clients total the same as one. Compare the per-core column.

Each engine at its best of 3 sweeps. Contention only ever
costs throughput, so the maximum is the estimate that converges on
the engine while a median would track how busy the box was. The
spread says how much it moved underneath the table:

| engine | min | max |
|---|---|---|
| tscore workers:1 | 204,360 | 206,282 |
| bun x8 reusePort (darwin: 1 active, 7 idle) | 170,361 | 188,674 |
| bun x1 | 175,892 | 183,413 |
| node x1 | 137,498 | 142,490 |
| tscore workers:8 | 224,304 | 239,676 |
| node cluster x8 | 163,852 | 189,202 |

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
