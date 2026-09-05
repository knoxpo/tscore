# Engine comparison: tscore vs node (V8) vs bun (JSC)

- date: 2026-09-06 00:56
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
| startup (hello.ts) | 2.1 (0.05x) | 43.2 (1.00x) | 5.8 (0.13x) | **tscore** |
| parse+compile (~50k LOC) | 8.8 (0.08x) | 109.0 (1.00x) | 13.7 (0.13x) | **tscore** |

## Steady-state (shared sources, in-program TIME_MS)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| objects | 109.3 (4.25x) | 25.7 (1.00x) | 24.2 (0.94x) | **bun** |
| closures | 105.3 (2.87x) | 36.7 (1.00x) | 52.7 (1.44x) | **node** |
| alloc | 137.9 (3.06x) | 45.1 (1.00x) | 50.5 (1.12x) | **node** |
| gc_churn | 213.7 (3.67x) | 58.3 (1.00x) | 46.4 (0.80x) | **bun** |
| promises | 66.2 (0.99x) | 66.7 (1.00x) | 61.1 (0.92x) | **bun** |

## Async (per-engine variants, same algorithm)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| timer storm (2000x sleep 1ms) | 11.6 (0.88x) | 13.2 (1.00x) | 11.9 (0.91x) | **tscore** |
| channel 100k msgs (vs worker postMessage) | 5.3 (0.14x) | 36.8 (1.00x) | 30.2 (0.82x) | **tscore** |

## Long-running (30s sustained mixed compute+alloc, single run)

| metric | tscore | node | bun | winner |
|---|---|---|---|---|
| throughput (ops/sec) | 637337 (0.67x) | 949464 (1.00x) | 715063 (0.75x) | **node** |
| stability (last/first decile) | 0.966 | 1.007 | 0.969 | |

## Multicore (tscore parallel.map vs node/bun worker_threads)

### primes

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 78.2 (1.19x) | 65.8 (1.00x) | 37.2 (0.56x) | **bun** |
| 2 | 40.3 (0.91x) | 44.4 (1.00x) | 25.3 (0.57x) | **bun** |
| 4 | 21.0 (0.66x) | 31.8 (1.00x) | 17.8 (0.56x) | **bun** |
| 8 | 12.4 (0.46x) | 27.2 (1.00x) | 15.4 (0.57x) | **tscore** |

### mandelbrot

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 25.6 (0.68x) | 37.4 (1.00x) | 32.5 (0.87x) | **tscore** |
| 2 | 13.0 (0.51x) | 25.5 (1.00x) | 21.0 (0.82x) | **tscore** |
| 4 | 7.0 (0.29x) | 24.0 (1.00x) | 19.3 (0.81x) | **tscore** |
| 8 | 4.1 (0.17x) | 23.9 (1.00x) | 17.5 (0.73x) | **tscore** |

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
| tscore workers:1 | 203,652 | 1.0 | 207,989 **best/core** |
| bun x1 | 180,070 | 1.0 | 176,846 |
| bun x8 reusePort | 179,973 | 1.0 | 175,002 |
| node x1 | 134,621 | 1.0 | 133,740 |
| tscore workers:8 | 211,905 | 2.0 | 106,466 |
| node cluster x8 | 178,754 | 3.8 | 46,630 |

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
