# Engine comparison: tscore vs node (V8) vs bun (JSC)

- date: 2026-08-31 14:41
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
| startup (hello.ts) | 5.6 (0.05x) | 103.8 (1.00x) | 20.8 (0.20x) | **tscore** |
| parse+compile (~50k LOC) | 20.0 (0.07x) | 288.7 (1.00x) | 38.1 (0.13x) | **tscore** |

## Steady-state (shared sources, in-program TIME_MS)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| objects | 686.2 (6.70x) | 102.5 (1.00x) | 72.0 (0.70x) | **bun** |
| closures | 336.0 (3.19x) | 105.2 (1.00x) | 381.7 (3.63x) | **node** |
| alloc | 882.9 (7.96x) | 110.9 (1.00x) | 419.3 (3.78x) | **node** |
| gc_churn | 2031.8 (10.60x) | 191.6 (1.00x) | 183.5 (0.96x) | **bun** |
| promises | 167.9 (0.74x) | 225.4 (1.00x) | 239.7 (1.06x) | **tscore** |

## Async (per-engine variants, same algorithm)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| timer storm (2000x sleep 1ms) | 47.0 (3.28x) | 14.3 (1.00x) | 12.0 (0.84x) | **bun** |
| channel 100k msgs (vs worker postMessage) | 7.5 (0.15x) | 49.7 (1.00x) | 41.6 (0.84x) | **tscore** |

## Long-running (30s sustained mixed compute+alloc, single run)

| metric | tscore | node | bun | winner |
|---|---|---|---|---|
| throughput (ops/sec) | 156338 (0.24x) | 648544 (1.00x) | 501431 (0.77x) | **node** |
| stability (last/first decile) | 1.014 | 0.989 | 0.998 | |

## Multicore (tscore parallel.map vs node/bun worker_threads)

### primes

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 113.9 (1.19x) | 95.8 (1.00x) | 52.0 (0.54x) | **bun** |
| 2 | 59.1 (0.92x) | 64.2 (1.00x) | 35.7 (0.56x) | **bun** |
| 4 | 30.2 (0.67x) | 45.0 (1.00x) | 24.9 (0.55x) | **bun** |
| 8 | 15.6 (0.46x) | 34.0 (1.00x) | 18.2 (0.54x) | **tscore** |

### mandelbrot

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 38.6 (0.76x) | 51.0 (1.00x) | 45.4 (0.89x) | **tscore** |
| 2 | 19.0 (0.55x) | 34.4 (1.00x) | 29.6 (0.86x) | **tscore** |
| 4 | 10.0 (0.31x) | 32.2 (1.00x) | 26.9 (0.84x) | **tscore** |
| 8 | 5.3 (0.17x) | 30.2 (1.00x) | 22.1 (0.73x) | **tscore** |

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
