# Engine comparison: tscore vs node (V8) vs bun (JSC)

- date: 2026-08-30 11:29
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
| startup (hello.ts) | 1.4 (0.04x) | 38.2 (1.00x) | 4.5 (0.12x) | **tscore** |
| parse+compile (~50k LOC) | 19.6 (0.20x) | 98.2 (1.00x) | 11.8 (0.12x) | **bun** |

## Steady-state (shared sources, in-program TIME_MS)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| objects | 1501.2 (64.64x) | 23.2 (1.00x) | 23.3 (1.01x) | **node** |
| closures | 479.4 (14.42x) | 33.2 (1.00x) | 48.4 (1.46x) | **node** |
| alloc | 1082.3 (25.89x) | 41.8 (1.00x) | 46.9 (1.12x) | **node** |
| gc_churn | 967.1 (18.22x) | 53.1 (1.00x) | 43.1 (0.81x) | **bun** |
| promises | 84.6 (1.36x) | 62.2 (1.00x) | 57.1 (0.92x) | **bun** |

## Async (per-engine variants, same algorithm)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| timer storm (2000x sleep 1ms) | 31.3 (2.31x) | 13.6 (1.00x) | 12.1 (0.89x) | **bun** |
| channel 100k msgs (vs worker postMessage) | 6.6 (0.19x) | 34.3 (1.00x) | 28.9 (0.84x) | **tscore** |

## Long-running (30s sustained mixed compute+alloc, single run)

| metric | tscore | node | bun | winner |
|---|---|---|---|---|
| throughput (ops/sec) | 87146 (0.08x) | 1027143 (1.00x) | 782085 (0.76x) | **node** |
| stability (last/first decile) | 0.967 | 1.01 | 1.036 | |

## Multicore (tscore parallel.map vs node/bun worker_threads)

### primes

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 1523.4 (25.25x) | 60.3 (1.00x) | 34.0 (0.56x) | **bun** |
| 2 | 811.5 (18.64x) | 43.5 (1.00x) | 24.5 (0.56x) | **bun** |
| 4 | 410.6 (13.21x) | 31.1 (1.00x) | 16.4 (0.53x) | **bun** |
| 8 | 220.4 (8.68x) | 25.4 (1.00x) | 15.3 (0.60x) | **bun** |

### mandelbrot

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 239.3 (7.13x) | 33.6 (1.00x) | 30.5 (0.91x) | **bun** |
| 2 | 120.1 (4.95x) | 24.3 (1.00x) | 20.1 (0.83x) | **bun** |
| 4 | 61.9 (2.68x) | 23.1 (1.00x) | 18.8 (0.81x) | **bun** |
| 8 | 35.5 (1.53x) | 23.2 (1.00x) | 17.0 (0.74x) | **bun** |

## Scoreboard

| engine | wins |
|---|---|
| tscore | 2 |
| node | 4 |
| bun | 12 |

Win = fastest median (highest throughput for long-running) on that row.

## Not benchmarkable yet

| category | status |
|---|---|
| modules | N/A — tscore is single-file entry only (import/export rejected) |
| networking | N/A — no sockets in runtime (planned post-M4) |
| async file I/O | N/A — no TS-visible fs API |
| async I/O | proxied by timer-storm + channel benchmarks above |
