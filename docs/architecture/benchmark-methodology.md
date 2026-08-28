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
