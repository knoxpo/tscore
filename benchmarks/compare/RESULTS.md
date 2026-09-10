# Engine comparison: tscore vs node (V8) vs bun (JSC)

- date: 2026-09-10 17:49
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
| startup (hello.ts) | 2.0 (0.05x) | 40.2 (1.00x) | 5.4 (0.13x) | **tscore** |
| parse+compile (~50k LOC) | 9.7 (0.09x) | 104.1 (1.00x) | 12.8 (0.12x) | **tscore** |

## Steady-state (shared sources, in-program TIME_MS)

| benchmark | tscore | node | bun | winner | peak RSS MB (tscore / node / bun) |
|---|---|---|---|---|---|
| objects | 47.8 (2.00x) | 23.9 (1.00x) | 23.3 (0.97x) | **bun** | 5 / 75 / 22 |
| closures | 60.6 (1.72x) | 35.3 (1.00x) | 50.2 (1.42x) | **node** | 23 / 83 / 44 |
| alloc | 41.1 (0.94x) | 43.6 (1.00x) | 48.0 (1.10x) | **tscore** | 21 / 79 / 32 |
| gc_churn | 76.4 (1.19x) | 64.1 (1.00x) | 46.0 (0.72x) | **bun** | 370 / 497 / 355 |
| promises | 57.8 (0.89x) | 64.7 (1.00x) | 59.1 (0.91x) | **tscore** | 30 / 83 / 34 |

## Async (per-engine variants, same algorithm)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| timer storm (2000x sleep 1ms) | 10.3 (0.77x) | 13.4 (1.00x) | 12.0 (0.90x) | **tscore** |
| channel 100k msgs (vs worker postMessage) | 5.4 (0.15x) | 35.0 (1.00x) | 30.0 (0.86x) | **tscore** |

## Long-running (30s sustained mixed compute+alloc, single run)

| metric | tscore | node | bun | winner |
|---|---|---|---|---|
| throughput (ops/sec) | 800844 (0.80x) | 1005722 (1.00x) | 767384 (0.76x) | **node** |
| stability (last/first decile) | 1.001 | 1.018 | 1.007 | |

## Multicore (tscore parallel.map vs node/bun worker_threads)

### primes

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 51.8 (0.79x) | 65.7 (1.00x) | 36.2 (0.55x) | **bun** |
| 2 | 27.4 (0.62x) | 44.3 (1.00x) | 25.2 (0.57x) | **bun** |
| 4 | 15.2 (0.41x) | 36.8 (1.00x) | 17.6 (0.48x) | **tscore** |
| 8 | 9.3 (0.34x) | 27.6 (1.00x) | 15.3 (0.55x) | **tscore** |

### mandelbrot

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 25.3 (0.71x) | 35.8 (1.00x) | 31.5 (0.88x) | **tscore** |
| 2 | 13.0 (0.49x) | 26.2 (1.00x) | 20.5 (0.78x) | **tscore** |
| 4 | 7.2 (0.30x) | 23.7 (1.00x) | 19.9 (0.84x) | **tscore** |
| 8 | 4.7 (0.20x) | 23.0 (1.00x) | 17.8 (0.77x) | **tscore** |

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
| tscore workers:1 | 198,833 | 1.0 | 202,291 **best/core** |
| bun x1 | 179,099 | 1.0 | 175,168 |
| bun x8 reusePort (darwin: 1 active, 7 idle) | 178,231 | 1.0 | 173,393 |
| node x1 | 135,671 | 1.0 | 134,258 |
| tscore workers:8 | 227,317 | 1.9 | 117,567 |
| node cluster x8 | 175,644 | 4.1 | 43,134 |

Total throughput is capped by this machine's loopback stack, not
by any engine: four independent servers with four independent
clients total the same as one. Compare the per-core column.

Each engine at its best of 3 sweeps. Contention only ever
costs throughput, so the maximum is the estimate that converges on
the engine while a median would track how busy the box was. The
spread says how much it moved underneath the table:

| engine | min | max |
|---|---|---|
| tscore workers:1 | 195,221 | 198,833 |
| bun x1 | 174,255 | 179,099 |
| bun x8 reusePort (darwin: 1 active, 7 idle) | 167,074 | 178,231 |
| node x1 | 134,636 | 135,671 |
| tscore workers:8 | 216,249 | 227,317 |
| node cluster x8 | 164,681 | 175,644 |

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
