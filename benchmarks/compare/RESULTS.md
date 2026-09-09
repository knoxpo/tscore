# Engine comparison: tscore vs node (V8) vs bun (JSC)

- date: 2026-09-09 12:47
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
| startup (hello.ts) | 1.8 (0.05x) | 38.6 (1.00x) | 4.5 (0.12x) | **tscore** |
| parse+compile (~50k LOC) | 9.3 (0.09x) | 102.4 (1.00x) | 11.9 (0.12x) | **tscore** |

## Steady-state (shared sources, in-program TIME_MS)

| benchmark | tscore | node | bun | winner | peak RSS MB (tscore / node / bun) |
|---|---|---|---|---|---|
| objects | 52.4 (2.16x) | 24.2 (1.00x) | 24.4 (1.00x) | **node** | 5 / 75 / 22 |
| closures | 64.1 (1.85x) | 34.6 (1.00x) | 53.4 (1.54x) | **node** | 23 / 83 / 45 |
| alloc | 42.6 (0.97x) | 43.9 (1.00x) | 59.1 (1.35x) | **tscore** | 21 / 78 / 32 |
| gc_churn | 73.6 (1.19x) | 61.7 (1.00x) | 46.4 (0.75x) | **bun** | 370 / 497 / 355 |
| promises | 56.3 (0.88x) | 64.1 (1.00x) | 62.1 (0.97x) | **tscore** | 30 / 83 / 34 |

## Async (per-engine variants, same algorithm)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| timer storm (2000x sleep 1ms) | 10.2 (0.86x) | 12.0 (1.00x) | 11.7 (0.97x) | **tscore** |
| channel 100k msgs (vs worker postMessage) | 5.7 (0.16x) | 36.4 (1.00x) | 28.7 (0.79x) | **tscore** |

## Long-running (30s sustained mixed compute+alloc, single run)

| metric | tscore | node | bun | winner |
|---|---|---|---|---|
| throughput (ops/sec) | 761856 (0.79x) | 968741 (1.00x) | 772253 (0.80x) | **node** |
| stability (last/first decile) | 0.982 | 1.021 | 0.997 | |

## Multicore (tscore parallel.map vs node/bun worker_threads)

### primes

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 53.9 (0.84x) | 64.2 (1.00x) | 35.7 (0.56x) | **bun** |
| 2 | 28.2 (0.64x) | 44.2 (1.00x) | 23.9 (0.54x) | **bun** |
| 4 | 14.5 (0.46x) | 31.7 (1.00x) | 17.1 (0.54x) | **tscore** |
| 8 | 9.2 (0.33x) | 27.6 (1.00x) | 15.1 (0.55x) | **tscore** |

### mandelbrot

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 25.0 (0.73x) | 34.1 (1.00x) | 30.9 (0.91x) | **tscore** |
| 2 | 13.5 (0.55x) | 24.7 (1.00x) | 21.8 (0.88x) | **tscore** |
| 4 | 7.8 (0.30x) | 25.7 (1.00x) | 19.2 (0.75x) | **tscore** |
| 8 | 5.4 (0.22x) | 24.2 (1.00x) | 19.0 (0.79x) | **tscore** |

## Scoreboard

| engine | wins |
|---|---|
| tscore | 12 |
| node | 3 |
| bun | 3 |

Win = fastest median (highest throughput for long-running) on that row.


## HTTP hello (wrk -t2 -c32 -d5s, best of 3)

| engine | req/s | cores | req/s per core |
|---|---|---|---|
| tscore workers:1 | 203,855 | 1.0 | 205,240 **best/core** |
| bun x8 reusePort (darwin: 1 active, 7 idle) | 184,722 | 1.0 | 178,574 |
| bun x1 | 178,523 | 1.0 | 174,258 |
| node x1 | 140,388 | 1.0 | 138,891 |
| tscore workers:8 | 174,142 | 2.4 | 73,323 |
| node cluster x8 | 149,948 | 3.7 | 40,475 |

Total throughput is capped by this machine's loopback stack, not
by any engine: four independent servers with four independent
clients total the same as one. Compare the per-core column.

Each engine at its best of 3 sweeps. Contention only ever
costs throughput, so the maximum is the estimate that converges on
the engine while a median would track how busy the box was. The
spread says how much it moved underneath the table:

| engine | min | max |
|---|---|---|
| tscore workers:1 | 191,069 | 203,855 |
| bun x8 reusePort (darwin: 1 active, 7 idle) | 165,074 | 184,722 |
| bun x1 | 171,420 | 178,523 |
| node x1 | 133,021 | 140,388 |
| tscore workers:8 | 171,678 | 174,142 |
| node cluster x8 | 144,653 | 149,948 |

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
