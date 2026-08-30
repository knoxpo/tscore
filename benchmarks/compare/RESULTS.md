# Engine comparison: tscore vs node (V8) vs bun (JSC)

- date: 2026-08-30 18:12
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
| startup (hello.ts) | 1.8 (0.05x) | 39.4 (1.00x) | 7.7 (0.20x) | **tscore** |
| parse+compile (~50k LOC) | 25.6 (0.21x) | 120.4 (1.00x) | 14.0 (0.12x) | **bun** |

## Steady-state (shared sources, in-program TIME_MS)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| objects | 192.7 (7.95x) | 24.2 (1.00x) | 24.0 (0.99x) | **bun** |
| closures | 134.2 (3.85x) | 34.9 (1.00x) | 49.4 (1.42x) | **node** |
| alloc | 230.2 (5.23x) | 44.1 (1.00x) | 47.8 (1.09x) | **node** |
| gc_churn | 648.3 (11.43x) | 56.7 (1.00x) | 43.3 (0.76x) | **bun** |
| promises | 64.3 (0.98x) | 65.4 (1.00x) | 57.7 (0.88x) | **bun** |

## Async (per-engine variants, same algorithm)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| timer storm (2000x sleep 1ms) | 12.7 (0.95x) | 13.3 (1.00x) | 12.0 (0.90x) | **bun** |
| channel 100k msgs (vs worker postMessage) | 5.6 (0.16x) | 34.7 (1.00x) | 29.6 (0.85x) | **tscore** |

## Long-running (30s sustained mixed compute+alloc, single run)

| metric | tscore | node | bun | winner |
|---|---|---|---|---|
| throughput (ops/sec) | 340143 (0.34x) | 1007243 (1.00x) | 788476 (0.78x) | **node** |
| stability (last/first decile) | 1.005 | 1.026 | 1 | |

## Multicore (tscore parallel.map vs node/bun worker_threads)

### primes

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 76.0 (1.24x) | 61.3 (1.00x) | 34.8 (0.57x) | **bun** |
| 2 | 39.9 (0.91x) | 43.7 (1.00x) | 23.7 (0.54x) | **bun** |
| 4 | 23.0 (0.75x) | 30.5 (1.00x) | 16.3 (0.54x) | **bun** |
| 8 | 11.7 (0.46x) | 25.3 (1.00x) | 14.3 (0.57x) | **tscore** |

### mandelbrot

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 24.7 (0.73x) | 34.0 (1.00x) | 30.7 (0.90x) | **tscore** |
| 2 | 12.6 (0.51x) | 24.7 (1.00x) | 20.4 (0.83x) | **tscore** |
| 4 | 7.1 (0.31x) | 22.8 (1.00x) | 18.7 (0.82x) | **tscore** |
| 8 | 3.8 (0.16x) | 23.3 (1.00x) | 17.4 (0.75x) | **tscore** |

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
