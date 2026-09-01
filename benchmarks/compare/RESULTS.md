# Engine comparison: tscore vs node (V8) vs bun (JSC)

- date: 2026-09-02 01:52
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
| startup (hello.ts) | 1.6 (0.04x) | 37.6 (1.00x) | 4.4 (0.12x) | **tscore** |
| parse+compile (~50k LOC) | 9.0 (0.09x) | 102.0 (1.00x) | 11.7 (0.11x) | **tscore** |

## Steady-state (shared sources, in-program TIME_MS)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| objects | 143.7 (5.94x) | 24.2 (1.00x) | 23.4 (0.97x) | **bun** |
| closures | 105.4 (3.06x) | 34.5 (1.00x) | 47.6 (1.38x) | **node** |
| alloc | 157.6 (3.52x) | 44.8 (1.00x) | 47.8 (1.07x) | **node** |
| gc_churn | 333.0 (6.13x) | 54.3 (1.00x) | 41.5 (0.76x) | **bun** |
| promises | 65.0 (1.02x) | 63.5 (1.00x) | 57.5 (0.91x) | **bun** |

## Async (per-engine variants, same algorithm)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| timer storm (2000x sleep 1ms) | 12.5 (0.94x) | 13.3 (1.00x) | 13.2 (1.00x) | **tscore** |
| channel 100k msgs (vs worker postMessage) | 5.2 (0.15x) | 35.2 (1.00x) | 29.2 (0.83x) | **tscore** |

## Long-running (30s sustained mixed compute+alloc, single run)

| metric | tscore | node | bun | winner |
|---|---|---|---|---|
| throughput (ops/sec) | 429399 (0.43x) | 987600 (1.00x) | 769919 (0.78x) | **node** |
| stability (last/first decile) | 0.997 | 1.014 | 1.036 | |

## Multicore (tscore parallel.map vs node/bun worker_threads)

### primes

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 77.9 (1.27x) | 61.5 (1.00x) | 35.0 (0.57x) | **bun** |
| 2 | 39.7 (0.92x) | 43.2 (1.00x) | 23.8 (0.55x) | **bun** |
| 4 | 20.3 (0.66x) | 30.8 (1.00x) | 16.8 (0.54x) | **bun** |
| 8 | 11.8 (0.47x) | 25.2 (1.00x) | 14.2 (0.56x) | **tscore** |

### mandelbrot

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 25.3 (0.73x) | 34.6 (1.00x) | 30.9 (0.89x) | **tscore** |
| 2 | 12.7 (0.52x) | 24.2 (1.00x) | 20.1 (0.83x) | **tscore** |
| 4 | 6.8 (0.30x) | 23.0 (1.00x) | 18.6 (0.81x) | **tscore** |
| 8 | 3.9 (0.18x) | 21.9 (1.00x) | 17.1 (0.78x) | **tscore** |

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
| bun x1 | 194,485 | 1.0 | 188,775 **best/core** |
| bun x8 reusePort | 195,203 | 1.0 | 188,183 |
| tscore workers:1 | 198,233 | 1.1 | 184,674 |
| node x1 | 137,861 | 1.0 | 136,168 |
| tscore workers:8 | 242,527 | 1.9 | 126,712 |
| node cluster x8 | 187,235 | 3.7 | 49,951 |

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
