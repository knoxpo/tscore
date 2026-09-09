# Engine comparison: tscore vs node (V8) vs bun (JSC)

- date: 2026-09-10 03:30
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
| startup (hello.ts) | 1.8 (0.05x) | 39.1 (1.00x) | 4.9 (0.13x) | **tscore** |
| parse+compile (~50k LOC) | 9.4 (0.09x) | 102.7 (1.00x) | 12.6 (0.12x) | **tscore** |

## Steady-state (shared sources, in-program TIME_MS)

| benchmark | tscore | node | bun | winner | peak RSS MB (tscore / node / bun) |
|---|---|---|---|---|---|
| objects | 55.6 (2.33x) | 23.8 (1.00x) | 23.1 (0.97x) | **bun** | 5 / 75 / 22 |
| closures | 60.1 (1.73x) | 34.8 (1.00x) | 48.2 (1.39x) | **node** | 22 / 83 / 45 |
| alloc | 40.9 (0.94x) | 43.4 (1.00x) | 48.0 (1.11x) | **tscore** | 21 / 79 / 32 |
| gc_churn | 78.0 (1.38x) | 56.4 (1.00x) | 44.1 (0.78x) | **bun** | 370 / 498 / 355 |
| promises | 57.3 (0.88x) | 64.8 (1.00x) | 59.0 (0.91x) | **tscore** | 30 / 83 / 34 |

## Async (per-engine variants, same algorithm)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| timer storm (2000x sleep 1ms) | 10.3 (0.76x) | 13.5 (1.00x) | 12.1 (0.90x) | **tscore** |
| channel 100k msgs (vs worker postMessage) | 5.5 (0.16x) | 35.1 (1.00x) | 29.7 (0.85x) | **tscore** |

## Long-running (30s sustained mixed compute+alloc, single run)

| metric | tscore | node | bun | winner |
|---|---|---|---|---|
| throughput (ops/sec) | 797637 (0.79x) | 1010473 (1.00x) | 799849 (0.79x) | **node** |
| stability (last/first decile) | 0.995 | 0.995 | 0.986 | |

## Multicore (tscore parallel.map vs node/bun worker_threads)

### primes

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 50.1 (0.74x) | 67.6 (1.00x) | 38.3 (0.57x) | **bun** |
| 2 | 27.7 (0.63x) | 43.7 (1.00x) | 25.9 (0.59x) | **bun** |
| 4 | 14.3 (0.47x) | 30.2 (1.00x) | 16.3 (0.54x) | **tscore** |
| 8 | 8.5 (0.33x) | 25.5 (1.00x) | 13.7 (0.54x) | **tscore** |

### mandelbrot

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 25.0 (0.75x) | 33.6 (1.00x) | 30.4 (0.91x) | **tscore** |
| 2 | 12.9 (0.54x) | 23.9 (1.00x) | 20.0 (0.84x) | **tscore** |
| 4 | 7.1 (0.32x) | 22.1 (1.00x) | 18.1 (0.82x) | **tscore** |
| 8 | 4.4 (0.20x) | 22.0 (1.00x) | 16.2 (0.74x) | **tscore** |

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
| tscore workers:1 | 207,503 | 1.0 | 208,541 **best/core** |
| bun x8 reusePort (darwin: 1 active, 7 idle) | 190,344 | 1.0 | 183,672 |
| bun x1 | 188,752 | 1.0 | 183,227 |
| node x1 | 143,680 | 1.0 | 141,348 |
| tscore workers:8 | 247,370 | 1.8 | 139,086 |
| node cluster x8 | 190,903 | 4.0 | 48,265 |

Total throughput is capped by this machine's loopback stack, not
by any engine: four independent servers with four independent
clients total the same as one. Compare the per-core column.

Each engine at its best of 3 sweeps. Contention only ever
costs throughput, so the maximum is the estimate that converges on
the engine while a median would track how busy the box was. The
spread says how much it moved underneath the table:

| engine | min | max |
|---|---|---|
| tscore workers:1 | 199,611 | 207,503 |
| bun x8 reusePort (darwin: 1 active, 7 idle) | 184,620 | 190,344 |
| bun x1 | 185,617 | 188,752 |
| node x1 | 138,057 | 143,680 |
| tscore workers:8 | 228,420 | 247,370 |
| node cluster x8 | 181,105 | 190,903 |

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
