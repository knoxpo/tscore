# Engine comparison: tscore vs node (V8) vs bun (JSC)

- date: 2026-08-30 20:26
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
| startup (hello.ts) | 1.6 (0.04x) | 37.8 (1.00x) | 4.3 (0.11x) | **tscore** |
| parse+compile (~50k LOC) | 7.1 (0.07x) | 100.9 (1.00x) | 11.7 (0.12x) | **tscore** |

## Steady-state (shared sources, in-program TIME_MS)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| objects | 186.2 (7.80x) | 23.9 (1.00x) | 23.2 (0.97x) | **bun** |
| closures | 134.4 (3.90x) | 34.4 (1.00x) | 48.0 (1.39x) | **node** |
| alloc | 281.5 (6.45x) | 43.6 (1.00x) | 45.9 (1.05x) | **node** |
| gc_churn | 639.2 (10.87x) | 58.8 (1.00x) | 43.3 (0.74x) | **bun** |
| promises | 61.7 (0.96x) | 64.3 (1.00x) | 57.0 (0.89x) | **bun** |

## Async (per-engine variants, same algorithm)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| timer storm (2000x sleep 1ms) | 12.7 (0.96x) | 13.3 (1.00x) | 12.2 (0.92x) | **bun** |
| channel 100k msgs (vs worker postMessage) | 5.7 (0.16x) | 35.2 (1.00x) | 29.2 (0.83x) | **tscore** |

## Long-running (30s sustained mixed compute+alloc, single run)

| metric | tscore | node | bun | winner |
|---|---|---|---|---|
| throughput (ops/sec) | 347258 (0.34x) | 1016016 (1.00x) | 822695 (0.81x) | **node** |
| stability (last/first decile) | 0.995 | 1.036 | 0.972 | |

## Multicore (tscore parallel.map vs node/bun worker_threads)

### primes

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 77.0 (1.25x) | 61.6 (1.00x) | 36.6 (0.59x) | **bun** |
| 2 | 41.7 (0.93x) | 44.7 (1.00x) | 23.9 (0.53x) | **bun** |
| 4 | 24.9 (0.73x) | 34.1 (1.00x) | 19.7 (0.58x) | **bun** |
| 8 | 13.2 (0.47x) | 28.1 (1.00x) | 14.9 (0.53x) | **tscore** |

### mandelbrot

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 25.9 (0.71x) | 36.6 (1.00x) | 31.6 (0.86x) | **tscore** |
| 2 | 12.8 (0.51x) | 25.2 (1.00x) | 21.6 (0.86x) | **tscore** |
| 4 | 7.4 (0.30x) | 24.7 (1.00x) | 19.1 (0.77x) | **tscore** |
| 8 | 4.8 (0.22x) | 22.1 (1.00x) | 17.9 (0.81x) | **tscore** |

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
