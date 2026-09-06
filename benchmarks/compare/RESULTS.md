# Engine comparison: tscore vs node (V8) vs bun (JSC)

- date: 2026-09-07 01:21
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
| startup (hello.ts) | 1.6 (0.04x) | 37.1 (1.00x) | 4.2 (0.11x) | **tscore** |
| parse+compile (~50k LOC) | 8.4 (0.08x) | 99.0 (1.00x) | 12.2 (0.12x) | **tscore** |

## Steady-state (shared sources, in-program TIME_MS)

| benchmark | tscore | node | bun | winner | peak RSS MB (tscore / node / bun) |
|---|---|---|---|---|---|
| objects | 66.2 (2.68x) | 24.7 (1.00x) | 23.5 (0.95x) | **bun** | 4 / 75 / 22 |
| closures | 66.8 (1.82x) | 36.6 (1.00x) | 48.4 (1.32x) | **node** | 23 / 83 / 45 |
| alloc | 79.6 (1.83x) | 43.4 (1.00x) | 50.2 (1.16x) | **node** | 57 / 79 / 32 |
| gc_churn | 131.6 (2.22x) | 59.2 (1.00x) | 44.8 (0.76x) | **bun** | 416 / 500 / 355 |
| promises | 57.3 (0.84x) | 68.4 (1.00x) | 66.8 (0.98x) | **tscore** | 29 / 83 / 34 |

## Async (per-engine variants, same algorithm)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| timer storm (2000x sleep 1ms) | 10.3 (0.78x) | 13.1 (1.00x) | 12.0 (0.92x) | **tscore** |
| channel 100k msgs (vs worker postMessage) | 5.5 (0.15x) | 35.9 (1.00x) | 29.6 (0.82x) | **tscore** |

## Long-running (30s sustained mixed compute+alloc, single run)

| metric | tscore | node | bun | winner |
|---|---|---|---|---|
| throughput (ops/sec) | 789789 (0.81x) | 980158 (1.00x) | 782213 (0.80x) | **node** |
| stability (last/first decile) | 1.024 | 1.021 | 0.968 | |

## Multicore (tscore parallel.map vs node/bun worker_threads)

### primes

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 52.1 (0.81x) | 64.7 (1.00x) | 36.9 (0.57x) | **bun** |
| 2 | 27.4 (0.57x) | 47.8 (1.00x) | 25.0 (0.52x) | **bun** |
| 4 | 14.3 (0.43x) | 33.0 (1.00x) | 17.6 (0.53x) | **tscore** |
| 8 | 8.7 (0.33x) | 26.6 (1.00x) | 14.8 (0.56x) | **tscore** |

### mandelbrot

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 25.2 (0.71x) | 35.3 (1.00x) | 31.6 (0.89x) | **tscore** |
| 2 | 12.9 (0.53x) | 24.3 (1.00x) | 20.5 (0.84x) | **tscore** |
| 4 | 6.7 (0.30x) | 22.4 (1.00x) | 18.4 (0.82x) | **tscore** |
| 8 | 3.9 (0.16x) | 23.5 (1.00x) | 16.6 (0.71x) | **tscore** |

## Scoreboard

| engine | wins |
|---|---|
| tscore | 11 |
| node | 3 |
| bun | 4 |

Win = fastest median (highest throughput for long-running) on that row.


## HTTP hello (wrk -t2 -c32 -d5s, best of 3)

| engine | req/s | cores | req/s per core |
|---|---|---|---|
| tscore workers:1 | 204,001 | 1.0 | 205,415 **best/core** |
| bun x1 | 186,410 | 1.0 | 181,316 |
| bun x8 reusePort (darwin: 1 active, 7 idle) | 185,220 | 1.0 | 178,702 |
| node x1 | 142,923 | 1.0 | 140,565 |
| tscore workers:8 | 231,728 | 2.0 | 118,596 |
| node cluster x8 | 186,673 | 3.9 | 47,953 |

Total throughput is capped by this machine's loopback stack, not
by any engine: four independent servers with four independent
clients total the same as one. Compare the per-core column.

Each engine at its best of 3 sweeps. Contention only ever
costs throughput, so the maximum is the estimate that converges on
the engine while a median would track how busy the box was. The
spread says how much it moved underneath the table:

| engine | min | max |
|---|---|---|
| tscore workers:1 | 197,356 | 204,001 |
| bun x1 | 178,929 | 186,410 |
| bun x8 reusePort (darwin: 1 active, 7 idle) | 182,897 | 185,220 |
| node x1 | 135,436 | 142,923 |
| tscore workers:8 | 196,048 | 231,728 |
| node cluster x8 | 180,332 | 186,673 |

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
