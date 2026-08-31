# Engine comparison: tscore vs node (V8) vs bun (JSC)

- date: 2026-08-31 19:43
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
| startup (hello.ts) | 1.6 (0.04x) | 36.7 (1.00x) | 4.2 (0.11x) | **tscore** |
| parse+compile (~50k LOC) | 7.5 (0.08x) | 98.2 (1.00x) | 11.2 (0.11x) | **tscore** |

## Steady-state (shared sources, in-program TIME_MS)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| objects | 168.3 (7.09x) | 23.7 (1.00x) | 23.2 (0.98x) | **bun** |
| closures | 115.4 (3.38x) | 34.1 (1.00x) | 47.7 (1.40x) | **node** |
| alloc | 177.8 (4.06x) | 43.8 (1.00x) | 45.7 (1.04x) | **node** |
| gc_churn | 453.3 (8.52x) | 53.2 (1.00x) | 41.3 (0.78x) | **bun** |
| promises | 63.5 (1.01x) | 63.2 (1.00x) | 57.1 (0.90x) | **bun** |

## Async (per-engine variants, same algorithm)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| timer storm (2000x sleep 1ms) | 15.0 (1.03x) | 14.5 (1.00x) | 13.4 (0.92x) | **bun** |
| channel 100k msgs (vs worker postMessage) | 5.2 (0.15x) | 34.1 (1.00x) | 28.3 (0.83x) | **tscore** |

## Long-running (30s sustained mixed compute+alloc, single run)

| metric | tscore | node | bun | winner |
|---|---|---|---|---|
| throughput (ops/sec) | 435657 (0.42x) | 1034130 (1.00x) | 799257 (0.77x) | **node** |
| stability (last/first decile) | 0.999 | 1.018 | 1.036 | |

## HTTP hello (wrk -t4 -c64 -d5s, 8 workers each; `benchmarks/compare/http_bench.sh`)

| engine | req/s | vs node | winner |
|---|---|---|---|
| tscore (http.serve) | 127,404 | 0.74x | |
| node (cluster) | 172,116 | 1.00x | |
| bun (Bun.serve) | 195,430 | 1.14x | **bun** |

## Multicore (tscore parallel.map vs node/bun worker_threads)

### primes

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 73.5 (1.24x) | 59.5 (1.00x) | 33.5 (0.56x) | **bun** |
| 2 | 39.6 (0.97x) | 40.9 (1.00x) | 23.7 (0.58x) | **bun** |
| 4 | 20.4 (0.65x) | 31.4 (1.00x) | 16.7 (0.53x) | **bun** |
| 8 | 11.8 (0.47x) | 24.9 (1.00x) | 14.2 (0.57x) | **tscore** |

### mandelbrot

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 24.4 (0.74x) | 33.0 (1.00x) | 29.3 (0.89x) | **tscore** |
| 2 | 12.7 (0.53x) | 23.9 (1.00x) | 20.0 (0.84x) | **tscore** |
| 4 | 6.8 (0.31x) | 22.3 (1.00x) | 18.4 (0.83x) | **tscore** |
| 8 | 3.8 (0.17x) | 22.1 (1.00x) | 16.1 (0.73x) | **tscore** |

## Scoreboard

| engine | wins |
|---|---|
| tscore | 8 |
| node | 3 |
| bun | 7 |

Win = fastest median (highest throughput for long-running) on that row.

## Not benchmarkable yet

| category | status |
|---|---|
| modules | N/A — tscore is single-file entry only (import/export rejected) |
| networking | N/A — no sockets in runtime (planned post-M4) |
| async file I/O | N/A — no TS-visible fs API |
| async I/O | proxied by timer-storm + channel benchmarks above |
