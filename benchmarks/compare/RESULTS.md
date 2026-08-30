# Engine comparison: tscore vs node (V8) vs bun (JSC)

- date: 2026-08-30 13:16
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
| startup (hello.ts) | 1.4 (0.04x) | 38.8 (1.00x) | 4.3 (0.11x) | **tscore** |
| parse+compile (~50k LOC) | 20.9 (0.20x) | 102.7 (1.00x) | 11.6 (0.11x) | **bun** |

## Steady-state (shared sources, in-program TIME_MS)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| objects | 1549.4 (65.50x) | 23.7 (1.00x) | 23.8 (1.01x) | **node** |
| closures | 529.2 (15.31x) | 34.6 (1.00x) | 51.9 (1.50x) | **node** |
| alloc | 1153.7 (26.75x) | 43.1 (1.00x) | 50.6 (1.17x) | **node** |
| gc_churn | 1050.3 (18.07x) | 58.1 (1.00x) | 48.9 (0.84x) | **bun** |
| promises | 86.1 (1.32x) | 65.1 (1.00x) | 63.2 (0.97x) | **bun** |

## Async (per-engine variants, same algorithm)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| timer storm (2000x sleep 1ms) | 32.1 (2.23x) | 14.4 (1.00x) | 13.0 (0.90x) | **bun** |
| channel 100k msgs (vs worker postMessage) | 9.3 (0.22x) | 41.5 (1.00x) | 32.8 (0.79x) | **tscore** |

## Long-running (30s sustained mixed compute+alloc, single run)

| metric | tscore | node | bun | winner |
|---|---|---|---|---|
| throughput (ops/sec) | 89937 (0.09x) | 1018335 (1.00x) | 828682 (0.81x) | **node** |
| stability (last/first decile) | 1.065 | 1.062 | 0.953 | |

## Multicore (tscore parallel.map vs node/bun worker_threads)

### primes

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
