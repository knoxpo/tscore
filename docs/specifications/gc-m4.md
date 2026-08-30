# Generational GC + Memory Runtime (M4)

## Collector: sticky in-place generational mark-sweep

Non-moving stays non-moving (Refs stable, Completers keep raw refs).
Generations are tracked per arena with an `old` bitmap plus a young
allocation log (the nursery); free-list reuse clears the old bit, so slot
recycling can never misclassify age.

- **Minor collection** (default, every 256k allocation events — array
  element growth counts): traces from roots + the remembered set, stops at
  old objects, sweeps only the young log. Pause is **O(young)**,
  independent of old-generation size (measured: 2× the old data, same
  minor pause; major pause doubles).
- **Promotion**: survive one minor → old (sticky). Promoted bytes are
  tracked; a **major** (full mark-sweep) runs when promoted bytes exceed
  max(8 MB, half the live set), when the remembered set degenerates
  (> 32k containers), or on `runtime.gc.collect()`. Majors reset old bits
  from live marks.
- **Write barrier**: container-only, dedup via dirty bit —
  `if old && !dirty { remember }` — on SetField/SetIndex/ArrayPush/
  StoreCell/SetUpval and the async runtime's promise/coroutine mutations.
  Zero executions on arithmetic-only paths (fnv/fib/mandelbrot flat).
- `TSCORE_GC_MAJOR_ONLY=1` env forces the old behavior (A/B debugging).

Measured on gen_churn (1M-object live old set + 3M young churn):
minor pause ~1.8ms vs major ~6.8ms at 1M (~4×), ~2.0ms vs ~10.1ms at 2M
(5× and growing with heap) — and total wall 1.9× faster.

## Memory limits

`--max-heap <MB>` (main realm) and `actor(setup, { maxHeap: bytes })`.
Live bytes are measured during major collections (approximation:
string bytes + array capacity×8 + object fields×24 + slot overheads);
exceeding the limit raises a clean runtime error at the next safepoint.
Burst between majors is bounded by the promotion trigger.

## Telemetry

- `runtime.gc.stats()` → { minorCollections, majorCollections, lastFreed,
  lastLive, liveBytes, lastPauseUs, maxPauseUs, heapSlots,
  promotedSinceMajor, rememberedPeak }
- `runtime.gc.collect()` → force a major.
- `runtime.stats()` → { workers, tasksExecuted, steals, submits, parks,
  actors, gcMinor, gcMajor }.

## Observability (spec §30)

- `tscore run --stats` prints an exit summary (workers, tasks, steals,
  parks, actors, GC counts/pauses, heap).
- Every `tscore run` exports a stats snapshot to
  `$TMPDIR/tscore-stats-<pid>.txt` twice a second (removed on exit;
  `--no-stats-export` disables).
- **`tscore top`** renders a live, 1s-refresh table of all running tscore
  processes: PID, script, uptime, workers, tasks, steals, heap, GC m/M,
  last pause, actors.
