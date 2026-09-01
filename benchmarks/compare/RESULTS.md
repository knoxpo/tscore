# Engine comparison: tscore vs node (V8) vs bun (JSC)

- date: 2026-09-01 17:34
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
| startup (hello.ts) | 2.2 (0.06x) | 39.7 (1.00x) | 4.2 (0.11x) | **tscore** |
| parse+compile (~50k LOC) | 8.7 (0.09x) | 97.4 (1.00x) | 11.0 (0.11x) | **tscore** |

## Steady-state (shared sources, in-program TIME_MS)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| objects | 142.2 (6.07x) | 23.4 (1.00x) | 23.0 (0.98x) | **bun** |
| closures | 110.7 (3.34x) | 33.2 (1.00x) | 47.1 (1.42x) | **node** |
| alloc | 157.4 (3.66x) | 43.0 (1.00x) | 45.6 (1.06x) | **node** |
| gc_churn | 331.6 (6.00x) | 55.3 (1.00x) | 39.8 (0.72x) | **bun** |
| promises | 61.8 (1.01x) | 61.4 (1.00x) | 56.5 (0.92x) | **bun** |

## Async (per-engine variants, same algorithm)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| timer storm (2000x sleep 1ms) | 12.6 (0.86x) | 14.7 (1.00x) | 13.4 (0.91x) | **tscore** |
| channel 100k msgs (vs worker postMessage) | 5.0 (0.15x) | 33.5 (1.00x) | 28.1 (0.84x) | **tscore** |

## Long-running (30s sustained mixed compute+alloc, single run)

| metric | tscore | node | bun | winner |
|---|---|---|---|---|
| throughput (ops/sec) | 436897 (0.42x) | 1041113 (1.00x) | 742680 (0.71x) | **node** |
| stability (last/first decile) | 0.999 | 1.013 | 0.948 | |

## Multicore (tscore parallel.map vs node/bun worker_threads)

### primes

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 75.4 (1.32x) | 57.1 (1.00x) | 34.9 (0.61x) | **bun** |
| 2 | 39.7 (0.96x) | 41.5 (1.00x) | 23.8 (0.57x) | **bun** |
| 4 | 20.3 (0.71x) | 28.5 (1.00x) | 16.3 (0.57x) | **bun** |
| 8 | 11.6 (0.46x) | 25.1 (1.00x) | 13.4 (0.53x) | **tscore** |

### mandelbrot

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 24.6 (0.74x) | 33.1 (1.00x) | 29.9 (0.90x) | **tscore** |
| 2 | 12.7 (0.53x) | 23.8 (1.00x) | 20.0 (0.84x) | **tscore** |
| 4 | 6.8 (0.31x) | 22.2 (1.00x) | 18.4 (0.83x) | **tscore** |
| 8 | 3.8 (0.18x) | 21.7 (1.00x) | 15.8 (0.73x) | **tscore** |

## Scoreboard

| engine | wins |
|---|---|
| tscore | 9 |
| node | 3 |
| bun | 6 |

Win = fastest median (highest throughput for long-running) on that row.


## HTTP hello (wrk -t2 -c32 -d5s)

| engine | req/s | cores | req/s per core |
|---|---|---|---|
| tscore workers:1 | 219,041 | 1.1 | 201,878 **best/core** |
| bun x8 reusePort | 195,611 | 1.0 | 188,032 |
| bun x1 | 184,177 | 1.0 | 179,468 |
| tscore workers:8 | 241,681 | 1.8 | 131,116 |
| node x1 | 131,914 | 1.0 | 130,641 |
| node cluster x8 | 144,465 | 3.5 | 41,667 |

Total throughput is capped by this machine's loopback stack, not
by any engine: four independent servers with four independent
clients total the same as one. Compare the per-core column.
## Runtime surface

| category | status |
|---|---|
| modules | SHIPPED — full ESM (static + dynamic import, cycles, bare specifiers, TLA) |
| networking | SHIPPED — runtime.net TCP (kqueue reactor, connect) + runtime.http |
| async file I/O | SHIPPED — runtime.fs (promise-native, dedicated I/O pool) + bytes |
| http throughput | see the HTTP section above (benchmarks/compare/http_bench.py) |
| async I/O | proxied by timer-storm + channel benchmarks above |
