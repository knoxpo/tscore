# Engine comparison: tscore vs node (V8) vs bun (JSC)

- date: 2026-09-07 23:05
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
| startup (hello.ts) | 1.7 (0.05x) | 36.8 (1.00x) | 4.2 (0.11x) | **tscore** |
| parse+compile (~50k LOC) | 9.3 (0.10x) | 95.8 (1.00x) | 11.0 (0.11x) | **tscore** |

## Steady-state (shared sources, in-program TIME_MS)

| benchmark | tscore | node | bun | winner | peak RSS MB (tscore / node / bun) |
|---|---|---|---|---|---|
| objects | 55.9 (2.44x) | 22.9 (1.00x) | 22.0 (0.96x) | **bun** | 5 / 75 / 22 |
| closures | 60.9 (1.86x) | 32.8 (1.00x) | 46.9 (1.43x) | **node** | 22 / 83 / 45 |
| alloc | 42.0 (0.89x) | 47.0 (1.00x) | 45.3 (0.96x) | **tscore** | 21 / 79 / 32 |
| gc_churn | 86.7 (1.42x) | 61.0 (1.00x) | 43.1 (0.71x) | **bun** | 370 / 500 / 355 |
| promises | 54.4 (0.90x) | 60.2 (1.00x) | 56.1 (0.93x) | **tscore** | 30 / 83 / 34 |

## Async (per-engine variants, same algorithm)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| timer storm (2000x sleep 1ms) | 10.3 (0.85x) | 12.1 (1.00x) | 12.1 (1.00x) | **tscore** |
| channel 100k msgs (vs worker postMessage) | 5.3 (0.15x) | 34.6 (1.00x) | 28.3 (0.82x) | **tscore** |

## Long-running (30s sustained mixed compute+alloc, single run)

| metric | tscore | node | bun | winner |
|---|---|---|---|---|
| throughput (ops/sec) | 843951 (0.82x) | 1032197 (1.00x) | 836278 (0.81x) | **node** |
| stability (last/first decile) | 1.001 | 1.022 | 0.989 | |

## Multicore (tscore parallel.map vs node/bun worker_threads)

### primes

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 48.8 (0.84x) | 58.2 (1.00x) | 33.1 (0.57x) | **bun** |
| 2 | 27.1 (0.64x) | 42.3 (1.00x) | 23.9 (0.56x) | **bun** |
| 4 | 14.1 (0.47x) | 30.2 (1.00x) | 16.8 (0.55x) | **tscore** |
| 8 | 8.6 (0.33x) | 25.9 (1.00x) | 14.4 (0.56x) | **tscore** |

### mandelbrot

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 24.5 (0.74x) | 33.2 (1.00x) | 29.9 (0.90x) | **tscore** |
| 2 | 12.9 (0.55x) | 23.6 (1.00x) | 19.9 (0.85x) | **tscore** |
| 4 | 7.1 (0.31x) | 22.5 (1.00x) | 18.3 (0.82x) | **tscore** |
| 8 | 4.5 (0.20x) | 22.7 (1.00x) | 16.6 (0.73x) | **tscore** |

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
| tscore workers:1 | 222,542 | 1.0 | 223,738 **best/core** |
| bun x8 reusePort (darwin: 1 active, 7 idle) | 196,269 | 1.0 | 189,823 |
| bun x1 | 195,908 | 1.0 | 189,823 |
| tscore workers:8 | 247,470 | 1.5 | 164,855 |
| node x1 | 147,787 | 1.0 | 145,437 |
| node cluster x8 | 201,600 | 3.7 | 54,070 |

Total throughput is capped by this machine's loopback stack, not
by any engine: four independent servers with four independent
clients total the same as one. Compare the per-core column.

Each engine at its best of 3 sweeps. Contention only ever
costs throughput, so the maximum is the estimate that converges on
the engine while a median would track how busy the box was. The
spread says how much it moved underneath the table:

| engine | min | max |
|---|---|---|
| tscore workers:1 | 218,721 | 222,542 |
| bun x8 reusePort (darwin: 1 active, 7 idle) | 194,858 | 196,269 |
| bun x1 | 190,594 | 195,908 |
| tscore workers:8 | 232,577 | 247,470 |
| node x1 | 142,704 | 147,787 |
| node cluster x8 | 190,511 | 201,600 |

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
