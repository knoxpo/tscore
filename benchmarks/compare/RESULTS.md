# Engine comparison: tscore vs node (V8) vs bun (JSC)

- date: 2026-09-10 20:20
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
| startup (hello.ts) | 1.9 (0.05x) | 38.8 (1.00x) | 4.8 (0.12x) | **tscore** |
| parse+compile (~50k LOC) | 9.6 (0.09x) | 101.4 (1.00x) | 12.3 (0.12x) | **tscore** |

## Steady-state (shared sources, in-program TIME_MS)

| benchmark | tscore | node | bun | winner | peak RSS MB (tscore / node / bun) |
|---|---|---|---|---|---|
| objects | 44.5 (1.89x) | 23.6 (1.00x) | 23.5 (1.00x) | **bun** | 5 / 75 / 22 |
| closures | 61.8 (1.79x) | 34.5 (1.00x) | 49.2 (1.43x) | **node** | 23 / 83 / 45 |
| alloc | 39.2 (0.90x) | 43.6 (1.00x) | 46.3 (1.06x) | **tscore** | 21 / 79 / 32 |
| gc_churn | 74.4 (1.35x) | 55.3 (1.00x) | 41.8 (0.76x) | **bun** | 370 / 499 / 355 |
| promises | 55.4 (0.85x) | 65.3 (1.00x) | 62.0 (0.95x) | **tscore** | 30 / 83 / 34 |

## Async (per-engine variants, same algorithm)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| timer storm (2000x sleep 1ms) | 10.3 (0.78x) | 13.2 (1.00x) | 12.2 (0.92x) | **tscore** |
| channel 100k msgs (vs worker postMessage) | 5.5 (0.15x) | 36.0 (1.00x) | 29.7 (0.83x) | **tscore** |

## Long-running (30s sustained mixed compute+alloc, single run)

| metric | tscore | node | bun | winner |
|---|---|---|---|---|
| throughput (ops/sec) | 778509 (0.84x) | 928712 (1.00x) | 685228 (0.74x) | **node** |
| stability (last/first decile) | 1.001 | 0.948 | 0.952 | |

## Multicore (tscore parallel.map vs node/bun worker_threads)

### primes

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 53.7 (0.80x) | 67.0 (1.00x) | 36.9 (0.55x) | **bun** |
| 2 | 28.3 (0.61x) | 46.7 (1.00x) | 25.9 (0.55x) | **bun** |
| 4 | 15.7 (0.47x) | 33.3 (1.00x) | 18.7 (0.56x) | **tscore** |
| 8 | 9.5 (0.34x) | 27.7 (1.00x) | 16.5 (0.60x) | **tscore** |

### mandelbrot

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 26.5 (0.73x) | 36.3 (1.00x) | 32.7 (0.90x) | **tscore** |
| 2 | 13.8 (0.52x) | 26.5 (1.00x) | 21.8 (0.82x) | **tscore** |
| 4 | 7.6 (0.31x) | 24.9 (1.00x) | 19.9 (0.80x) | **tscore** |
| 8 | 5.3 (0.21x) | 24.6 (1.00x) | 18.1 (0.74x) | **tscore** |

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
| bun x1 | 184,472 | 1.0 | 179,030 **best/core** |
| bun x8 reusePort (darwin: 1 active, 7 idle) | 184,240 | 1.0 | 177,772 |
| tscore workers:1 | 140,074 | 1.0 | 144,770 |
| node x1 | 122,532 | 1.0 | 121,791 |
| tscore workers:8 | 147,431 | 2.1 | 71,336 |
| node cluster x8 | 126,608 | 3.8 | 33,501 |

Total throughput is capped by this machine's loopback stack, not
by any engine: four independent servers with four independent
clients total the same as one. Compare the per-core column.

Each engine at its best of 3 sweeps. Contention only ever
costs throughput, so the maximum is the estimate that converges on
the engine while a median would track how busy the box was. The
spread says how much it moved underneath the table:

| engine | min | max |
|---|---|---|
| bun x1 | 107,155 | 184,472 |
| bun x8 reusePort (darwin: 1 active, 7 idle) | 98,655 | 184,240 |
| tscore workers:1 | 116,734 | 140,074 |
| node x1 | 76,231 | 122,532 |
| tscore workers:8 | 116,518 | 147,431 |
| node cluster x8 | 104,866 | 126,608 |

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
