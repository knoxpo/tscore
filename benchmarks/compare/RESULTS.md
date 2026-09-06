# Engine comparison: tscore vs node (V8) vs bun (JSC)

- date: 2026-09-06 23:32
- machine: Apple M5 Max, 18 logical cpus (6P + 12E)
- tscore: tscore 0.1.0, node: v24.17.0, bun: 1.4.0
- steady-state cells: median TIME_MS of 5 runs, first run discarded; in-program warmup pass before timing
- ratios: engine_ms / node_ms — lower is better, <1.00x is faster than node
- methodology: shared benchmarks are byte-identical sources in the tscore language subset;
  node/bun run non-idiomatic code (no Array.map, classes, etc.) — this compares engines on
  identical programs, not idiomatic-per-engine programs

## Startup + parse/compile (end-to-end, hyperfine)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| startup (hello.ts) | 1.8 (0.05x) | 37.9 (1.00x) | 4.6 (0.12x) | **tscore** |
| parse+compile (~50k LOC) | 9.0 (0.09x) | 99.6 (1.00x) | 11.3 (0.11x) | **tscore** |

## Steady-state (shared sources, in-program TIME_MS)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| objects | 63.8 (2.69x) | 23.7 (1.00x) | 23.2 (0.98x) | **bun** |
| closures | 103.1 (2.87x) | 35.9 (1.00x) | 47.8 (1.33x) | **node** |
| alloc | 70.8 (1.66x) | 42.6 (1.00x) | 45.1 (1.06x) | **node** |
| gc_churn | 118.5 (2.09x) | 56.7 (1.00x) | 42.2 (0.74x) | **bun** |
| promises | 56.1 (0.77x) | 72.4 (1.00x) | 60.6 (0.84x) | **tscore** |

## Async (per-engine variants, same algorithm)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| timer storm (2000x sleep 1ms) | 11.6 (0.87x) | 13.3 (1.00x) | 12.0 (0.91x) | **tscore** |
| channel 100k msgs (vs worker postMessage) | 5.1 (0.14x) | 35.0 (1.00x) | 29.2 (0.84x) | **tscore** |

## Long-running (30s sustained mixed compute+alloc, single run)

| metric | tscore | node | bun | winner |
|---|---|---|---|---|
| throughput (ops/sec) | 763974 (0.81x) | 946754 (1.00x) | 741177 (0.78x) | **node** |
| stability (last/first decile) | 0.909 | 0.905 | 1.248 | |

## Multicore (tscore parallel.map vs node/bun worker_threads)

### primes

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 51.7 (0.85x) | 60.9 (1.00x) | 34.8 (0.57x) | **bun** |
| 2 | 27.4 (0.63x) | 43.3 (1.00x) | 25.0 (0.58x) | **bun** |
| 4 | 14.7 (0.47x) | 30.9 (1.00x) | 16.9 (0.55x) | **tscore** |
| 8 | 8.4 (0.33x) | 25.5 (1.00x) | 14.0 (0.55x) | **tscore** |

### mandelbrot

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 25.1 (0.74x) | 33.8 (1.00x) | 30.8 (0.91x) | **tscore** |
| 2 | 12.8 (0.54x) | 23.9 (1.00x) | 20.1 (0.84x) | **tscore** |
| 4 | 6.8 (0.30x) | 22.8 (1.00x) | 18.3 (0.80x) | **tscore** |
| 8 | 3.9 (0.17x) | 22.8 (1.00x) | 16.9 (0.74x) | **tscore** |

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
| tscore workers:1 | 206,429 | 1.0 | 207,539 **best/core** |
| bun x1 | 188,592 | 1.0 | 183,440 |
| bun x8 reusePort (darwin: 1 active, 7 idle) | 186,049 | 1.0 | 179,869 |
| node x1 | 141,499 | 1.0 | 139,214 |
| tscore workers:8 | 222,587 | 2.0 | 112,321 |
| node cluster x8 | 184,670 | 4.0 | 46,571 |

Total throughput is capped by this machine's loopback stack, not
by any engine: four independent servers with four independent
clients total the same as one. Compare the per-core column.

Each engine at its best of 3 sweeps. Contention only ever
costs throughput, so the maximum is the estimate that converges on
the engine while a median would track how busy the box was. The
spread says how much it moved underneath the table:

| engine | min | max |
|---|---|---|
| tscore workers:1 | 188,362 | 206,429 |
| bun x1 | 176,695 | 188,592 |
| bun x8 reusePort (darwin: 1 active, 7 idle) | 138,445 | 186,049 |
| node x1 | 119,128 | 141,499 |
| tscore workers:8 | 184,459 | 222,587 |
| node cluster x8 | 162,805 | 184,670 |

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
