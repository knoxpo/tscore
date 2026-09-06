# Benchmark Methodology (M1)

## Claim under test

M1 claims **multicore scaling with clean ergonomics**, not single-core peak
performance. Reports always show both:

1. **Scaling curve** — `speedup(N) = t(1 core) / t(N cores)` on our own runtime.
2. **Absolute times** vs Node (worker_threads), Bun (Workers), Go
   (goroutines), Rust (rayon) — honesty check against "parallelizing molasses".

## Targets (prototype, not guarantees)

| Cores | Speedup |
|---|---|
| 2 | ≥ 1.7× |
| 4 | ≥ 3.0× |
| 8 | ≥ 5.5× |

Plus: 10,000+ runtime tasks scheduled without 10,000 OS threads.

## Workloads

Compute-per-element high (so clone overhead is honest but not dominant):

- FNV-style hashing over ranges
- Prime trial division (skewed per-item cost — exercises lazy splitting)
- Mandelbrot rows

Each ported 1:1 to every comparison runtime, same algorithm, no SIMD tricks.

## Harness rules

- Warmup run discarded; median of ≥5 runs; wall-clock via `Instant`.
- Clone/rehydrate time measured and reported **separately** from compute.
- `benchmarks/run.sh` emits the table and asserts the targets.
- Machine topology (`sysctl`/`/proc`) recorded in every report.

## Machine-state caveat

Absolute times on laptops swing ±40% with power/thermal state (observed:
same binary, same day, 8.7s vs 12.4s on fnv serial). Benchmark plugged in,
compare only numbers from one session, and trust ratios over absolutes.

## JIT-aware profiling

`sample` reports compiled-code addresses as `??? (in <unknown binary>)`.
Two runtime hooks fix that:

- `TSC_JIT_MAP=<file>` — one `<hex addr> <hex size> <name>` line per
  published region (`baseline:`/`optimizing:`/`osr:` + function name).
- `TSC_JIT_DUMP=<dir>` — raw bytes per region, for offset-level
  disassembly (`clang -c` a `.long` listing, then `otool -tvV`).

`benchmarks/jitprof.sh <program.ts>` joins a sample against the map and
prints the hot symbols plus the hottest offsets inside the top JIT
region — the input to any codegen change.

`benchmarks/compare/http_bench.sh [workers]` produces the HTTP row
(needs `wrk`; servers are long-lived, so it is not part of
`run_compare.sh`).
