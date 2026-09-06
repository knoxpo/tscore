# Engine comparison: tscore vs node (V8) vs bun (JSC)

- date: 2026-09-06 15:20
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
| startup (hello.ts) | 2.3 (0.04x) | 53.9 (1.00x) | 6.2 (0.12x) | **tscore** |
| parse+compile (~50k LOC) | 11.6 (0.08x) | 145.1 (1.00x) | 16.4 (0.11x) | **tscore** |

## Steady-state (shared sources, in-program TIME_MS)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| objects | 107.2 (3.04x) | 35.3 (1.00x) | 34.5 (0.98x) | **bun** |
| closures | 152.4 (2.96x) | 51.5 (1.00x) | 69.7 (1.35x) | **node** |
| alloc | 127.5 (1.97x) | 64.7 (1.00x) | 69.3 (1.07x) | **node** |
| gc_churn | 183.9 (2.55x) | 72.2 (1.00x) | 59.7 (0.83x) | **bun** |
| promises | 85.1 (0.90x) | 95.0 (1.00x) | 88.2 (0.93x) | **tscore** |

## Async (per-engine variants, same algorithm)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| timer storm (2000x sleep 1ms) | 12.1 (0.87x) | 13.9 (1.00x) | 12.2 (0.88x) | **tscore** |
| channel 100k msgs (vs worker postMessage) | 7.7 (0.15x) | 50.3 (1.00x) | 42.0 (0.83x) | **tscore** |

## Long-running (30s sustained mixed compute+alloc, single run)

| metric | tscore | node | bun | winner |
|---|---|---|---|---|
| throughput (ops/sec) | 484804 (0.74x) | 654009 (1.00x) | 535370 (0.82x) | **node** |
| stability (last/first decile) | 1.003 | 1.009 | 1.003 | |

## Multicore (tscore parallel.map vs node/bun worker_threads)

### primes

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 77.9 (0.83x) | 93.3 (1.00x) | 51.3 (0.55x) | **bun** |
| 2 | 40.9 (0.63x) | 64.6 (1.00x) | 34.9 (0.54x) | **bun** |
| 4 | 21.3 (0.48x) | 44.2 (1.00x) | 23.9 (0.54x) | **tscore** |
| 8 | 11.1 (0.32x) | 34.3 (1.00x) | 21.6 (0.63x) | **tscore** |

### mandelbrot

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 37.2 (0.75x) | 49.6 (1.00x) | 45.2 (0.91x) | **tscore** |
| 2 | 19.0 (0.55x) | 34.6 (1.00x) | 29.8 (0.86x) | **tscore** |
| 4 | 10.1 (0.31x) | 32.3 (1.00x) | 27.0 (0.84x) | **tscore** |
| 8 | 5.2 (0.17x) | 30.3 (1.00x) | 21.8 (0.72x) | **tscore** |

## Scoreboard

| engine | wins |
|---|---|
| tscore | 11 |
| node | 3 |
| bun | 4 |

Win = fastest median (highest throughput for long-running) on that row.


## HTTP hello (wrk -t2 -c32 -d5s)

| engine | req/s | cores | req/s per core |
|---|---|---|---|
| tscore workers:1 | 148,040 | 1.0 | 148,554 **best/core** |
| bun x1 | 131,684 | 1.0 | 127,359 |
| bun x8 reusePort | 130,423 | 1.0 | 125,675 |
| node x1 | 98,004 | 1.0 | 95,797 |
| tscore workers:8 | 155,033 | 1.8 | 86,182 |
| node cluster x8 | 139,653 | 3.6 | 38,466 |

Total throughput is capped by this machine's loopback stack, not
by any engine: four independent servers with four independent
clients total the same as one. Compare the per-core column.
## Runtime surface

| category | status |
|---|---|
| modules | SHIPPED — full ESM: static + dynamic import, cycles (TDZ), TLA; bare specifiers via tscore.json and node_modules (package.json exports/main); .js/.mjs/.cjs specifiers resolve to TS sources; import.meta.url/filename/dirname |
| networking | SHIPPED — runtime.net TCP (kqueue reactor, connect) + runtime.http |
| async file I/O | SHIPPED — runtime.fs (promise-native, dedicated I/O pool) + bytes |
| http throughput | see the HTTP section above (benchmarks/compare/http_bench.py) |
| async I/O | proxied by timer-storm + channel benchmarks above |
