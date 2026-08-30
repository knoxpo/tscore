# Engine comparison: tscore vs node (V8) vs bun (JSC)

- date: 2026-08-30 18:47
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
| startup (hello.ts) | 1.8 (0.05x) | 37.6 (1.00x) | 5.8 (0.15x) | **tscore** |
| parse+compile (~50k LOC) | 24.8 (0.23x) | 106.4 (1.00x) | 12.3 (0.12x) | **bun** |

## Steady-state (shared sources, in-program TIME_MS)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| objects | 186.0 (7.81x) | 23.8 (1.00x) | 23.4 (0.98x) | **bun** |
| closures | 133.6 (3.91x) | 34.2 (1.00x) | 48.8 (1.43x) | **node** |
| alloc | 223.9 (5.13x) | 43.7 (1.00x) | 49.2 (1.13x) | **node** |
| gc_churn | 646.6 (12.63x) | 51.2 (1.00x) | 42.7 (0.83x) | **bun** |
| promises | 63.1 (0.99x) | 63.4 (1.00x) | 57.1 (0.90x) | **bun** |

## Async (per-engine variants, same algorithm)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| timer storm (2000x sleep 1ms) | 12.7 (1.05x) | 12.0 (1.00x) | 12.0 (1.00x) | **bun** |
| channel 100k msgs (vs worker postMessage) | 5.7 (0.17x) | 33.6 (1.00x) | 28.9 (0.86x) | **tscore** |

## Long-running (30s sustained mixed compute+alloc, single run)

| metric | tscore | node | bun | winner |
|---|---|---|---|---|
| throughput (ops/sec) | 353430 (0.36x) | 989427 (1.00x) | 716899 (0.72x) | **node** |
| stability (last/first decile) | 0.955 | 1.021 | 1.036 | |

## Multicore (tscore parallel.map vs node/bun worker_threads)

### primes

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 76.2 (1.21x) | 62.9 (1.00x) | 35.4 (0.56x) | **bun** |
| 2 | 40.0 (0.93x) | 42.9 (1.00x) | 24.1 (0.56x) | **bun** |
| 4 | 23.3 (0.76x) | 30.7 (1.00x) | 17.3 (0.56x) | **bun** |
| 8 | 12.6 (0.49x) | 25.6 (1.00x) | 14.6 (0.57x) | **tscore** |

### mandelbrot

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 24.8 (0.72x) | 34.5 (1.00x) | 30.7 (0.89x) | **tscore** |
| 2 | 12.7 (0.52x) | 24.6 (1.00x) | 20.3 (0.83x) | **tscore** |
| 4 | 6.8 (0.22x) | 30.9 (1.00x) | 18.8 (0.61x) | **tscore** |
| 8 | 5.0 (0.22x) | 22.4 (1.00x) | 17.0 (0.76x) | **tscore** |

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
