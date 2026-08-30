# Engine comparison: tscore vs node (V8) vs bun (JSC)

- date: 2026-08-30 20:36
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
| startup (hello.ts) | 1.8 (0.05x) | 37.8 (1.00x) | 4.5 (0.12x) | **tscore** |
| parse+compile (~50k LOC) | 8.1 (0.08x) | 99.9 (1.00x) | 12.6 (0.13x) | **tscore** |

## Steady-state (shared sources, in-program TIME_MS)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| objects | 185.8 (7.84x) | 23.7 (1.00x) | 23.3 (0.98x) | **bun** |
| closures | 134.4 (3.85x) | 34.9 (1.00x) | 50.7 (1.45x) | **node** |
| alloc | 229.8 (5.28x) | 43.5 (1.00x) | 47.8 (1.10x) | **node** |
| gc_churn | 638.9 (12.17x) | 52.5 (1.00x) | 42.3 (0.80x) | **bun** |
| promises | 61.3 (0.97x) | 63.5 (1.00x) | 75.2 (1.18x) | **tscore** |

## Async (per-engine variants, same algorithm)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| timer storm (2000x sleep 1ms) | 12.6 (0.96x) | 13.1 (1.00x) | 12.0 (0.91x) | **bun** |
| channel 100k msgs (vs worker postMessage) | 5.6 (0.16x) | 34.6 (1.00x) | 30.0 (0.87x) | **tscore** |

## Long-running (30s sustained mixed compute+alloc, single run)

| metric | tscore | node | bun | winner |
|---|---|---|---|---|
| throughput (ops/sec) | 354236 (0.36x) | 975789 (1.00x) | 747858 (0.77x) | **node** |
| stability (last/first decile) | 0.987 | 0.99 | 0.998 | |

## Multicore (tscore parallel.map vs node/bun worker_threads)

### primes

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 84.1 (1.31x) | 64.1 (1.00x) | 36.5 (0.57x) | **bun** |
| 2 | 40.2 (0.88x) | 45.8 (1.00x) | 24.9 (0.54x) | **bun** |
| 4 | 23.7 (0.77x) | 30.6 (1.00x) | 16.2 (0.53x) | **bun** |
| 8 | 11.7 (0.46x) | 25.2 (1.00x) | 13.6 (0.54x) | **tscore** |

### mandelbrot

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 26.0 (0.77x) | 33.9 (1.00x) | 31.1 (0.92x) | **tscore** |
| 2 | 13.1 (0.54x) | 24.3 (1.00x) | 20.5 (0.84x) | **tscore** |
| 4 | 6.8 (0.30x) | 22.2 (1.00x) | 18.3 (0.83x) | **tscore** |
| 8 | 4.7 (0.21x) | 22.4 (1.00x) | 16.6 (0.74x) | **tscore** |

## Scoreboard

| engine | wins |
|---|---|
| tscore | 9 |
| node | 3 |
| bun | 6 |

Win = fastest median (highest throughput for long-running) on that row.

## Not benchmarkable yet

| category | status |
|---|---|
| modules | N/A — tscore is single-file entry only (import/export rejected) |
| networking | N/A — no sockets in runtime (planned post-M4) |
| async file I/O | N/A — no TS-visible fs API |
| async I/O | proxied by timer-storm + channel benchmarks above |
