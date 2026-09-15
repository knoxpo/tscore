# Engine comparison: tscore vs node (V8) vs bun (JSC)

- date: 2026-09-16 04:12
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
| startup (hello.ts) | 1.7 (0.05x) | 37.6 (1.00x) | 4.5 (0.12x) | **tscore** |
| parse+compile (~50k LOC) | 8.8 (0.09x) | 99.5 (1.00x) | 12.3 (0.12x) | **tscore** |

## Steady-state (shared sources, in-program TIME_MS)

| benchmark | tscore | node | bun | winner | peak RSS MB (tscore / node / bun) |
|---|---|---|---|---|---|
| objects | 32.7 (1.37x) | 23.9 (1.00x) | 23.1 (0.97x) | **bun** | 5 / 75 / 22 |
| closures | 53.3 (1.56x) | 34.3 (1.00x) | 47.8 (1.39x) | **node** | 22 / 83 / 45 |
| alloc | 37.9 (0.88x) | 43.2 (1.00x) | 49.3 (1.14x) | **tscore** | 21 / 79 / 32 |
| gc_churn | 72.5 (1.46x) | 49.5 (1.00x) | 42.1 (0.85x) | **bun** | 370 / 490 / 355 |
| promises | 57.2 (0.88x) | 64.9 (1.00x) | 57.7 (0.89x) | **tscore** | 30 / 83 / 34 |

## Async (per-engine variants, same algorithm)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| timer storm (2000x sleep 1ms) | 10.3 (0.78x) | 13.1 (1.00x) | 12.0 (0.91x) | **tscore** |
| channel 100k msgs (vs worker postMessage) | 5.0 (0.14x) | 34.8 (1.00x) | 29.3 (0.84x) | **tscore** |

## Long-running (30s sustained mixed compute+alloc, single run)

| metric | tscore | node | bun | winner |
|---|---|---|---|---|
| throughput (ops/sec) | 916796 (0.90x) | 1024220 (1.00x) | 822642 (0.80x) | **node** |
| stability (last/first decile) | 1.021 | 1.014 | 0.999 | |

## Multicore (tscore parallel.map vs node/bun worker_threads)

### primes

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 47.9 (0.82x) | 58.1 (1.00x) | 34.5 (0.59x) | **bun** |
| 2 | 26.5 (0.62x) | 42.8 (1.00x) | 24.2 (0.57x) | **bun** |
| 4 | 14.2 (0.47x) | 30.5 (1.00x) | 16.3 (0.54x) | **tscore** |
| 8 | 8.1 (0.32x) | 25.2 (1.00x) | 14.1 (0.56x) | **tscore** |

### mandelbrot

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 24.8 (0.74x) | 33.4 (1.00x) | 29.8 (0.89x) | **tscore** |
| 2 | 12.9 (0.53x) | 24.2 (1.00x) | 20.3 (0.84x) | **tscore** |
| 4 | 7.5 (0.34x) | 22.2 (1.00x) | 18.5 (0.83x) | **tscore** |
| 8 | 4.4 (0.20x) | 22.2 (1.00x) | 16.4 (0.74x) | **tscore** |

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
| tscore workers:1 | 219,144 | 1.0 | 220,244 **best/core** |
| bun x1 | 191,872 | 1.0 | 186,226 |
| bun x8 reusePort (darwin: 1 active, 7 idle) | 191,736 | 1.0 | 184,999 |
| node x1 | 145,298 | 1.0 | 142,690 |
| tscore workers:8 | 242,765 | 1.7 | 139,910 |
| node cluster x8 | 192,343 | 3.8 | 50,637 |

Total throughput is capped by this machine's loopback stack, not
by any engine: four independent servers with four independent
clients total the same as one. Compare the per-core column.

Each engine at its best of 3 sweeps. Contention only ever
costs throughput, so the maximum is the estimate that converges on
the engine while a median would track how busy the box was. The
spread says how much it moved underneath the table:

| engine | min | max |
|---|---|---|
| tscore workers:1 | 213,545 | 219,144 |
| bun x1 | 191,855 | 191,872 |
| bun x8 reusePort (darwin: 1 active, 7 idle) | 191,238 | 191,736 |
| node x1 | 142,730 | 145,298 |
| tscore workers:8 | 228,573 | 242,765 |
| node cluster x8 | 191,517 | 192,343 |

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
