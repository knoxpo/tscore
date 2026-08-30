# Engine comparison: tscore vs node (V8) vs bun (JSC)

- date: 2026-08-30 14:54
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
| startup (hello.ts) | 1.4 (0.04x) | 38.4 (1.00x) | 4.8 (0.12x) | **tscore** |
| parse+compile (~50k LOC) | 20.8 (0.20x) | 101.6 (1.00x) | 12.3 (0.12x) | **bun** |

## Steady-state (shared sources, in-program TIME_MS)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| objects | 327.1 (13.75x) | 23.8 (1.00x) | 23.4 (0.98x) | **bun** |
| closures | 321.8 (9.32x) | 34.5 (1.00x) | 57.2 (1.66x) | **node** |
| alloc | 1063.8 (24.18x) | 44.0 (1.00x) | 47.7 (1.08x) | **node** |
| gc_churn | 931.8 (16.92x) | 55.1 (1.00x) | 42.7 (0.78x) | **bun** |
| promises | 84.7 (1.33x) | 63.5 (1.00x) | 57.5 (0.91x) | **bun** |

## Async (per-engine variants, same algorithm)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| timer storm (2000x sleep 1ms) | 13.3 (1.06x) | 12.6 (1.00x) | 12.2 (0.97x) | **bun** |
| channel 100k msgs (vs worker postMessage) | 6.2 (0.18x) | 34.7 (1.00x) | 29.4 (0.85x) | **tscore** |

## Long-running (30s sustained mixed compute+alloc, single run)

| metric | tscore | node | bun | winner |
|---|---|---|---|---|
| throughput (ops/sec) | 161641 (0.16x) | 1026278 (1.00x) | 822665 (0.80x) | **node** |
| stability (last/first decile) | 0.998 | 1.013 | 1.026 | |

## Multicore (tscore parallel.map vs node/bun worker_threads)

### primes

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 288.7 (4.53x) | 63.7 (1.00x) | 35.0 (0.55x) | **bun** |
| 2 | 156.2 (3.52x) | 44.4 (1.00x) | 24.1 (0.54x) | **bun** |
| 4 | 79.3 (2.47x) | 32.1 (1.00x) | 16.9 (0.53x) | **bun** |
| 8 | 40.7 (1.59x) | 25.7 (1.00x) | 14.6 (0.57x) | **bun** |

### mandelbrot

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 117.6 (3.47x) | 33.8 (1.00x) | 30.4 (0.90x) | **bun** |
| 2 | 64.2 (2.57x) | 25.0 (1.00x) | 20.4 (0.82x) | **bun** |
| 4 | 32.2 (1.39x) | 23.2 (1.00x) | 18.9 (0.82x) | **bun** |
| 8 | 16.4 (0.72x) | 22.7 (1.00x) | 17.5 (0.77x) | **tscore** |

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
