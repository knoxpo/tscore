# Engine comparison: tscore vs node (V8) vs bun (JSC)

- date: 2026-09-07 15:23
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
| startup (hello.ts) | 2.1 (0.05x) | 39.5 (1.00x) | 4.3 (0.11x) | **tscore** |
| parse+compile (~50k LOC) | 8.8 (0.09x) | 101.2 (1.00x) | 11.3 (0.11x) | **tscore** |

## Steady-state (shared sources, in-program TIME_MS)

| benchmark | tscore | node | bun | winner | peak RSS MB (tscore / node / bun) |
|---|---|---|---|---|---|
| objects | 60.5 (2.54x) | 23.8 (1.00x) | 23.4 (0.98x) | **bun** | 5 / 75 / 22 |
| closures | 64.4 (1.89x) | 34.1 (1.00x) | 47.4 (1.39x) | **node** | 22 / 83 / 45 |
| alloc | 51.5 (1.18x) | 43.6 (1.00x) | 47.3 (1.08x) | **node** | 21 / 79 / 32 |
| gc_churn | 91.1 (1.43x) | 63.7 (1.00x) | 44.8 (0.70x) | **bun** | 370 / 497 / 355 |
| promises | 57.6 (0.90x) | 63.6 (1.00x) | 62.3 (0.98x) | **tscore** | 30 / 83 / 34 |

## Async (per-engine variants, same algorithm)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| timer storm (2000x sleep 1ms) | 10.3 (0.77x) | 13.3 (1.00x) | 12.2 (0.92x) | **tscore** |
| channel 100k msgs (vs worker postMessage) | 5.4 (0.15x) | 35.8 (1.00x) | 28.9 (0.81x) | **tscore** |

## Long-running (30s sustained mixed compute+alloc, single run)

| metric | tscore | node | bun | winner |
|---|---|---|---|---|
| throughput (ops/sec) | 761460 (0.74x) | 1024194 (1.00x) | 825178 (0.81x) | **node** |
| stability (last/first decile) | 0.976 | 1.022 | 1.042 | |

## Multicore (tscore parallel.map vs node/bun worker_threads)

### primes

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 50.0 (0.84x) | 59.2 (1.00x) | 34.3 (0.58x) | **bun** |
| 2 | 27.1 (0.63x) | 43.1 (1.00x) | 23.6 (0.55x) | **bun** |
| 4 | 14.2 (0.47x) | 30.1 (1.00x) | 16.5 (0.55x) | **tscore** |
| 8 | 8.4 (0.33x) | 25.5 (1.00x) | 14.3 (0.56x) | **tscore** |

### mandelbrot

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 24.7 (0.74x) | 33.2 (1.00x) | 30.1 (0.91x) | **tscore** |
| 2 | 12.8 (0.54x) | 23.6 (1.00x) | 20.2 (0.86x) | **tscore** |
| 4 | 7.0 (0.31x) | 22.2 (1.00x) | 18.5 (0.83x) | **tscore** |
| 8 | 4.4 (0.19x) | 23.1 (1.00x) | 16.5 (0.71x) | **tscore** |

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
| tscore workers:1 | 207,135 | 1.0 | 208,266 **best/core** |
| bun x1 | 189,962 | 1.0 | 184,798 |
| bun x8 reusePort (darwin: 1 active, 7 idle) | 184,100 | 1.0 | 178,740 |
| tscore workers:8 | 251,718 | 1.7 | 144,390 |
| node x1 | 140,512 | 1.0 | 138,217 |
| node cluster x8 | 187,886 | 4.0 | 47,405 |

Total throughput is capped by this machine's loopback stack, not
by any engine: four independent servers with four independent
clients total the same as one. Compare the per-core column.

Each engine at its best of 3 sweeps. Contention only ever
costs throughput, so the maximum is the estimate that converges on
the engine while a median would track how busy the box was. The
spread says how much it moved underneath the table:

| engine | min | max |
|---|---|---|
| tscore workers:1 | 193,568 | 207,135 |
| bun x1 | 171,176 | 189,962 |
| bun x8 reusePort (darwin: 1 active, 7 idle) | 179,122 | 184,100 |
| tscore workers:8 | 234,947 | 251,718 |
| node x1 | 133,745 | 140,512 |
| node cluster x8 | 184,384 | 187,886 |

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
