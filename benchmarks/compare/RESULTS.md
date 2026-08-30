# Engine comparison: tscore vs node (V8) vs bun (JSC)

- date: 2026-08-30 14:39
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
| startup (hello.ts) | 2.0 (0.04x) | 54.8 (1.00x) | 6.5 (0.12x) | **tscore** |
| parse+compile (~50k LOC) | 30.9 (0.20x) | 153.6 (1.00x) | 17.5 (0.11x) | **bun** |

## Steady-state (shared sources, in-program TIME_MS)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| objects | 717.3 (20.02x) | 35.8 (1.00x) | 35.4 (0.99x) | **bun** |
| closures | 503.0 (9.79x) | 51.4 (1.00x) | 70.4 (1.37x) | **node** |
| alloc | 1575.6 (24.29x) | 64.9 (1.00x) | 69.5 (1.07x) | **node** |
| gc_churn | 1426.6 (18.65x) | 76.5 (1.00x) | 61.1 (0.80x) | **bun** |
| promises | 126.3 (1.33x) | 94.8 (1.00x) | 86.3 (0.91x) | **bun** |

## Async (per-engine variants, same algorithm)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| timer storm (2000x sleep 1ms) | 13.2 (1.03x) | 12.9 (1.00x) | 12.2 (0.95x) | **bun** |
| channel 100k msgs (vs worker postMessage) | 9.5 (0.18x) | 52.6 (1.00x) | 42.8 (0.81x) | **tscore** |

## Long-running (30s sustained mixed compute+alloc, single run)

| metric | tscore | node | bun | winner |
|---|---|---|---|---|
| throughput (ops/sec) | 102404 (0.16x) | 655988 (1.00x) | 522108 (0.80x) | **node** |
| stability (last/first decile) | 1.002 | 1.017 | 0.972 | |

## Multicore (tscore parallel.map vs node/bun worker_threads)

### primes

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 464.8 (5.06x) | 91.8 (1.00x) | 51.4 (0.56x) | **bun** |
| 2 | 232.8 (3.86x) | 60.3 (1.00x) | 35.1 (0.58x) | **bun** |
| 4 | 117.5 (2.65x) | 44.4 (1.00x) | 25.0 (0.56x) | **bun** |
| 8 | 54.8 (1.62x) | 33.9 (1.00x) | 18.6 (0.55x) | **bun** |

### mandelbrot

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 183.9 (3.62x) | 50.8 (1.00x) | 45.5 (0.89x) | **bun** |
| 2 | 92.6 (2.64x) | 35.0 (1.00x) | 30.0 (0.86x) | **bun** |
| 4 | 47.6 (1.46x) | 32.6 (1.00x) | 26.8 (0.82x) | **bun** |
| 8 | 21.3 (0.70x) | 30.2 (1.00x) | 22.2 (0.74x) | **tscore** |

## Scoreboard

| engine | wins |
|---|---|
| tscore | 3 |
| node | 3 |
| bun | 12 |

Win = fastest median (highest throughput for long-running) on that row.

## Not benchmarkable yet

| category | status |
|---|---|
| modules | N/A — tscore is single-file entry only (import/export rejected) |
| networking | N/A — no sockets in runtime (planned post-M4) |
| async file I/O | N/A — no TS-visible fs API |
| async I/O | proxied by timer-storm + channel benchmarks above |
