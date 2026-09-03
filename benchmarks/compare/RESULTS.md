# Engine comparison: tscore vs node (V8) vs bun (JSC)

- date: 2026-09-03 19:02
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
| startup (hello.ts) | 1.6 (0.04x) | 35.8 (1.00x) | 4.2 (0.12x) | **tscore** |
| parse+compile (~50k LOC) | 7.2 (0.07x) | 96.7 (1.00x) | 11.0 (0.11x) | **tscore** |

## Steady-state (shared sources, in-program TIME_MS)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| objects | 116.7 (4.99x) | 23.4 (1.00x) | 23.3 (1.00x) | **bun** |
| closures | 102.2 (3.04x) | 33.7 (1.00x) | 47.8 (1.42x) | **node** |
| alloc | 132.3 (3.07x) | 43.1 (1.00x) | 47.3 (1.10x) | **node** |
| gc_churn | 190.4 (3.69x) | 51.6 (1.00x) | 41.2 (0.80x) | **bun** |
| promises | 63.3 (1.01x) | 62.7 (1.00x) | 57.2 (0.91x) | **bun** |

## Async (per-engine variants, same algorithm)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| timer storm (2000x sleep 1ms) | 11.7 (0.94x) | 12.4 (1.00x) | 12.2 (0.98x) | **tscore** |
| channel 100k msgs (vs worker postMessage) | 5.1 (0.15x) | 33.6 (1.00x) | 28.8 (0.85x) | **tscore** |

## Long-running (30s sustained mixed compute+alloc, single run)

| metric | tscore | node | bun | winner |
|---|---|---|---|---|
| throughput (ops/sec) | 592484 (0.58x) | 1021216 (1.00x) | 804345 (0.79x) | **node** |
| stability (last/first decile) | 1.004 | 1.001 | 1.019 | |

## Multicore (tscore parallel.map vs node/bun worker_threads)

### primes

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 75.0 (1.30x) | 57.7 (1.00x) | 34.2 (0.59x) | **bun** |
| 2 | 39.7 (0.97x) | 40.9 (1.00x) | 23.5 (0.57x) | **bun** |
| 4 | 20.2 (0.70x) | 29.0 (1.00x) | 16.9 (0.58x) | **bun** |
| 8 | 12.0 (0.47x) | 25.3 (1.00x) | 14.6 (0.58x) | **tscore** |

### mandelbrot

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 24.7 (0.75x) | 33.0 (1.00x) | 30.0 (0.91x) | **tscore** |
| 2 | 12.7 (0.54x) | 23.6 (1.00x) | 20.0 (0.85x) | **tscore** |
| 4 | 6.8 (0.30x) | 22.3 (1.00x) | 18.2 (0.81x) | **tscore** |
| 8 | 3.9 (0.18x) | 21.3 (1.00x) | 16.4 (0.77x) | **tscore** |

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
| tscore workers:1 | 218,198 | 1.1 | 197,923 **best/core** |
| bun x1 | 195,762 | 1.0 | 188,233 |
| bun x8 reusePort | 195,245 | 1.0 | 186,646 |
| node x1 | 146,410 | 1.0 | 144,031 |
| tscore workers:8 | 216,256 | 2.0 | 107,093 |
| node cluster x8 | 185,106 | 3.9 | 47,406 |

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
