# Engine comparison: tscore vs node (V8) vs bun (JSC)

- date: 2026-08-30 20:47
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
| startup (hello.ts) | 1.9 (0.05x) | 37.8 (1.00x) | 4.5 (0.12x) | **tscore** |
| parse+compile (~50k LOC) | 8.2 (0.08x) | 100.3 (1.00x) | 12.0 (0.12x) | **tscore** |

## Steady-state (shared sources, in-program TIME_MS)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| objects | 185.3 (7.74x) | 23.9 (1.00x) | 22.7 (0.95x) | **bun** |
| closures | 135.2 (3.72x) | 36.3 (1.00x) | 48.9 (1.35x) | **node** |
| alloc | 227.6 (5.26x) | 43.3 (1.00x) | 50.2 (1.16x) | **node** |
| gc_churn | 629.6 (11.87x) | 53.1 (1.00x) | 42.7 (0.80x) | **bun** |
| promises | 61.3 (0.96x) | 63.6 (1.00x) | 58.1 (0.91x) | **bun** |

## Async (per-engine variants, same algorithm)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| timer storm (2000x sleep 1ms) | 12.7 (0.96x) | 13.3 (1.00x) | 12.1 (0.91x) | **bun** |
| channel 100k msgs (vs worker postMessage) | 5.6 (0.16x) | 34.3 (1.00x) | 29.2 (0.85x) | **tscore** |

## Long-running (30s sustained mixed compute+alloc, single run)

| metric | tscore | node | bun | winner |
|---|---|---|---|---|
| throughput (ops/sec) | 360133 (0.35x) | 1023913 (1.00x) | 824729 (0.81x) | **node** |
| stability (last/first decile) | 1.025 | 1.02 | 1.033 | |

## Multicore (tscore parallel.map vs node/bun worker_threads)

### primes

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 75.5 (1.29x) | 58.7 (1.00x) | 34.2 (0.58x) | **bun** |
| 2 | 40.1 (0.96x) | 41.9 (1.00x) | 23.4 (0.56x) | **bun** |
| 4 | 23.7 (0.78x) | 30.5 (1.00x) | 16.9 (0.55x) | **bun** |
| 8 | 12.2 (0.50x) | 24.5 (1.00x) | 14.0 (0.57x) | **tscore** |

### mandelbrot

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 24.7 (0.75x) | 32.8 (1.00x) | 29.8 (0.91x) | **tscore** |
| 2 | 12.9 (0.54x) | 23.7 (1.00x) | 19.9 (0.84x) | **tscore** |
| 4 | 6.7 (0.30x) | 22.0 (1.00x) | 18.2 (0.82x) | **tscore** |
| 8 | 4.0 (0.18x) | 21.6 (1.00x) | 16.6 (0.77x) | **tscore** |

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
