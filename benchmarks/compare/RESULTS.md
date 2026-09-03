# Engine comparison: tscore vs node (V8) vs bun (JSC)

- date: 2026-09-03 19:26
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
| startup (hello.ts) | 1.6 (0.04x) | 36.6 (1.00x) | 4.3 (0.12x) | **tscore** |
| parse+compile (~50k LOC) | 7.4 (0.08x) | 97.4 (1.00x) | 11.2 (0.11x) | **tscore** |

## Steady-state (shared sources, in-program TIME_MS)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| objects | 111.0 (4.65x) | 23.9 (1.00x) | 23.1 (0.97x) | **bun** |
| closures | 103.3 (3.07x) | 33.6 (1.00x) | 49.4 (1.47x) | **node** |
| alloc | 132.5 (3.10x) | 42.7 (1.00x) | 48.6 (1.14x) | **node** |
| gc_churn | 192.3 (3.83x) | 50.2 (1.00x) | 42.0 (0.84x) | **bun** |
| promises | 62.4 (0.99x) | 63.0 (1.00x) | 58.1 (0.92x) | **bun** |

## Async (per-engine variants, same algorithm)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| timer storm (2000x sleep 1ms) | 11.7 (0.88x) | 13.3 (1.00x) | 12.0 (0.91x) | **tscore** |
| channel 100k msgs (vs worker postMessage) | 5.0 (0.15x) | 33.5 (1.00x) | 28.5 (0.85x) | **tscore** |

## Long-running (30s sustained mixed compute+alloc, single run)

| metric | tscore | node | bun | winner |
|---|---|---|---|---|
| throughput (ops/sec) | 594048 (0.58x) | 1017643 (1.00x) | 781026 (0.77x) | **node** |
| stability (last/first decile) | 0.993 | 1.027 | 1.042 | |

## Multicore (tscore parallel.map vs node/bun worker_threads)

### primes

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 74.5 (1.25x) | 59.8 (1.00x) | 34.8 (0.58x) | **bun** |
| 2 | 39.5 (0.93x) | 42.7 (1.00x) | 24.3 (0.57x) | **bun** |
| 4 | 20.3 (0.66x) | 30.9 (1.00x) | 16.4 (0.53x) | **bun** |
| 8 | 11.9 (0.47x) | 25.4 (1.00x) | 14.5 (0.57x) | **tscore** |

### mandelbrot

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 24.7 (0.74x) | 33.2 (1.00x) | 29.8 (0.90x) | **tscore** |
| 2 | 12.7 (0.51x) | 24.9 (1.00x) | 20.2 (0.81x) | **tscore** |
| 4 | 6.8 (0.30x) | 22.5 (1.00x) | 18.2 (0.81x) | **tscore** |
| 8 | 4.0 (0.18x) | 21.8 (1.00x) | 16.2 (0.75x) | **tscore** |

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
| tscore workers:1 | 197,311 | 1.1 | 185,535 **best/core** |
| bun x8 reusePort | 193,580 | 1.0 | 185,045 |
| bun x1 | 191,399 | 1.0 | 184,725 |
| node x1 | 133,945 | 1.0 | 134,487 |
| tscore workers:8 | 223,932 | 2.0 | 114,665 |
| node cluster x8 | 180,568 | 3.9 | 45,803 |

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
