# Engine comparison: tscore vs node (V8) vs bun (JSC)

- date: 2026-09-06 16:54
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
| startup (hello.ts) | 2.5 (0.05x) | 54.4 (1.00x) | 6.1 (0.11x) | **tscore** |
| parse+compile (~50k LOC) | 13.4 (0.09x) | 145.0 (1.00x) | 16.7 (0.12x) | **tscore** |

## Steady-state (shared sources, in-program TIME_MS)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| objects | 108.3 (3.07x) | 35.2 (1.00x) | 34.6 (0.98x) | **bun** |
| closures | 151.9 (2.97x) | 51.2 (1.00x) | 69.6 (1.36x) | **node** |
| alloc | 127.3 (1.94x) | 65.5 (1.00x) | 71.0 (1.08x) | **node** |
| gc_churn | 174.0 (2.34x) | 74.2 (1.00x) | 59.0 (0.80x) | **bun** |
| promises | 88.9 (0.95x) | 93.8 (1.00x) | 87.1 (0.93x) | **bun** |

## Async (per-engine variants, same algorithm)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| timer storm (2000x sleep 1ms) | 11.9 (0.89x) | 13.4 (1.00x) | 12.2 (0.91x) | **tscore** |
| channel 100k msgs (vs worker postMessage) | 7.7 (0.16x) | 48.3 (1.00x) | 42.6 (0.88x) | **tscore** |

## Long-running (30s sustained mixed compute+alloc, single run)

| metric | tscore | node | bun | winner |
|---|---|---|---|---|
| throughput (ops/sec) | 501838 (0.77x) | 654152 (1.00x) | 504094 (0.77x) | **node** |
| stability (last/first decile) | 0.999 | 1.019 | 0.97 | |

## Multicore (tscore parallel.map vs node/bun worker_threads)

### primes

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 78.7 (0.86x) | 92.0 (1.00x) | 53.1 (0.58x) | **bun** |
| 2 | 41.4 (0.65x) | 64.0 (1.00x) | 35.3 (0.55x) | **bun** |
| 4 | 20.9 (0.47x) | 44.4 (1.00x) | 24.0 (0.54x) | **tscore** |
| 8 | 10.9 (0.32x) | 33.9 (1.00x) | 18.8 (0.55x) | **tscore** |

### mandelbrot

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 37.4 (0.75x) | 50.0 (1.00x) | 45.7 (0.91x) | **tscore** |
| 2 | 19.0 (0.55x) | 34.9 (1.00x) | 30.0 (0.86x) | **tscore** |
| 4 | 10.0 (0.31x) | 32.5 (1.00x) | 27.0 (0.83x) | **tscore** |
| 8 | 5.2 (0.17x) | 30.5 (1.00x) | 22.2 (0.73x) | **tscore** |

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
| tscore workers:1 | 148,147 | 1.0 | 148,639 **best/core** |
| bun x1 | 129,343 | 1.0 | 125,348 |
| bun x8 reusePort | 127,466 | 1.0 | 122,577 |
| tscore workers:8 | 161,144 | 1.7 | 95,812 |
| node x1 | 95,231 | 1.0 | 93,016 |
| node cluster x8 | 141,216 | 3.6 | 38,805 |

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
