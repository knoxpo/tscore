# Engine comparison: tscore vs node (V8) vs bun (JSC)

- date: 2026-09-03 03:54
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
| startup (hello.ts) | 1.7 (0.05x) | 36.1 (1.00x) | 4.3 (0.12x) | **tscore** |
| parse+compile (~50k LOC) | 8.6 (0.09x) | 96.6 (1.00x) | 11.3 (0.12x) | **tscore** |

## Steady-state (shared sources, in-program TIME_MS)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| objects | 115.9 (4.98x) | 23.3 (1.00x) | 24.3 (1.04x) | **node** |
| closures | 103.4 (3.12x) | 33.1 (1.00x) | 47.9 (1.45x) | **node** |
| alloc | 154.2 (3.64x) | 42.4 (1.00x) | 48.6 (1.15x) | **node** |
| gc_churn | 319.5 (6.32x) | 50.5 (1.00x) | 41.5 (0.82x) | **bun** |
| promises | 61.9 (0.99x) | 62.2 (1.00x) | 57.9 (0.93x) | **bun** |

## Async (per-engine variants, same algorithm)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| timer storm (2000x sleep 1ms) | 11.6 (0.93x) | 12.4 (1.00x) | 12.1 (0.97x) | **tscore** |
| channel 100k msgs (vs worker postMessage) | 5.0 (0.15x) | 34.2 (1.00x) | 28.4 (0.83x) | **tscore** |

## Long-running (30s sustained mixed compute+alloc, single run)

| metric | tscore | node | bun | winner |
|---|---|---|---|---|
| throughput (ops/sec) | 602035 (0.59x) | 1028726 (1.00x) | 768439 (0.75x) | **node** |
| stability (last/first decile) | 0.992 | 1.014 | 0.946 | |

## Multicore (tscore parallel.map vs node/bun worker_threads)

### primes

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 73.3 (1.07x) | 68.3 (1.00x) | 34.2 (0.50x) | **bun** |
| 2 | 39.5 (0.91x) | 43.2 (1.00x) | 23.7 (0.55x) | **bun** |
| 4 | 20.2 (0.67x) | 30.1 (1.00x) | 16.1 (0.54x) | **bun** |
| 8 | 11.8 (0.46x) | 25.4 (1.00x) | 14.6 (0.57x) | **tscore** |

### mandelbrot

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 24.5 (0.76x) | 32.2 (1.00x) | 29.4 (0.91x) | **tscore** |
| 2 | 12.7 (0.54x) | 23.4 (1.00x) | 20.0 (0.85x) | **tscore** |
| 4 | 6.8 (0.31x) | 22.0 (1.00x) | 18.3 (0.83x) | **tscore** |
| 8 | 3.9 (0.17x) | 22.5 (1.00x) | 16.3 (0.73x) | **tscore** |

## Scoreboard

| engine | wins |
|---|---|
| tscore | 9 |
| node | 4 |
| bun | 5 |

Win = fastest median (highest throughput for long-running) on that row.


## HTTP hello (wrk -t2 -c32 -d5s)

| engine | req/s | cores | req/s per core |
|---|---|---|---|
| tscore workers:1 | 217,675 | 1.1 | 197,833 **best/core** |
| bun x1 | 194,644 | 1.0 | 187,563 |
| bun x8 reusePort | 195,679 | 1.0 | 186,674 |
| node x1 | 145,597 | 1.0 | 142,819 |
| tscore workers:8 | 213,684 | 1.9 | 111,472 |
| node cluster x8 | 184,915 | 4.0 | 46,565 |

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
