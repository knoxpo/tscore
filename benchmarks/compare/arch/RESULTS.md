# Architecture probes: tscore vs node (V8) vs bun (JSC)

- date: 2026-09-06 01:38
- machine: Apple M5 Max, 18 logical cpus (6P + 12E)
- tscore: tscore 0.1.0, node: v24.17.0, bun: 1.4.0
- cells: median of 5 runs, first run discarded; pools/workers created and warmed before timing
- ratios: engine / node — lower is better
- node/bun variants use worker_threads + postMessage, the idiomatic way to use other cores there

## Fan-out: hand N trivial items to other cores and collect results (ms per batch, 18 workers)

| items | tscore parallel.map | node worker pool | bun worker pool | winner |
|---|---|---|---|---|
| 8 | 0.188 (1.44x) | 0.131 (1.00x) | 0.091 (0.70x) | **bun** |
| 64 | 0.254 (0.53x) | 0.476 (1.00x) | 0.328 (0.69x) | **tscore** |
| 512 | 0.440 (0.17x) | 2.528 (1.00x) | 1.750 (0.69x) | **tscore** |
| 4096 | 0.521 (0.03x) | 19.437 (1.00x) | 12.694 (0.65x) | **tscore** |

## Pause detector: 1M-object live set + 5 s allocation churn (ms; same file on all engines)

| metric | tscore | node | bun | winner |
|---|---|---|---|---|
| MAX_STALL_MS | 4.00 (0.35x) | 11.49 (1.00x) | 4.81 (0.42x) | **tscore** |
| P99_MS | 0.10 (1.00x) | 0.10 (1.00x) | 0.10 (1.00x) | **tscore** |
| P999_MS | 2.40 (24.00x) | 0.10 (1.00x) | 0.40 (4.00x) | **node** |
| iterations in 5 s (higher is better) | 129970000 | 1108742000 | 725608000 | |

## Payload: round-trip a nested object to another realm/thread and back (ms per round trip)

| items (~bytes) | tscore parallel.map | node postMessage | bun postMessage | winner |
|---|---|---|---|---|
| 10 (~1 KB) | 0.057 (2.20x) | 0.026 (1.00x) | 0.026 (0.99x) | **bun** |
| 1,000 (~100 KB) | 0.394 (0.49x) | 0.805 (1.00x) | 0.654 (0.81x) | **tscore** |
| 100,000 (~10 MB) | 41.477 (0.49x) | 84.256 (1.00x) | 92.932 (1.10x) | **tscore** |
