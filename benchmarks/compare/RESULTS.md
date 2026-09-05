# Engine comparison: tscore vs node (V8) vs bun (JSC)

- date: 2026-09-06 02:51
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
| startup (hello.ts) | 2.5 (0.06x) | 41.1 (1.00x) | 5.5 (0.13x) | **tscore** |
| parse+compile (~50k LOC) | 8.3 (0.08x) | 105.3 (1.00x) | 13.1 (0.12x) | **tscore** |

## Steady-state (shared sources, in-program TIME_MS)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| objects | 107.1 (4.23x) | 25.3 (1.00x) | 23.3 (0.92x) | **bun** |
| closures | 104.8 (2.82x) | 37.1 (1.00x) | 50.8 (1.37x) | **node** |
| alloc | 116.4 (2.63x) | 44.3 (1.00x) | 49.0 (1.11x) | **node** |
| gc_churn | 154.7 (2.72x) | 57.0 (1.00x) | 45.6 (0.80x) | **bun** |
| promises | 65.0 (0.99x) | 65.4 (1.00x) | 59.5 (0.91x) | **bun** |

## Async (per-engine variants, same algorithm)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| timer storm (2000x sleep 1ms) | 11.7 (0.87x) | 13.4 (1.00x) | 12.1 (0.90x) | **tscore** |
| channel 100k msgs (vs worker postMessage) | 5.2 (0.15x) | 35.2 (1.00x) | 29.5 (0.84x) | **tscore** |

## Long-running (30s sustained mixed compute+alloc, single run)

| metric | tscore | node | bun | winner |
|---|---|---|---|---|
| throughput (ops/sec) | 724949 (0.73x) | 994770 (1.00x) | 761237 (0.77x) | **node** |
| stability (last/first decile) | 0.999 | 1.019 | 1.11 | |

## Multicore (tscore parallel.map vs node/bun worker_threads)

### primes

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 77.6 (1.12x) | 69.5 (1.00x) | 35.9 (0.52x) | **bun** |
| 2 | 40.7 (0.90x) | 45.3 (1.00x) | 24.6 (0.54x) | **bun** |
| 4 | 20.8 (0.65x) | 31.9 (1.00x) | 17.0 (0.53x) | **bun** |
| 8 | 12.0 (0.46x) | 26.3 (1.00x) | 15.3 (0.58x) | **tscore** |

### mandelbrot

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 25.2 (0.70x) | 35.8 (1.00x) | 31.9 (0.89x) | **tscore** |
| 2 | 12.9 (0.50x) | 25.8 (1.00x) | 20.8 (0.81x) | **tscore** |
| 4 | 7.0 (0.29x) | 24.4 (1.00x) | 19.1 (0.78x) | **tscore** |
| 8 | 4.0 (0.17x) | 24.1 (1.00x) | 17.5 (0.73x) | **tscore** |

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
| tscore workers:1 | 191,723 | 1.0 | 197,011 **best/core** |
| bun x8 reusePort | 184,455 | 1.0 | 179,117 |
| bun x1 | 159,428 | 1.0 | 159,969 |
| node x1 | 135,184 | 1.0 | 134,854 |
| tscore workers:8 | 231,216 | 1.7 | 132,208 |
| node cluster x8 | 181,928 | 3.8 | 47,481 |

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
