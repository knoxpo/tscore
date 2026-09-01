# Engine comparison: tscore vs node (V8) vs bun (JSC)

- date: 2026-09-02 04:55
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
| startup (hello.ts) | 1.6 (0.04x) | 37.0 (1.00x) | 4.2 (0.11x) | **tscore** |
| parse+compile (~50k LOC) | 8.8 (0.09x) | 98.0 (1.00x) | 11.2 (0.11x) | **tscore** |

## Steady-state (shared sources, in-program TIME_MS)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| objects | 139.3 (5.84x) | 23.9 (1.00x) | 24.0 (1.01x) | **node** |
| closures | 104.4 (3.08x) | 33.9 (1.00x) | 47.3 (1.40x) | **node** |
| alloc | 154.4 (3.59x) | 43.0 (1.00x) | 46.2 (1.07x) | **node** |
| gc_churn | 325.6 (6.18x) | 52.7 (1.00x) | 42.5 (0.81x) | **bun** |
| promises | 67.9 (1.02x) | 66.7 (1.00x) | 75.6 (1.13x) | **node** |

## Async (per-engine variants, same algorithm)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| timer storm (2000x sleep 1ms) | 11.7 (0.89x) | 13.2 (1.00x) | 11.9 (0.90x) | **tscore** |
| channel 100k msgs (vs worker postMessage) | 5.1 (0.14x) | 35.9 (1.00x) | 29.5 (0.82x) | **tscore** |

## Long-running (30s sustained mixed compute+alloc, single run)

| metric | tscore | node | bun | winner |
|---|---|---|---|---|
| throughput (ops/sec) | 583734 (0.62x) | 945044 (1.00x) | 816953 (0.86x) | **node** |
| stability (last/first decile) | 0.961 | 1.107 | 0.998 | |

## Multicore (tscore parallel.map vs node/bun worker_threads)

### primes

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 75.0 (1.25x) | 60.0 (1.00x) | 34.9 (0.58x) | **bun** |
| 2 | 39.7 (0.92x) | 43.3 (1.00x) | 23.9 (0.55x) | **bun** |
| 4 | 20.4 (0.67x) | 30.3 (1.00x) | 16.4 (0.54x) | **bun** |
| 8 | 11.9 (0.47x) | 25.1 (1.00x) | 13.9 (0.55x) | **tscore** |

### mandelbrot

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 24.7 (0.74x) | 33.4 (1.00x) | 29.9 (0.89x) | **tscore** |
| 2 | 12.7 (0.51x) | 25.1 (1.00x) | 20.2 (0.80x) | **tscore** |
| 4 | 6.8 (0.30x) | 22.4 (1.00x) | 18.4 (0.82x) | **tscore** |
| 8 | 3.9 (0.18x) | 22.2 (1.00x) | 16.0 (0.72x) | **tscore** |

## Scoreboard

| engine | wins |
|---|---|
| tscore | 9 |
| node | 5 |
| bun | 4 |

Win = fastest median (highest throughput for long-running) on that row.


## HTTP hello (wrk -t2 -c32 -d5s)

| engine | req/s | cores | req/s per core |
|---|---|---|---|
| tscore workers:1 | 212,001 | 1.1 | 196,437 **best/core** |
| bun x1 | 191,199 | 1.0 | 185,601 |
| bun x8 reusePort | 190,823 | 1.0 | 183,771 |
| node x1 | 143,367 | 1.0 | 141,022 |
| tscore workers:8 | 242,120 | 1.7 | 140,271 |
| node cluster x8 | 190,317 | 3.7 | 51,133 |

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
