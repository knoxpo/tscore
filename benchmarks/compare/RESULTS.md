# Engine comparison: tscore vs node (V8) vs bun (JSC)

- date: 2026-09-03 04:46
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
| startup (hello.ts) | 1.7 (0.04x) | 38.2 (1.00x) | 4.8 (0.13x) | **tscore** |
| parse+compile (~50k LOC) | 9.5 (0.09x) | 106.0 (1.00x) | 12.9 (0.12x) | **tscore** |

## Steady-state (shared sources, in-program TIME_MS)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| objects | 124.7 (4.94x) | 25.3 (1.00x) | 24.7 (0.98x) | **bun** |
| closures | 109.4 (2.98x) | 36.7 (1.00x) | 53.1 (1.45x) | **node** |
| alloc | 162.0 (3.53x) | 45.9 (1.00x) | 52.3 (1.14x) | **node** |
| gc_churn | 206.6 (3.78x) | 54.6 (1.00x) | 44.1 (0.81x) | **bun** |
| promises | 67.6 (1.00x) | 67.4 (1.00x) | 62.9 (0.93x) | **bun** |

## Async (per-engine variants, same algorithm)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| timer storm (2000x sleep 1ms) | 11.4 (0.89x) | 12.9 (1.00x) | 11.7 (0.91x) | **tscore** |
| channel 100k msgs (vs worker postMessage) | 5.4 (0.15x) | 35.0 (1.00x) | 29.9 (0.85x) | **tscore** |

## Long-running (30s sustained mixed compute+alloc, single run)

| metric | tscore | node | bun | winner |
|---|---|---|---|---|
| throughput (ops/sec) | 559250 (0.60x) | 926731 (1.00x) | 689604 (0.74x) | **node** |
| stability (last/first decile) | 0.996 | 1.016 | 0.937 | |

## Multicore (tscore parallel.map vs node/bun worker_threads)

### primes

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 80.5 (1.23x) | 65.5 (1.00x) | 36.5 (0.56x) | **bun** |
| 2 | 41.1 (0.95x) | 43.0 (1.00x) | 24.4 (0.57x) | **bun** |
| 4 | 20.7 (0.68x) | 30.4 (1.00x) | 16.5 (0.54x) | **bun** |
| 8 | 11.9 (0.47x) | 25.3 (1.00x) | 14.0 (0.55x) | **tscore** |

### mandelbrot

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 26.4 (0.74x) | 35.7 (1.00x) | 32.6 (0.91x) | **tscore** |
| 2 | 13.2 (0.54x) | 24.6 (1.00x) | 20.9 (0.85x) | **tscore** |
| 4 | 6.9 (0.30x) | 23.2 (1.00x) | 18.7 (0.81x) | **tscore** |
| 8 | 3.9 (0.17x) | 22.5 (1.00x) | 16.2 (0.72x) | **tscore** |

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
| tscore workers:1 | 212,981 | 1.1 | 197,413 **best/core** |
| bun x1 | 189,413 | 1.0 | 182,787 |
| bun x8 reusePort | 189,837 | 1.0 | 181,477 |
| node x1 | 141,869 | 1.0 | 139,547 |
| tscore workers:8 | 230,399 | 1.8 | 130,719 |
| node cluster x8 | 171,379 | 4.0 | 43,117 |

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
