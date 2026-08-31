# Engine comparison: tscore vs node (V8) vs bun (JSC)

- date: 2026-08-31 17:26
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
| startup (hello.ts) | 1.6 (0.04x) | 38.2 (1.00x) | 4.9 (0.13x) | **tscore** |
| parse+compile (~50k LOC) | 8.0 (0.08x) | 102.9 (1.00x) | 12.4 (0.12x) | **tscore** |

## Steady-state (shared sources, in-program TIME_MS)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| objects | 186.7 (7.70x) | 24.3 (1.00x) | 23.5 (0.97x) | **bun** |
| closures | 113.9 (2.72x) | 41.9 (1.00x) | 71.6 (1.71x) | **node** |
| alloc | 188.6 (4.28x) | 44.1 (1.00x) | 48.4 (1.10x) | **node** |
| gc_churn | 483.5 (8.96x) | 54.0 (1.00x) | 44.1 (0.82x) | **bun** |
| promises | 64.5 (1.01x) | 63.9 (1.00x) | 58.4 (0.91x) | **bun** |

## Async (per-engine variants, same algorithm)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| timer storm (2000x sleep 1ms) | 12.6 (0.95x) | 13.3 (1.00x) | 12.0 (0.90x) | **bun** |
| channel 100k msgs (vs worker postMessage) | 5.1 (0.15x) | 34.9 (1.00x) | 29.2 (0.84x) | **tscore** |

## Long-running (30s sustained mixed compute+alloc, single run)

| metric | tscore | node | bun | winner |
|---|---|---|---|---|
| throughput (ops/sec) | 406252 (0.41x) | 1002619 (1.00x) | 776542 (0.77x) | **node** |
| stability (last/first decile) | 0.997 | 1.02 | 1.036 | |

## HTTP hello (wrk -t4 -c64 -d5s, 8 workers each; measured 2026-08-31)

| engine | req/s | vs node |
|---|---|---|
| tscore (http.serve, workers:8) | 125,708 | 0.74x |
| node (cluster x8) | 170,374 | 1.00x |
| bun (Bun.serve) | 190,350 | 1.12x |

## Multicore (tscore parallel.map vs node/bun worker_threads)

### primes

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 76.3 (1.25x) | 61.2 (1.00x) | 35.1 (0.57x) | **bun** |
| 2 | 41.4 (0.93x) | 44.4 (1.00x) | 24.0 (0.54x) | **bun** |
| 4 | 20.4 (0.65x) | 31.2 (1.00x) | 16.7 (0.54x) | **bun** |
| 8 | 11.8 (0.46x) | 25.7 (1.00x) | 15.0 (0.58x) | **tscore** |

### mandelbrot

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 25.0 (0.70x) | 35.5 (1.00x) | 30.9 (0.87x) | **tscore** |
| 2 | 12.8 (0.52x) | 24.5 (1.00x) | 20.2 (0.83x) | **tscore** |
| 4 | 6.9 (0.30x) | 22.7 (1.00x) | 18.5 (0.82x) | **tscore** |
| 8 | 4.0 (0.18x) | 22.8 (1.00x) | 16.6 (0.73x) | **tscore** |

## Scoreboard

| engine | wins |
|---|---|
| tscore | 8 |
| node | 3 |
| bun | 7 |

Win = fastest median (highest throughput for long-running) on that row.

## Not benchmarkable yet

| category | status |
|---|---|
| modules | N/A — tscore is single-file entry only (import/export rejected) |
| networking | N/A — no sockets in runtime (planned post-M4) |
| async file I/O | N/A — no TS-visible fs API |
| async I/O | proxied by timer-storm + channel benchmarks above |
