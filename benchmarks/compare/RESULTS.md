# Engine comparison: tscore vs node (V8) vs bun (JSC)

- date: 2026-09-11 22:58
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
| startup (hello.ts) | 1.8 (0.05x) | 38.4 (1.00x) | 4.5 (0.12x) | **tscore** |
| parse+compile (~50k LOC) | 9.1 (0.09x) | 99.3 (1.00x) | 11.8 (0.12x) | **tscore** |

## Steady-state (shared sources, in-program TIME_MS)

| benchmark | tscore | node | bun | winner | peak RSS MB (tscore / node / bun) |
|---|---|---|---|---|---|
| objects | 32.9 (1.40x) | 23.5 (1.00x) | 23.3 (0.99x) | **bun** | 5 / 75 / 22 |
| closures | 56.9 (1.66x) | 34.3 (1.00x) | 47.5 (1.39x) | **node** | 23 / 83 / 44 |
| alloc | 37.9 (0.89x) | 42.7 (1.00x) | 47.4 (1.11x) | **tscore** | 21 / 79 / 32 |
| gc_churn | 73.9 (1.40x) | 52.7 (1.00x) | 43.6 (0.83x) | **bun** | 370 / 497 / 355 |
| promises | 54.6 (0.87x) | 62.9 (1.00x) | 57.9 (0.92x) | **tscore** | 30 / 83 / 34 |

## Async (per-engine variants, same algorithm)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| timer storm (2000x sleep 1ms) | 10.3 (0.84x) | 12.2 (1.00x) | 12.0 (0.99x) | **tscore** |
| channel 100k msgs (vs worker postMessage) | 5.0 (0.15x) | 33.5 (1.00x) | 28.7 (0.86x) | **tscore** |

## Long-running (30s sustained mixed compute+alloc, single run)

| metric | tscore | node | bun | winner |
|---|---|---|---|---|
| throughput (ops/sec) | 816101 (0.80x) | 1017972 (1.00x) | 751374 (0.74x) | **node** |
| stability (last/first decile) | 0.993 | 1.018 | 0.939 | |

## Multicore (tscore parallel.map vs node/bun worker_threads)

### primes

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 49.4 (0.82x) | 60.1 (1.00x) | 34.5 (0.57x) | **bun** |
| 2 | 26.5 (0.61x) | 43.7 (1.00x) | 24.7 (0.57x) | **bun** |
| 4 | 14.1 (0.47x) | 30.3 (1.00x) | 16.8 (0.55x) | **tscore** |
| 8 | 8.2 (0.32x) | 25.5 (1.00x) | 15.1 (0.59x) | **tscore** |

### mandelbrot

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 25.1 (0.74x) | 33.8 (1.00x) | 30.2 (0.89x) | **tscore** |
| 2 | 13.2 (0.54x) | 24.5 (1.00x) | 20.3 (0.83x) | **tscore** |
| 4 | 7.6 (0.33x) | 22.9 (1.00x) | 18.8 (0.82x) | **tscore** |
| 8 | 5.3 (0.24x) | 22.3 (1.00x) | 16.3 (0.73x) | **tscore** |

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
| tscore workers:1 | 203,720 | 1.0 | 205,572 **best/core** |
| bun x8 reusePort (darwin: 1 active, 7 idle) | 183,499 | 1.0 | 177,845 |
| bun x1 | 181,899 | 1.0 | 177,232 |
| node x1 | 139,949 | 1.0 | 137,944 |
| tscore workers:8 | 234,741 | 1.9 | 124,891 |
| node cluster x8 | 183,745 | 4.0 | 46,226 |

Total throughput is capped by this machine's loopback stack, not
by any engine: four independent servers with four independent
clients total the same as one. Compare the per-core column.

Each engine at its best of 3 sweeps. Contention only ever
costs throughput, so the maximum is the estimate that converges on
the engine while a median would track how busy the box was. The
spread says how much it moved underneath the table:

| engine | min | max |
|---|---|---|
| tscore workers:1 | 201,563 | 203,720 |
| bun x8 reusePort (darwin: 1 active, 7 idle) | 144,709 | 183,499 |
| bun x1 | 166,521 | 181,899 |
| node x1 | 138,625 | 139,949 |
| tscore workers:8 | 229,344 | 234,741 |
| node cluster x8 | 179,261 | 183,745 |

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
