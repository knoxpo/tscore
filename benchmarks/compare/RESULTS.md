# Engine comparison: tscore vs node (V8) vs bun (JSC)

- date: 2026-08-30 13:35
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
| startup (hello.ts) | 1.3 (0.03x) | 37.2 (1.00x) | 4.1 (0.11x) | **tscore** |
| parse+compile (~50k LOC) | 19.7 (0.20x) | 98.3 (1.00x) | 11.2 (0.11x) | **bun** |

## Steady-state (shared sources, in-program TIME_MS)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| objects | 1157.2 (49.22x) | 23.5 (1.00x) | 24.0 (1.02x) | **node** |
| closures | 496.6 (15.04x) | 33.0 (1.00x) | 46.7 (1.42x) | **node** |
| alloc | 1195.5 (27.94x) | 42.8 (1.00x) | 45.8 (1.07x) | **node** |
| gc_churn | 1020.6 (19.34x) | 52.8 (1.00x) | 43.4 (0.82x) | **bun** |
| promises | 83.8 (1.35x) | 62.2 (1.00x) | 55.8 (0.90x) | **bun** |

## Async (per-engine variants, same algorithm)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| timer storm (2000x sleep 1ms) | 15.5 (1.05x) | 14.7 (1.00x) | 13.4 (0.91x) | **bun** |
| channel 100k msgs (vs worker postMessage) | 6.1 (0.17x) | 35.1 (1.00x) | 28.6 (0.81x) | **tscore** |

## Long-running (30s sustained mixed compute+alloc, single run)

| metric | tscore | node | bun | winner |
|---|---|---|---|---|
| throughput (ops/sec) | 116915 (0.11x) | 1031376 (1.00x) | 791932 (0.77x) | **node** |
| stability (last/first decile) | 0.987 | 1.018 | 0.972 | |

## Multicore (tscore parallel.map vs node/bun worker_threads)

### primes

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 351.1 (6.10x) | 57.6 (1.00x) | 34.3 (0.59x) | **bun** |
| 2 | 190.4 (4.44x) | 42.9 (1.00x) | 23.6 (0.55x) | **bun** |
| 4 | 95.5 (3.13x) | 30.5 (1.00x) | 16.5 (0.54x) | **bun** |
| 8 | 49.4 (1.98x) | 24.9 (1.00x) | 14.3 (0.57x) | **bun** |

### mandelbrot

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 120.5 (3.74x) | 32.2 (1.00x) | 29.1 (0.90x) | **bun** |
| 2 | 62.9 (2.66x) | 23.7 (1.00x) | 19.9 (0.84x) | **bun** |
| 4 | 33.0 (1.49x) | 22.1 (1.00x) | 18.2 (0.82x) | **bun** |
| 8 | 16.9 (0.78x) | 21.7 (1.00x) | 16.4 (0.75x) | **bun** |

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
