# Engine comparison: tscore vs node (V8) vs bun (JSC)

- date: 2026-08-30 20:03
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
| startup (hello.ts) | 1.6 (0.04x) | 40.1 (1.00x) | 5.2 (0.13x) | **tscore** |
| parse+compile (~50k LOC) | 14.2 (0.13x) | 108.0 (1.00x) | 12.5 (0.12x) | **bun** |

## Steady-state (shared sources, in-program TIME_MS)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| objects | 187.9 (7.94x) | 23.7 (1.00x) | 23.2 (0.98x) | **bun** |
| closures | 139.6 (3.08x) | 45.3 (1.00x) | 70.1 (1.55x) | **node** |
| alloc | 352.2 (6.78x) | 51.9 (1.00x) | 65.6 (1.26x) | **node** |
| gc_churn | 655.6 (11.90x) | 55.1 (1.00x) | 43.1 (0.78x) | **bun** |
| promises | 62.9 (0.98x) | 64.0 (1.00x) | 58.5 (0.92x) | **bun** |

## Async (per-engine variants, same algorithm)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| timer storm (2000x sleep 1ms) | 12.6 (1.02x) | 12.3 (1.00x) | 12.1 (0.98x) | **bun** |
| channel 100k msgs (vs worker postMessage) | 5.6 (0.16x) | 34.7 (1.00x) | 29.3 (0.84x) | **tscore** |

## Long-running (30s sustained mixed compute+alloc, single run)

| metric | tscore | node | bun | winner |
|---|---|---|---|---|
| throughput (ops/sec) | 350895 (0.34x) | 1025431 (1.00x) | 815752 (0.80x) | **node** |
| stability (last/first decile) | 1.006 | 1.019 | 0.996 | |

## Multicore (tscore parallel.map vs node/bun worker_threads)

### primes

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 75.5 (1.26x) | 60.1 (1.00x) | 34.7 (0.58x) | **bun** |
| 2 | 39.9 (0.91x) | 43.7 (1.00x) | 23.8 (0.54x) | **bun** |
| 4 | 23.5 (0.78x) | 30.2 (1.00x) | 16.8 (0.55x) | **bun** |
| 8 | 12.6 (0.49x) | 25.6 (1.00x) | 14.6 (0.57x) | **tscore** |

### mandelbrot

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 24.8 (0.73x) | 34.2 (1.00x) | 30.8 (0.90x) | **tscore** |
| 2 | 12.9 (0.54x) | 23.9 (1.00x) | 20.2 (0.84x) | **tscore** |
| 4 | 6.8 (0.29x) | 23.0 (1.00x) | 18.6 (0.81x) | **tscore** |
| 8 | 4.8 (0.20x) | 23.2 (1.00x) | 17.0 (0.73x) | **tscore** |

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
