# Engine comparison: tscore vs node (V8) vs bun (JSC)

- date: 2026-08-30 03:27
- machine: Apple M5 Max, 18 logical cpus (6P + 12E)
- tscore: tscore 0.1.0, node: v24.17.0, bun: 1.4.0
- steady-state cells: median TIME_MS of 5 runs, first run discarded; in-program warmup pass before timing
- ratios: engine_ms / node_ms — lower is better, <1.00x is faster than node
- methodology: shared benchmarks are byte-identical sources in the tscore language subset;
  node/bun run non-idiomatic code (no Array.map, classes, etc.) — this compares engines on
  identical programs, not idiomatic-per-engine programs

## Startup + parse/compile (end-to-end, hyperfine)

| benchmark | tscore | node | bun |
|---|---|---|---|
| startup (hello.ts) | 1.5 (0.04x) | 39.2 (1.00x) | 5.0 (0.13x) |
| parse+compile (~50k LOC) | 21.3 (0.20x) | 105.4 (1.00x) | 12.0 (0.11x) |

## Steady-state (shared sources, in-program TIME_MS)

| benchmark | tscore | node | bun |
|---|---|---|---|
| objects | 1566.7 (65.11x) | 24.1 (1.00x) | 23.8 (0.99x) |
| closures | 502.2 (13.98x) | 35.9 (1.00x) | 51.5 (1.43x) |
| alloc | 1204.9 (26.95x) | 44.7 (1.00x) | 51.9 (1.16x) |
| gc_churn | 1036.6 (17.15x) | 60.4 (1.00x) | 45.0 (0.74x) |
| promises | 88.5 (1.35x) | 65.5 (1.00x) | 60.0 (0.92x) |

## Async (per-engine variants, same algorithm)

| benchmark | tscore | node | bun |
|---|---|---|---|
| timer storm (2000x sleep 1ms) | 30.0 (2.14x) | 14.0 (1.00x) | 12.6 (0.90x) |
| channel 100k msgs (vs worker postMessage) | 6.9 (0.13x) | 53.9 (1.00x) | 39.0 (0.72x) |

## Long-running (30s sustained mixed compute+alloc, single run)

| metric | tscore | node | bun |
|---|---|---|---|
| throughput (ops/sec) | 84387 (0.08x) | 1001150 (1.00x) | 816544 (0.82x) |
| stability (last/first decile) | 1.05 | 1.075 | 1.033 |

## Multicore (tscore parallel.map vs node/bun worker_threads)

### primes

| workers | tscore | node | bun |
|---|---|---|---|
| 1 | 103.1 (1.69x) | 61.0 (1.00x) | 34.9 (0.57x) |
| 2 | 102.7 (2.37x) | 43.3 (1.00x) | 24.2 (0.56x) |
| 4 | 120.8 (3.84x) | 31.5 (1.00x) | 17.8 (0.57x) |
| 8 | 113.3 (4.40x) | 25.7 (1.00x) | 15.2 (0.59x) |

### mandelbrot

| workers | tscore | node | bun |
|---|---|---|---|
| 1 | 19.0 (0.57x) | 33.5 (1.00x) | 30.3 (0.90x) |
| 2 | 18.8 (0.80x) | 23.5 (1.00x) | 19.8 (0.84x) |
| 4 | 19.3 (0.87x) | 22.1 (1.00x) | 18.3 (0.83x) |
| 8 | 18.8 (0.86x) | 21.8 (1.00x) | 16.6 (0.76x) |

## Not benchmarkable yet

| category | status |
|---|---|
| modules | N/A — tscore is single-file entry only (import/export rejected) |
| networking | N/A — no sockets in runtime (planned post-M4) |
| async file I/O | N/A — no TS-visible fs API |
| async I/O | proxied by timer-storm + channel benchmarks above |
