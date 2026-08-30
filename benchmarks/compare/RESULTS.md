# Engine comparison: tscore vs node (V8) vs bun (JSC)

- date: 2026-08-30 19:16
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
| startup (hello.ts) | 1.8 (0.05x) | 37.5 (1.00x) | 4.4 (0.12x) | **tscore** |
| parse+compile (~50k LOC) | 22.9 (0.23x) | 99.0 (1.00x) | 12.4 (0.13x) | **bun** |

## Steady-state (shared sources, in-program TIME_MS)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| objects | 184.2 (7.73x) | 23.8 (1.00x) | 23.2 (0.97x) | **bun** |
| closures | 131.6 (3.99x) | 33.0 (1.00x) | 47.2 (1.43x) | **node** |
| alloc | 238.4 (5.44x) | 43.8 (1.00x) | 45.8 (1.05x) | **node** |
| gc_churn | 642.8 (12.33x) | 52.2 (1.00x) | 42.9 (0.82x) | **bun** |
| promises | 61.4 (0.99x) | 61.9 (1.00x) | 55.6 (0.90x) | **bun** |

## Async (per-engine variants, same algorithm)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| timer storm (2000x sleep 1ms) | 12.5 (0.93x) | 13.4 (1.00x) | 12.2 (0.91x) | **bun** |
| channel 100k msgs (vs worker postMessage) | 5.7 (0.17x) | 33.1 (1.00x) | 28.9 (0.87x) | **tscore** |

## Long-running (30s sustained mixed compute+alloc, single run)

| metric | tscore | node | bun | winner |
|---|---|---|---|---|
| throughput (ops/sec) | 344285 (0.40x) | 859739 (1.00x) | 805425 (0.94x) | **node** |
| stability (last/first decile) | 0.98 | 0.99 | 0.998 | |

## Multicore (tscore parallel.map vs node/bun worker_threads)

### primes

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 75.9 (1.23x) | 61.6 (1.00x) | 35.5 (0.58x) | **bun** |
| 2 | 40.0 (0.89x) | 44.7 (1.00x) | 23.7 (0.53x) | **bun** |
| 4 | 23.7 (0.78x) | 30.4 (1.00x) | 16.6 (0.55x) | **bun** |
| 8 | 12.0 (0.46x) | 25.9 (1.00x) | 15.4 (0.59x) | **tscore** |

### mandelbrot

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 24.9 (0.71x) | 35.2 (1.00x) | 31.0 (0.88x) | **tscore** |
| 2 | 12.8 (0.55x) | 23.5 (1.00x) | 20.3 (0.86x) | **tscore** |
| 4 | 7.2 (0.31x) | 22.9 (1.00x) | 18.5 (0.81x) | **tscore** |
| 8 | 4.7 (0.21x) | 22.4 (1.00x) | 17.1 (0.76x) | **tscore** |

## Scoreboard

| engine | wins |
|---|---|
| tscore | 7 |
| node | 3 |
| bun | 8 |

Win = fastest median (highest throughput for long-running) on that row.

## Not benchmarkable yet

| category | status |
|---|---|
| modules | N/A — tscore is single-file entry only (import/export rejected) |
| networking | N/A — no sockets in runtime (planned post-M4) |
| async file I/O | N/A — no TS-visible fs API |
| async I/O | proxied by timer-storm + channel benchmarks above |
