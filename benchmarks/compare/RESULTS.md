# Engine comparison: tscore vs node (V8) vs bun (JSC)

- date: 2026-09-06 21:56
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
| startup (hello.ts) | 2.6 (0.05x) | 57.4 (1.00x) | 6.9 (0.12x) | **tscore** |
| parse+compile (~50k LOC) | 13.8 (0.09x) | 151.1 (1.00x) | 17.3 (0.11x) | **tscore** |

## Steady-state (shared sources, in-program TIME_MS)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| objects | 102.8 (2.68x) | 38.3 (1.00x) | 36.3 (0.95x) | **bun** |
| closures | 151.8 (2.85x) | 53.2 (1.00x) | 71.7 (1.35x) | **node** |
| alloc | 109.0 (1.70x) | 64.2 (1.00x) | 70.9 (1.10x) | **node** |
| gc_churn | 178.3 (2.11x) | 84.6 (1.00x) | 69.9 (0.83x) | **bun** |
| promises | 88.1 (0.91x) | 97.1 (1.00x) | 91.7 (0.94x) | **tscore** |

## Async (per-engine variants, same algorithm)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| timer storm (2000x sleep 1ms) | 12.0 (0.94x) | 12.7 (1.00x) | 12.0 (0.94x) | **bun** |
| channel 100k msgs (vs worker postMessage) | 7.6 (0.15x) | 50.4 (1.00x) | 42.7 (0.85x) | **tscore** |

## Long-running (30s sustained mixed compute+alloc, single run)

| metric | tscore | node | bun | winner |
|---|---|---|---|---|
| throughput (ops/sec) | 505131 (0.77x) | 654451 (1.00x) | 515758 (0.79x) | **node** |
| stability (last/first decile) | 1.058 | 1.011 | 1.011 | |

## Multicore (tscore parallel.map vs node/bun worker_threads)

### primes

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 77.8 (0.83x) | 93.9 (1.00x) | 51.9 (0.55x) | **bun** |
| 2 | 40.7 (0.62x) | 65.1 (1.00x) | 36.5 (0.56x) | **bun** |
| 4 | 20.7 (0.47x) | 43.6 (1.00x) | 25.3 (0.58x) | **tscore** |
| 8 | 11.2 (0.33x) | 34.3 (1.00x) | 18.6 (0.54x) | **tscore** |

### mandelbrot

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 37.4 (0.75x) | 50.1 (1.00x) | 45.3 (0.90x) | **tscore** |
| 2 | 19.0 (0.55x) | 34.7 (1.00x) | 30.0 (0.87x) | **tscore** |
| 4 | 10.0 (0.31x) | 32.7 (1.00x) | 26.7 (0.82x) | **tscore** |
| 8 | 5.3 (0.17x) | 30.4 (1.00x) | 21.7 (0.72x) | **tscore** |

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
| tscore workers:1 | 147,557 | 1.0 | 148,363 **best/core** |
| bun x1 | 131,022 | 1.0 | 126,822 |
| bun x8 reusePort (darwin: 1 active, 7 idle) | 127,573 | 1.0 | 123,613 |
| node x1 | 98,156 | 1.0 | 95,737 |
| tscore workers:8 | 156,553 | 1.7 | 89,534 |
| node cluster x8 | 139,905 | 3.7 | 37,601 |

Total throughput is capped by this machine's loopback stack, not
by any engine: four independent servers with four independent
clients total the same as one. Compare the per-core column.

bun has no working multi-core HTTP on darwin: SO_REUSEPORT there
permits the shared bind but does not distribute, so one of the 8
processes serves every connection and the rest idle. Its row is a
second bun x1 measurement, not a multi-core one. node's cluster
distributes in userspace via the primary, and tscore's workers
share one listener with a kqueue per thread; both scale here.
## Runtime surface

| category | status |
|---|---|
| modules | SHIPPED — full ESM: static + dynamic import, cycles (TDZ), TLA; bare specifiers via tscore.json and node_modules (package.json exports/main); .js/.mjs/.cjs specifiers resolve to TS sources; import.meta.url/filename/dirname |
| networking | SHIPPED — runtime.net TCP (kqueue reactor, connect) + runtime.http |
| async file I/O | SHIPPED — runtime.fs (promise-native, dedicated I/O pool) + bytes |
| http throughput | see the HTTP section above (benchmarks/compare/http_bench.py) |
| async I/O | proxied by timer-storm + channel benchmarks above |
