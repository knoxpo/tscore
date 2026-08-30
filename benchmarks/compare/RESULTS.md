# Engine comparison: tscore vs node (V8) vs bun (JSC)

- date: 2026-08-30 15:54
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
| startup (hello.ts) | 1.4 (0.04x) | 36.9 (1.00x) | 4.1 (0.11x) | **tscore** |
| parse+compile (~50k LOC) | 19.9 (0.20x) | 99.6 (1.00x) | 11.4 (0.11x) | **bun** |

## Steady-state (shared sources, in-program TIME_MS)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| objects | 236.9 (10.01x) | 23.7 (1.00x) | 23.4 (0.99x) | **bun** |
| closures | 222.7 (6.65x) | 33.5 (1.00x) | 47.5 (1.42x) | **node** |
| alloc | 450.2 (10.46x) | 43.0 (1.00x) | 47.0 (1.09x) | **node** |
| gc_churn | 831.0 (15.37x) | 54.1 (1.00x) | 41.5 (0.77x) | **bun** |
| promises | 79.5 (1.26x) | 63.1 (1.00x) | 57.1 (0.90x) | **bun** |

## Async (per-engine variants, same algorithm)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| timer storm (2000x sleep 1ms) | 13.2 (1.06x) | 12.4 (1.00x) | 12.0 (0.97x) | **bun** |
| channel 100k msgs (vs worker postMessage) | 5.9 (0.17x) | 34.2 (1.00x) | 29.3 (0.86x) | **tscore** |

## Long-running (30s sustained mixed compute+alloc, single run)

| metric | tscore | node | bun | winner |
|---|---|---|---|---|
| throughput (ops/sec) | 293913 (0.28x) | 1040033 (1.00x) | 767598 (0.74x) | **node** |
| stability (last/first decile) | 1.02 | 0.997 | 0.918 | |

## Multicore (tscore parallel.map vs node/bun worker_threads)

### primes

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 85.8 (1.43x) | 60.1 (1.00x) | 33.8 (0.56x) | **bun** |
| 2 | 46.0 (1.06x) | 43.3 (1.00x) | 24.0 (0.55x) | **bun** |
| 4 | 25.5 (0.83x) | 30.7 (1.00x) | 16.1 (0.52x) | **bun** |
| 8 | 13.4 (0.53x) | 25.3 (1.00x) | 14.2 (0.56x) | **tscore** |

### mandelbrot

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 118.0 (3.59x) | 32.9 (1.00x) | 29.6 (0.90x) | **bun** |
