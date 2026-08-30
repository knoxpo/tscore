# Engine comparison: tscore vs node (V8) vs bun (JSC)

- date: 2026-08-31 02:50
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
| startup (hello.ts) | 1.4 (0.04x) | 35.5 (1.00x) | 4.1 (0.12x) | **tscore** |
| parse+compile (~50k LOC) | 6.7 (0.07x) | 96.8 (1.00x) | 10.9 (0.11x) | **tscore** |

## Steady-state (shared sources, in-program TIME_MS)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| objects | 178.0 (7.63x) | 23.3 (1.00x) | 22.6 (0.97x) | **bun** |
| closures | 111.1 (3.37x) | 33.0 (1.00x) | 46.5 (1.41x) | **node** |
| alloc | 297.7 (6.46x) | 46.1 (1.00x) | 58.0 (1.26x) | **node** |
| gc_churn | 474.3 (8.91x) | 53.2 (1.00x) | 42.5 (0.80x) | **bun** |
| promises | 62.0 (0.98x) | 63.0 (1.00x) | 58.8 (0.93x) | **bun** |

## Async (per-engine variants, same algorithm)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| timer storm (2000x sleep 1ms) | 12.5 (1.04x) | 12.1 (1.00x) | 11.9 (0.99x) | **bun** |
| channel 100k msgs (vs worker postMessage) | 5.2 (0.15x) | 34.8 (1.00x) | 28.7 (0.83x) | **tscore** |

## Long-running (30s sustained mixed compute+alloc, single run)

| metric | tscore | node | bun | winner |
|---|---|---|---|---|
| throughput (ops/sec) | 288719 (0.28x) | 1024007 (1.00x) | 771922 (0.75x) | **node** |
| stability (last/first decile) | 1.022 | 1.001 | 0.779 | |

## Multicore (tscore parallel.map vs node/bun worker_threads)

### primes

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 97.7 (1.22x) | 79.9 (1.00x) | 54.0 (0.68x) | **bun** |
| 2 | 51.6 (0.84x) | 61.3 (1.00x) | 38.9 (0.63x) | **bun** |
| 4 | 31.5 (0.53x) | 58.9 (1.00x) | 33.3 (0.57x) | **tscore** |
| 8 | 21.0 (0.36x) | 58.2 (1.00x) | 36.1 (0.62x) | **tscore** |

### mandelbrot

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 28.9 (0.56x) | 52.0 (1.00x) | 36.6 (0.70x) | **tscore** |
| 2 | 13.1 (0.53x) | 24.8 (1.00x) | 21.1 (0.85x) | **tscore** |
| 4 | 7.2 (0.30x) | 23.9 (1.00x) | 18.7 (0.78x) | **tscore** |
| 8 | 4.0 (0.15x) | 27.5 (1.00x) | 19.3 (0.70x) | **tscore** |

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
