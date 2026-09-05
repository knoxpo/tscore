# Engine comparison: tscore vs node (V8) vs bun (JSC)

- date: 2026-09-06 01:36
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
| startup (hello.ts) | 2.1 (0.05x) | 39.9 (1.00x) | 5.7 (0.14x) | **tscore** |
| parse+compile (~50k LOC) | 8.3 (0.08x) | 104.5 (1.00x) | 13.1 (0.13x) | **tscore** |

## Steady-state (shared sources, in-program TIME_MS)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| objects | 105.7 (3.91x) | 27.0 (1.00x) | 24.2 (0.89x) | **bun** |
| closures | 107.6 (2.77x) | 38.8 (1.00x) | 51.0 (1.31x) | **node** |
| alloc | 118.0 (2.66x) | 44.4 (1.00x) | 48.9 (1.10x) | **node** |
| gc_churn | 210.5 (3.49x) | 60.3 (1.00x) | 48.4 (0.80x) | **bun** |
| promises | 65.0 (0.96x) | 67.6 (1.00x) | 63.5 (0.94x) | **bun** |

## Async (per-engine variants, same algorithm)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| timer storm (2000x sleep 1ms) | 11.5 (0.87x) | 13.1 (1.00x) | 11.8 (0.90x) | **tscore** |
| channel 100k msgs (vs worker postMessage) | 5.2 (0.14x) | 37.1 (1.00x) | 30.3 (0.81x) | **tscore** |

## Long-running (30s sustained mixed compute+alloc, single run)

| metric | tscore | node | bun | winner |
|---|---|---|---|---|
| throughput (ops/sec) | 731230 (0.74x) | 986848 (1.00x) | 725011 (0.73x) | **node** |
| stability (last/first decile) | 1.013 | 1.013 | 0.889 | |

## Multicore (tscore parallel.map vs node/bun worker_threads)

### primes

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 77.3 (1.21x) | 64.0 (1.00x) | 35.9 (0.56x) | **bun** |
| 2 | 40.7 (0.88x) | 46.0 (1.00x) | 24.8 (0.54x) | **bun** |
| 4 | 21.1 (0.65x) | 32.5 (1.00x) | 17.8 (0.55x) | **bun** |
| 8 | 11.8 (0.44x) | 26.7 (1.00x) | 15.2 (0.57x) | **tscore** |

### mandelbrot

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 25.2 (0.72x) | 35.1 (1.00x) | 31.3 (0.89x) | **tscore** |
| 2 | 12.9 (0.52x) | 24.7 (1.00x) | 20.6 (0.84x) | **tscore** |
| 4 | 6.9 (0.29x) | 23.9 (1.00x) | 19.0 (0.79x) | **tscore** |
| 8 | 3.9 (0.16x) | 23.9 (1.00x) | 17.3 (0.72x) | **tscore** |

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
| tscore workers:1 | 183,337 | 1.0 | 190,665 **best/core** |
| bun x8 reusePort | 169,397 | 1.0 | 164,494 |
| bun x1 | 164,574 | 1.0 | 162,209 |
| tscore workers:8 | 227,148 | 1.9 | 121,680 |
| node x1 | 118,430 | 1.0 | 119,259 |
| node cluster x8 | 161,812 | 3.8 | 42,303 |

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
