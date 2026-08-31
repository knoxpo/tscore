# Engine comparison: tscore vs node (V8) vs bun (JSC)

- date: 2026-09-01 04:29
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
| startup (hello.ts) | 1.7 (0.04x) | 39.5 (1.00x) | 4.6 (0.12x) | **tscore** |
| parse+compile (~50k LOC) | 8.2 (0.08x) | 101.5 (1.00x) | 12.8 (0.13x) | **tscore** |

## Steady-state (shared sources, in-program TIME_MS)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| objects | 168.7 (6.93x) | 24.3 (1.00x) | 24.1 (0.99x) | **bun** |
| closures | 115.2 (3.26x) | 35.3 (1.00x) | 49.3 (1.40x) | **node** |
| alloc | 162.6 (3.69x) | 44.1 (1.00x) | 49.3 (1.12x) | **node** |
| gc_churn | 478.3 (8.30x) | 57.6 (1.00x) | 44.5 (0.77x) | **bun** |
| promises | 65.4 (1.01x) | 64.6 (1.00x) | 62.3 (0.96x) | **bun** |

## Async (per-engine variants, same algorithm)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| timer storm (2000x sleep 1ms) | 12.6 (0.97x) | 13.1 (1.00x) | 11.9 (0.91x) | **bun** |
| channel 100k msgs (vs worker postMessage) | 5.3 (0.14x) | 36.8 (1.00x) | 29.3 (0.80x) | **tscore** |

## Long-running (30s sustained mixed compute+alloc, single run)

| metric | tscore | node | bun | winner |
|---|---|---|---|---|
| throughput (ops/sec) | 405407 (0.40x) | 1002072 (1.00x) | 802823 (0.80x) | **node** |
| stability (last/first decile) | 0.949 | 1.005 | 1.014 | |

## Multicore (tscore parallel.map vs node/bun worker_threads)

### primes

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 77.2 (1.26x) | 61.4 (1.00x) | 35.3 (0.57x) | **bun** |
| 2 | 40.0 (0.91x) | 43.8 (1.00x) | 25.0 (0.57x) | **bun** |
| 4 | 20.8 (0.66x) | 31.4 (1.00x) | 16.6 (0.53x) | **bun** |
| 8 | 11.8 (0.44x) | 26.7 (1.00x) | 15.1 (0.57x) | **tscore** |

### mandelbrot

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 25.2 (0.72x) | 35.2 (1.00x) | 31.4 (0.89x) | **tscore** |
| 2 | 12.9 (0.51x) | 25.2 (1.00x) | 20.5 (0.81x) | **tscore** |
| 4 | 6.9 (0.30x) | 23.0 (1.00x) | 18.9 (0.82x) | **tscore** |
| 8 | 3.9 (0.17x) | 22.8 (1.00x) | 16.9 (0.74x) | **tscore** |

## Scoreboard

| engine | wins |
|---|---|
| tscore | 8 |
| node | 3 |
| bun | 7 |

Win = fastest median (highest throughput for long-running) on that row.


## HTTP hello (wrk -t2 -c32 -d5s)

| engine | req/s | cores | req/s per core |
|---|---|---|---|
| bun x1 | 188,052 | 1.0 | 183,530 **best/core** |
| tscore workers:1 | 195,289 | 1.1 | 181,969 |
| bun x8 reusePort | 186,787 | 1.0 | 181,952 |
| node x1 | 140,398 | 1.0 | 139,276 |
| tscore workers:8 | 230,137 | 2.0 | 116,650 |
| node cluster x8 | 179,269 | 3.7 | 47,852 |

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
