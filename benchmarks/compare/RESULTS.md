# Engine comparison: tscore vs node (V8) vs bun (JSC)

- date: 2026-09-06 15:52
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
| startup (hello.ts) | 2.4 (0.04x) | 55.9 (1.00x) | 6.7 (0.12x) | **tscore** |
| parse+compile (~50k LOC) | 11.5 (0.08x) | 150.4 (1.00x) | 17.6 (0.12x) | **tscore** |

## Steady-state (shared sources, in-program TIME_MS)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| objects | 107.9 (3.01x) | 35.9 (1.00x) | 35.5 (0.99x) | **bun** |
| closures | 154.8 (3.01x) | 51.4 (1.00x) | 70.3 (1.37x) | **node** |
| alloc | 126.3 (1.95x) | 64.8 (1.00x) | 69.9 (1.08x) | **node** |
| gc_churn | 203.5 (2.66x) | 76.5 (1.00x) | 59.5 (0.78x) | **bun** |
| promises | 87.4 (0.91x) | 95.7 (1.00x) | 89.5 (0.94x) | **tscore** |

## Async (per-engine variants, same algorithm)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| timer storm (2000x sleep 1ms) | 12.0 (0.88x) | 13.7 (1.00x) | 12.2 (0.89x) | **tscore** |
| channel 100k msgs (vs worker postMessage) | 7.6 (0.15x) | 51.7 (1.00x) | 42.3 (0.82x) | **tscore** |

## Long-running (30s sustained mixed compute+alloc, single run)

| metric | tscore | node | bun | winner |
|---|---|---|---|---|
| throughput (ops/sec) | 476563 (0.73x) | 654831 (1.00x) | 502389 (0.77x) | **node** |
| stability (last/first decile) | 1.011 | 1.012 | 0.956 | |

## Multicore (tscore parallel.map vs node/bun worker_threads)

### primes

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 77.2 (0.83x) | 93.0 (1.00x) | 51.7 (0.56x) | **bun** |
| 2 | 40.6 (0.66x) | 61.1 (1.00x) | 35.6 (0.58x) | **bun** |
| 4 | 20.9 (0.47x) | 44.2 (1.00x) | 24.0 (0.54x) | **tscore** |
| 8 | 11.0 (0.32x) | 34.5 (1.00x) | 18.6 (0.54x) | **tscore** |

### mandelbrot

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 37.2 (0.75x) | 49.9 (1.00x) | 45.3 (0.91x) | **tscore** |
| 2 | 19.0 (0.55x) | 34.7 (1.00x) | 29.8 (0.86x) | **tscore** |
| 4 | 10.0 (0.31x) | 32.3 (1.00x) | 27.0 (0.84x) | **tscore** |
| 8 | 5.1 (0.17x) | 29.7 (1.00x) | 22.2 (0.75x) | **tscore** |

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
| tscore workers:1 | 137,783 | 1.0 | 138,263 **best/core** |
| bun x1 | 125,042 | 1.0 | 121,452 |
| bun x8 reusePort | 125,028 | 1.0 | 120,688 |
| node x1 | 94,738 | 1.0 | 92,778 |
| tscore workers:8 | 158,466 | 2.0 | 78,423 |
| node cluster x8 | 136,847 | 3.8 | 36,245 |

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
