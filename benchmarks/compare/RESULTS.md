# Engine comparison: tscore vs node (V8) vs bun (JSC)

- date: 2026-09-06 13:55
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
| startup (hello.ts) | 2.5 (0.05x) | 55.4 (1.00x) | 7.1 (0.13x) | **tscore** |
| parse+compile (~50k LOC) | 11.6 (0.08x) | 150.7 (1.00x) | 17.5 (0.12x) | **tscore** |

## Steady-state (shared sources, in-program TIME_MS)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| objects | 118.6 (3.29x) | 36.1 (1.00x) | 35.0 (0.97x) | **bun** |
| closures | 155.3 (2.94x) | 52.7 (1.00x) | 72.1 (1.37x) | **node** |
| alloc | 128.4 (2.01x) | 63.9 (1.00x) | 70.6 (1.11x) | **node** |
| gc_churn | 237.9 (3.16x) | 75.4 (1.00x) | 61.9 (0.82x) | **bun** |
| promises | 85.0 (0.89x) | 95.9 (1.00x) | 87.2 (0.91x) | **tscore** |

## Async (per-engine variants, same algorithm)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| timer storm (2000x sleep 1ms) | 12.2 (0.96x) | 12.7 (1.00x) | 12.1 (0.95x) | **bun** |
| channel 100k msgs (vs worker postMessage) | 7.6 (0.15x) | 51.6 (1.00x) | 43.2 (0.84x) | **tscore** |

## Long-running (30s sustained mixed compute+alloc, single run)

| metric | tscore | node | bun | winner |
|---|---|---|---|---|
| throughput (ops/sec) | 692062 (0.68x) | 1013612 (1.00x) | 763082 (0.75x) | **node** |
| stability (last/first decile) | 1.565 | 1.049 | 0.995 | |

## Multicore (tscore parallel.map vs node/bun worker_threads)

### primes

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 53.3 (0.90x) | 59.1 (1.00x) | 33.8 (0.57x) | **bun** |
| 2 | 27.2 (0.62x) | 43.9 (1.00x) | 23.7 (0.54x) | **bun** |
| 4 | 13.9 (0.46x) | 30.3 (1.00x) | 16.1 (0.53x) | **tscore** |
| 8 | 8.3 (0.32x) | 26.0 (1.00x) | 13.9 (0.54x) | **tscore** |

### mandelbrot

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 26.1 (0.78x) | 33.4 (1.00x) | 29.5 (0.88x) | **tscore** |
| 2 | 12.8 (0.54x) | 23.8 (1.00x) | 20.2 (0.85x) | **tscore** |
| 4 | 6.9 (0.23x) | 29.9 (1.00x) | 18.3 (0.61x) | **tscore** |
| 8 | 3.9 (0.17x) | 23.5 (1.00x) | 16.7 (0.71x) | **tscore** |

## Scoreboard

| engine | wins |
|---|---|
| tscore | 10 |
| node | 3 |
| bun | 5 |

Win = fastest median (highest throughput for long-running) on that row.


## HTTP hello (wrk -t2 -c32 -d5s)

| engine | req/s | cores | req/s per core |
|---|---|---|---|
| tscore workers:1 | 206,283 | 1.0 | 207,314 **best/core** |
| bun x8 reusePort | 180,147 | 1.0 | 174,792 |
| bun x1 | 178,766 | 1.0 | 174,530 |
| node x1 | 137,160 | 1.0 | 135,441 |
| tscore workers:8 | 245,097 | 1.9 | 129,124 |
| node cluster x8 | 172,601 | 4.0 | 43,358 |

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
