# Engine comparison: tscore vs node (V8) vs bun (JSC)

- date: 2026-09-02 00:16
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
| startup (hello.ts) | 1.8 (0.05x) | 39.4 (1.00x) | 4.9 (0.12x) | **tscore** |
| parse+compile (~50k LOC) | 9.3 (0.09x) | 101.9 (1.00x) | 11.9 (0.12x) | **tscore** |

## Steady-state (shared sources, in-program TIME_MS)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| objects | 138.5 (5.75x) | 24.1 (1.00x) | 24.1 (1.00x) | **node** |
| closures | 109.1 (3.21x) | 34.0 (1.00x) | 48.9 (1.44x) | **node** |
| alloc | 161.2 (3.71x) | 43.5 (1.00x) | 47.4 (1.09x) | **node** |
| gc_churn | 335.7 (5.82x) | 57.6 (1.00x) | 43.6 (0.76x) | **bun** |
| promises | 63.3 (1.00x) | 63.3 (1.00x) | 57.7 (0.91x) | **bun** |

## Async (per-engine variants, same algorithm)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| timer storm (2000x sleep 1ms) | 11.7 (0.92x) | 12.7 (1.00x) | 12.0 (0.94x) | **tscore** |
| channel 100k msgs (vs worker postMessage) | 5.1 (0.15x) | 34.8 (1.00x) | 29.1 (0.84x) | **tscore** |

## Long-running (30s sustained mixed compute+alloc, single run)

| metric | tscore | node | bun | winner |
|---|---|---|---|---|
| throughput (ops/sec) | 426923 (0.42x) | 1007924 (1.00x) | 798819 (0.79x) | **node** |
| stability (last/first decile) | 0.988 | 0.986 | 0.912 | |

## Multicore (tscore parallel.map vs node/bun worker_threads)

### primes

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 76.9 (1.22x) | 63.1 (1.00x) | 35.9 (0.57x) | **bun** |
| 2 | 40.4 (0.91x) | 44.4 (1.00x) | 25.2 (0.57x) | **bun** |
| 4 | 21.2 (0.63x) | 33.5 (1.00x) | 17.2 (0.51x) | **bun** |
| 8 | 12.4 (0.45x) | 27.8 (1.00x) | 15.8 (0.57x) | **tscore** |

### mandelbrot

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 25.2 (0.71x) | 35.6 (1.00x) | 31.9 (0.90x) | **tscore** |
| 2 | 13.0 (0.50x) | 26.2 (1.00x) | 21.6 (0.82x) | **tscore** |
| 4 | 7.1 (0.30x) | 23.8 (1.00x) | 20.1 (0.84x) | **tscore** |
| 8 | 4.0 (0.17x) | 23.9 (1.00x) | 17.2 (0.72x) | **tscore** |

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
| bun x1 | 188,632 | 1.0 | 183,074 **best/core** |
| tscore workers:1 | 183,245 | 1.0 | 176,612 |
| bun x8 reusePort | 176,929 | 1.0 | 176,191 |
| node x1 | 143,701 | 1.0 | 141,644 |
| tscore workers:8 | 193,432 | 2.1 | 91,983 |
| node cluster x8 | 171,926 | 3.8 | 45,097 |

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
