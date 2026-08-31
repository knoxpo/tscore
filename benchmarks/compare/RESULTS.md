# Engine comparison: tscore vs node (V8) vs bun (JSC)

- date: 2026-08-31 20:06
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
| startup (hello.ts) | 1.6 (0.04x) | 36.6 (1.00x) | 4.2 (0.11x) | **tscore** |
| parse+compile (~50k LOC) | 7.6 (0.08x) | 96.8 (1.00x) | 10.9 (0.11x) | **tscore** |

## Steady-state (shared sources, in-program TIME_MS)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| objects | 162.8 (7.04x) | 23.1 (1.00x) | 23.4 (1.01x) | **node** |
| closures | 110.2 (3.32x) | 33.1 (1.00x) | 48.3 (1.46x) | **node** |
| alloc | 174.2 (4.13x) | 42.2 (1.00x) | 46.7 (1.11x) | **node** |
| gc_churn | 453.9 (8.69x) | 52.3 (1.00x) | 42.8 (0.82x) | **bun** |
| promises | 61.8 (1.01x) | 61.3 (1.00x) | 56.7 (0.92x) | **bun** |

## Async (per-engine variants, same algorithm)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| timer storm (2000x sleep 1ms) | 15.0 (1.12x) | 13.4 (1.00x) | 13.3 (0.99x) | **bun** |
| channel 100k msgs (vs worker postMessage) | 5.0 (0.15x) | 33.8 (1.00x) | 28.1 (0.83x) | **tscore** |

## Long-running (30s sustained mixed compute+alloc, single run)

| metric | tscore | node | bun | winner |
|---|---|---|---|---|
| throughput (ops/sec) | 431333 (0.42x) | 1034509 (1.00x) | 834778 (0.81x) | **node** |
| stability (last/first decile) | 0.996 | 1.013 | 1.001 | |

## HTTP hello, single-threaded (wrk -t2 -c32 -d5s; `benchmarks/compare/http_bench.sh`)

Per-core comparison: one tscore worker, one node process, one Bun.serve
event loop (its default). This is the honest engine head-to-head.

| engine | req/s | vs node | winner |
|---|---|---|---|
| tscore (workers:1) | 213,806 | 1.49x | **tscore** |
| node (single process) | 143,172 | 1.00x | |
| bun (Bun.serve) | 189,753 | 1.33x | |

## HTTP hello, 8 workers

Scale-out on a loopback benchmark is client-bound for every engine —
a ceiling check, not a speedup.

| engine | req/s | vs node | vs own 1-thread |
|---|---|---|---|
| tscore (workers:8) | 230,785 | 1.23x | 1.08x |
| node (cluster x8) | 187,523 | 1.00x | 1.31x |
| bun | single-threaded by default | — | — |
