# Engine comparison: tscore vs node (V8) vs bun (JSC)

- date: 2026-09-13 04:34
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
| startup (hello.ts) | 1.7 (0.05x) | 37.5 (1.00x) | 4.2 (0.11x) | **tscore** |
| parse+compile (~50k LOC) | 8.7 (0.09x) | 99.5 (1.00x) | 11.3 (0.11x) | **tscore** |

## Steady-state (shared sources, in-program TIME_MS)

| benchmark | tscore | node | bun | winner | peak RSS MB (tscore / node / bun) |
|---|---|---|---|---|---|
| objects | 32.9 (1.40x) | 23.4 (1.00x) | 22.8 (0.97x) | **bun** | 5 / 75 / 22 |
| closures | 57.8 (1.68x) | 34.5 (1.00x) | 46.9 (1.36x) | **node** | 22 / 83 / 45 |
| alloc | 37.6 (0.86x) | 43.5 (1.00x) | 45.0 (1.03x) | **tscore** | 21 / 79 / 32 |
| gc_churn | 73.3 (1.41x) | 52.0 (1.00x) | 41.6 (0.80x) | **bun** | 370 / 499 / 355 |
| promises | 56.1 (0.89x) | 62.8 (1.00x) | 56.0 (0.89x) | **bun** | 30 / 83 / 34 |

## Async (per-engine variants, same algorithm)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| timer storm (2000x sleep 1ms) | 10.3 (0.84x) | 12.3 (1.00x) | 12.2 (0.99x) | **tscore** |
| channel 100k msgs (vs worker postMessage) | 5.1 (0.15x) | 34.5 (1.00x) | 28.4 (0.82x) | **tscore** |

## Long-running (30s sustained mixed compute+alloc, single run)

| metric | tscore | node | bun | winner |
|---|---|---|---|---|
| throughput (ops/sec) | 918275 (0.92x) | 992955 (1.00x) | 758758 (0.76x) | **node** |
| stability (last/first decile) | 0.935 | 1.015 | 0.98 | |

## Multicore (tscore parallel.map vs node/bun worker_threads)

### primes

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 48.1 (0.81x) | 59.4 (1.00x) | 34.4 (0.58x) | **bun** |
| 2 | 26.6 (0.62x) | 43.1 (1.00x) | 23.8 (0.55x) | **bun** |
| 4 | 14.0 (0.47x) | 30.0 (1.00x) | 16.7 (0.56x) | **tscore** |
| 8 | 8.0 (0.32x) | 24.8 (1.00x) | 14.0 (0.57x) | **tscore** |

### mandelbrot

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 24.6 (0.74x) | 33.1 (1.00x) | 29.8 (0.90x) | **tscore** |
| 2 | 12.9 (0.55x) | 23.2 (1.00x) | 19.9 (0.86x) | **tscore** |
| 4 | 7.1 (0.32x) | 21.9 (1.00x) | 18.2 (0.83x) | **tscore** |
| 8 | 4.3 (0.20x) | 21.7 (1.00x) | 16.2 (0.75x) | **tscore** |

## Scoreboard

| engine | wins |
|---|---|
| tscore | 11 |
| node | 2 |
| bun | 5 |

Win = fastest median (highest throughput for long-running) on that row.


## HTTP hello (wrk -t2 -c32 -d5s, best of 3)

| engine | req/s | cores | req/s per core |
|---|---|---|---|
| tscore workers:1 | 221,626 | 1.0 | 222,343 **best/core** |
| bun x1 | 196,753 | 1.0 | 191,024 |
| bun x8 reusePort (darwin: 1 active, 7 idle) | 196,885 | 1.0 | 189,975 |
| tscore workers:8 | 250,140 | 1.6 | 153,415 |
| node x1 | 147,478 | 1.0 | 144,821 |
| node cluster x8 | 196,050 | 3.8 | 52,252 |

Total throughput is capped by this machine's loopback stack, not
by any engine: four independent servers with four independent
clients total the same as one. Compare the per-core column.

Each engine at its best of 3 sweeps. Contention only ever
costs throughput, so the maximum is the estimate that converges on
the engine while a median would track how busy the box was. The
spread says how much it moved underneath the table:

| engine | min | max |
|---|---|---|
| tscore workers:1 | 219,817 | 221,626 |
| bun x1 | 192,784 | 196,753 |
| bun x8 reusePort (darwin: 1 active, 7 idle) | 185,725 | 196,885 |
| tscore workers:8 | 222,311 | 250,140 |
| node x1 | 127,107 | 147,478 |
| node cluster x8 | 176,249 | 196,050 |

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
