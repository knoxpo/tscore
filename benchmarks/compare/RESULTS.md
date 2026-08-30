# Engine comparison: tscore vs node (V8) vs bun (JSC)

- date: 2026-08-31 01:00
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
| startup (hello.ts) | 1.7 (0.04x) | 38.2 (1.00x) | 5.3 (0.14x) | **tscore** |
| parse+compile (~50k LOC) | 9.6 (0.09x) | 102.6 (1.00x) | 12.9 (0.13x) | **tscore** |

## Steady-state (shared sources, in-program TIME_MS)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| objects | 186.1 (6.58x) | 28.3 (1.00x) | 23.5 (0.83x) | **bun** |
| closures | 138.9 (4.02x) | 34.5 (1.00x) | 54.8 (1.59x) | **node** |
| alloc | 236.9 (5.32x) | 44.6 (1.00x) | 47.2 (1.06x) | **node** |
| gc_churn | 636.0 (10.92x) | 58.2 (1.00x) | 45.2 (0.78x) | **bun** |
| promises | 145.9 (2.14x) | 68.1 (1.00x) | 77.7 (1.14x) | **node** |

## Async (per-engine variants, same algorithm)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| timer storm (2000x sleep 1ms) | 12.3 (1.00x) | 12.3 (1.00x) | 12.0 (0.97x) | **bun** |
| channel 100k msgs (vs worker postMessage) | 5.2 (0.13x) | 40.2 (1.00x) | 38.9 (0.97x) | **tscore** |

## Long-running (30s sustained mixed compute+alloc, single run)

| metric | tscore | node | bun | winner |
|---|---|---|---|---|
| throughput (ops/sec) | 344082 (0.36x) | 958839 (1.00x) | 742835 (0.77x) | **node** |
| stability (last/first decile) | 1.17 | 0.914 | 0.889 | |

## Multicore (tscore parallel.map vs node/bun worker_threads)

### primes

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 79.6 (1.18x) | 67.4 (1.00x) | 37.7 (0.56x) | **bun** |
| 2 | 42.9 (0.92x) | 46.8 (1.00x) | 26.7 (0.57x) | **bun** |
| 4 | 25.1 (0.74x) | 33.9 (1.00x) | 20.8 (0.61x) | **bun** |
| 8 | 13.9 (0.49x) | 28.2 (1.00x) | 17.3 (0.61x) | **tscore** |

### mandelbrot

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 26.1 (0.70x) | 37.4 (1.00x) | 32.5 (0.87x) | **tscore** |
| 2 | 13.2 (0.49x) | 26.7 (1.00x) | 21.5 (0.80x) | **tscore** |
| 4 | 7.1 (0.27x) | 26.0 (1.00x) | 20.3 (0.78x) | **tscore** |
| 8 | 4.0 (0.16x) | 24.6 (1.00x) | 17.8 (0.72x) | **tscore** |

## Scoreboard

| engine | wins |
|---|---|
| tscore | 8 |
| node | 4 |
| bun | 6 |

Win = fastest median (highest throughput for long-running) on that row.

## Not benchmarkable yet

| category | status |
|---|---|
| modules | N/A — tscore is single-file entry only (import/export rejected) |
| networking | N/A — no sockets in runtime (planned post-M4) |
| async file I/O | N/A — no TS-visible fs API |
| async I/O | proxied by timer-storm + channel benchmarks above |
