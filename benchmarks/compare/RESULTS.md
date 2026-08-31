# Engine comparison: tscore vs node (V8) vs bun (JSC)

- date: 2026-08-31 21:45
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
| startup (hello.ts) | 2.5 (0.04x) | 56.1 (1.00x) | 6.6 (0.12x) | **tscore** |
| parse+compile (~50k LOC) | 11.6 (0.08x) | 147.1 (1.00x) | 17.5 (0.12x) | **tscore** |

## Steady-state (shared sources, in-program TIME_MS)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| objects | 255.1 (7.03x) | 36.3 (1.00x) | 34.9 (0.96x) | **bun** |
| closures | 170.5 (3.27x) | 52.1 (1.00x) | 72.5 (1.39x) | **node** |
| alloc | 272.5 (4.16x) | 65.6 (1.00x) | 78.2 (1.19x) | **node** |
| gc_churn | 764.9 (5.84x) | 131.0 (1.00x) | 81.3 (0.62x) | **bun** |
| promises | 101.5 (1.03x) | 98.2 (1.00x) | 124.5 (1.27x) | **node** |

## Async (per-engine variants, same algorithm)

| benchmark | tscore | node | bun | winner |
|---|---|---|---|---|
| timer storm (2000x sleep 1ms) | 12.3 (0.97x) | 12.6 (1.00x) | 12.0 (0.95x) | **bun** |
| channel 100k msgs (vs worker postMessage) | 7.9 (0.15x) | 53.5 (1.00x) | 44.1 (0.82x) | **tscore** |

## Long-running (30s sustained mixed compute+alloc, single run)

| metric | tscore | node | bun | winner |
|---|---|---|---|---|
| throughput (ops/sec) | 274910 (0.42x) | 653815 (1.00x) | 498898 (0.76x) | **node** |
| stability (last/first decile) | 1.002 | 1.018 | 0.968 | |

## HTTP hello (wrk -t2 -c32 -d5s; `python3 benchmarks/compare/http_bench.py`)

Total req/s here is capped by this machine's loopback stack, not by any
engine: four *independent* server processes, each with its own wrk,
total the same as one server alone. A multi-worker config can therefore
look faster while only burning more cores. **Compare the per-core
column** — it is the only figure that measures the engine.

| engine | req/s | cores | req/s per core |
|---|---|---|---|
| tscore workers:1 | 142,337 | 1.1 | **127,532** |
| bun x8 reusePort | 127,546 | 1.0 | 122,864 |
| bun x1 | 126,123 | 1.0 | 122,430 |
| node x1 | 93,634 | 1.0 | 91,616 |
| tscore workers:8 | 153,225 | 2.0 | 78,020 |
| node cluster x8 | 131,997 | 3.7 | 36,131 |

Notes: bun's `reusePort` processes add no busy cores here (~1.0), so
multi-process bun is not a speedup on this box. tscore `workers:8` buys
~+8% throughput for ~2x the CPU — a poor trade on this benchmark, which
is why `workers:1` is the default. Absolute numbers drift with ambient
machine load; only same-window comparisons are meaningful.

## Multicore (tscore parallel.map vs node/bun worker_threads)

### primes

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 114.4 (1.22x) | 94.0 (1.00x) | 52.1 (0.55x) | **bun** |
| 2 | 59.4 (0.91x) | 65.1 (1.00x) | 36.4 (0.56x) | **bun** |
| 4 | 30.4 (0.69x) | 44.3 (1.00x) | 24.5 (0.55x) | **bun** |
| 8 | 15.7 (0.46x) | 34.0 (1.00x) | 18.6 (0.55x) | **tscore** |

### mandelbrot

| workers | tscore | node | bun | winner |
|---|---|---|---|---|
| 1 | 37.2 (0.73x) | 50.8 (1.00x) | 46.1 (0.91x) | **tscore** |
| 2 | 19.0 (0.54x) | 35.5 (1.00x) | 30.3 (0.85x) | **tscore** |
| 4 | 10.1 (0.31x) | 32.7 (1.00x) | 27.3 (0.84x) | **tscore** |
| 8 | 5.2 (0.17x) | 30.6 (1.00x) | 21.3 (0.70x) | **tscore** |

## Scoreboard

| engine | wins |
|---|---|
| tscore | 8 |
| node | 4 |
| bun | 6 |

Win = fastest median (highest throughput for long-running) on that row.

## Runtime surface

| category | status |
|---|---|
| modules | SHIPPED — full ESM (static + dynamic import, cycles, bare specifiers, TLA) |
| networking | SHIPPED — runtime.net TCP (kqueue reactor, connect) + runtime.http |
| async file I/O | SHIPPED — runtime.fs (promise-native, dedicated I/O pool) + bytes |
| http throughput | see the HTTP row above (benchmarks/compare/http_bench.sh) |
| async I/O | proxied by timer-storm + channel benchmarks above |
