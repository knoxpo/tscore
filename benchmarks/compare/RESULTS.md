# Engine comparison: tscore vs node (V8) vs bun (JSC)

- date: 2026-09-11 02:40
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
| startup (hello.ts) | 1.8 (0.05x) | 39.8 (1.00x) | 4.9 (0.12x) | **tscore** |
| parse+compile (~50k LOC) | 9.3 (0.09x) | 103.3 (1.00x) | 12.6 (0.12x) | **tscore** |

## Steady-state (shared sources, in-program TIME_MS)

| benchmark | tscore | node | bun | winner | peak RSS MB (tscore / node / bun) |
|---|---|---|---|---|---|
| objects | 44.1 (1.82x) | 24.3 (1.00x) | 23.0 (0.95x) | **bun** | 5 / 75 / 22 |
| closures | 60.9 (1.78x) | 34.3 (1.00x) | 50.4 (1.47x) | **node** | 22 / 83 / 45 |
| alloc | 39.7 (0.91x) | 43.6 (1.00x) | 46.1 (1.06x) | **tscore** | 21 / 79 / 32 |
| gc_churn | 75.3 (1.06x) | 70.8 (1.00x) | 45.9 (0.65x) | **bun** | 370 / 498 / 355 |
| promises | 57.1 (0.90x) | 63.4 (1.00x) | 58.2 (0.92x) | **tscore** | 30 / 83 / 34 |

## Async (per-engine variants, same algorithm)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| timer storm (2000x sleep 1ms) | 10.3 (0.76x) | 13.5 (1.00x) | 12.2 (0.90x) | **tscore** |
| channel 100k msgs (vs worker postMessage) | 5.0 (0.14x) | 36.5 (1.00x) | 29.8 (0.82x) | **tscore** |

## Long-running (30s sustained mixed compute+alloc, single run)

| metric | tscore | node | bun | winner |
|---|---|---|---|---|
| throughput (ops/sec) | 779362 (0.80x) | 975163 (1.00x) | 780250 (0.80x) | **node** |
| stability (last/first decile) | 1.067 | 0.923 | 1.098 | |

## Multicore (tscore parallel.map vs node/bun worker_threads)

### primes

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 52.6 (0.84x) | 62.4 (1.00x) | 35.3 (0.57x) | **bun** |
| 2 | 27.3 (0.61x) | 44.6 (1.00x) | 25.6 (0.57x) | **bun** |
| 4 | 14.8 (0.46x) | 32.3 (1.00x) | 16.9 (0.52x) | **tscore** |
| 8 | 8.8 (0.33x) | 26.5 (1.00x) | 16.3 (0.61x) | **tscore** |

### mandelbrot

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 25.6 (0.73x) | 34.8 (1.00x) | 31.0 (0.89x) | **tscore** |
| 2 | 13.1 (0.54x) | 24.2 (1.00x) | 20.2 (0.83x) | **tscore** |
| 4 | 7.5 (0.33x) | 23.1 (1.00x) | 18.7 (0.81x) | **tscore** |
| 8 | 5.1 (0.21x) | 24.0 (1.00x) | 17.8 (0.74x) | **tscore** |

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
| tscore workers:1 | 200,590 | 1.0 | 203,237 **best/core** |
| bun x1 | 178,746 | 1.0 | 175,256 |
| bun x8 reusePort (darwin: 1 active, 7 idle) | 176,284 | 1.0 | 172,160 |
| node x1 | 138,534 | 1.0 | 136,655 |
| tscore workers:8 | 223,183 | 2.0 | 114,122 |
| node cluster x8 | 178,656 | 4.1 | 43,866 |

Total throughput is capped by this machine's loopback stack, not
by any engine: four independent servers with four independent
clients total the same as one. Compare the per-core column.

Each engine at its best of 3 sweeps. Contention only ever
costs throughput, so the maximum is the estimate that converges on
the engine while a median would track how busy the box was. The
spread says how much it moved underneath the table:

| engine | min | max |
|---|---|---|
| tscore workers:1 | 179,130 | 200,590 |
| bun x1 | 149,966 | 178,746 |
| bun x8 reusePort (darwin: 1 active, 7 idle) | 153,552 | 176,284 |
| node x1 | 117,575 | 138,534 |
| tscore workers:8 | 171,346 | 223,183 |
| node cluster x8 | 150,708 | 178,656 |

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
