# Engine comparison: tscore vs node (V8) vs bun (JSC)

- date: 2026-09-06 18:35
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
| startup (hello.ts) | 2.5 (0.04x) | 56.4 (1.00x) | 6.5 (0.12x) | **tscore** |
| parse+compile (~50k LOC) | 13.3 (0.09x) | 147.4 (1.00x) | 17.1 (0.12x) | **tscore** |

## Steady-state (shared sources, in-program TIME_MS)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| objects | 100.7 (2.83x) | 35.5 (1.00x) | 35.8 (1.01x) | **node** |
| closures | 150.1 (2.92x) | 51.5 (1.00x) | 70.0 (1.36x) | **node** |
| alloc | 114.5 (1.76x) | 65.1 (1.00x) | 71.4 (1.10x) | **node** |
| gc_churn | 181.3 (2.33x) | 77.8 (1.00x) | 60.3 (0.78x) | **bun** |
| promises | 86.5 (0.90x) | 96.3 (1.00x) | 88.0 (0.91x) | **tscore** |

## Async (per-engine variants, same algorithm)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| timer storm (2000x sleep 1ms) | 12.1 (0.94x) | 12.9 (1.00x) | 12.2 (0.94x) | **tscore** |
| channel 100k msgs (vs worker postMessage) | 7.7 (0.15x) | 51.1 (1.00x) | 43.8 (0.86x) | **tscore** |

## Long-running (30s sustained mixed compute+alloc, single run)

| metric | tscore | node | bun | winner |
|---|---|---|---|---|
| throughput (ops/sec) | 499383 (0.77x) | 648551 (1.00x) | 478378 (0.74x) | **node** |
| stability (last/first decile) | 1.002 | 1.085 | 0.79 | |

## Multicore (tscore parallel.map vs node/bun worker_threads)

### primes

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 93.0 (0.79x) | 117.4 (1.00x) | 52.9 (0.45x) | **bun** |
| 2 | 41.0 (0.61x) | 67.4 (1.00x) | 37.8 (0.56x) | **bun** |
| 4 | 20.8 (0.46x) | 45.0 (1.00x) | 24.8 (0.55x) | **tscore** |
| 8 | 11.1 (0.32x) | 34.5 (1.00x) | 18.7 (0.54x) | **tscore** |

### mandelbrot

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 37.2 (0.74x) | 50.2 (1.00x) | 45.3 (0.90x) | **tscore** |
| 2 | 19.0 (0.54x) | 35.1 (1.00x) | 29.8 (0.85x) | **tscore** |
| 4 | 10.0 (0.31x) | 32.4 (1.00x) | 26.9 (0.83x) | **tscore** |
| 8 | 5.1 (0.17x) | 30.4 (1.00x) | 22.1 (0.73x) | **tscore** |

## Scoreboard

| engine | wins |
|---|---|
| tscore | 11 |
| node | 4 |
| bun | 3 |

Win = fastest median (highest throughput for long-running) on that row.


## HTTP hello (wrk -t2 -c32 -d5s)

| engine | req/s | cores | req/s per core |
|---|---|---|---|
| tscore workers:1 | 135,639 | 1.0 | 136,885 **best/core** |
| tscore workers:8 | 148,893 | 1.9 | 79,453 |
| bun x8 reusePort | 68,544 | 1.0 | 67,581 |
| bun x1 | 64,928 | 1.0 | 65,325 |
| node x1 | 63,863 | 1.0 | 64,328 |
| node cluster x8 | 107,128 | 3.7 | 28,735 |

Total throughput is capped by this machine's loopback stack, not
by any engine: four independent servers with four independent
clients total the same as one. Compare the per-core column.
## Runtime surface

| category | status |
|---|---|
| modules | SHIPPED — full ESM: static + dynamic import, cycles (TDZ), TLA; bare specifiers via tscore.json and node_modules (package.json exports/main); .js/.mjs/.cjs specifiers resolve to TS sources; import.meta.url/filename/dirname |
| networking | SHIPPED — runtime.net TCP (kqueue reactor, connect) + runtime.http |
| async file I/O | SHIPPED — runtime.fs (promise-native, dedicated I/O pool) + bytes |
| http throughput | see the HTTP section above (benchmarks/compare/http_bench.py) |
| async I/O | proxied by timer-storm + channel benchmarks above |
