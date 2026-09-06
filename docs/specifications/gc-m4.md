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

## Copying nursery (evacuating young generation)

Objects, arrays and strings are born in a **bump-allocated nursery**
(young ref = payload bit 31; closures/cells/foreigns stay non-moving —
raw refs live in JIT registers and cross-thread Completers). Allocation
is a push plus recycled backing buffers; there is no young log and no
free-list search for these kinds.

- **Minor = evacuation**: the trace rewrites every reachable edge in
  place (stack slots, globals, remembered containers, coroutine regs);
  young survivors move into the old arenas via dedicated `promote_*`
  (old bit set, young log skipped — the normal alloc path would clear
  the bit and silence barriers). Dead nursery slots are never visited
  beyond a buffer-salvage walk; the nursery resets by truncation. Minor
  cost is **O(live young)**, not O(allocated).
- **Trigger**: nursery bytes ≥ `TSC_NURSERY_BYTES` (default 16 MB —
  sized so short-lived working sets die in-nursery; 2 MB measured +37%
  on gc_churn from premature evacuation). `TSC_NURSERY_BYTES=1` doubles
  as GC-stress mode (minor at every safepoint); `TSC_NO_NURSERY=1` opts
  out entirely.
- **Majors are unchanged** and always preceded by an evacuation (the
  major trace indexes old arenas raw and asserts an empty nursery).
- **JIT**: inline heap templates select old vs nursery arena base with
  one shifted load from a per-realm `[old, young]` base-pair table
  (`Heap::refresh_bases()` keeps it coherent across pointer moves), and
  an optimizing-compiler field-access CSE cache (x15/x16: validated object address +
  shape id, zeroed on any cold-path call) collapses repeated accesses to
  the same object into single slot loads.
- **Moving-GC invariant**: no obj/arr/str `Value` may live in a Rust
  local across a safepoint. Natives receive `NativeArgs` (stack indices)
  instead of copied values for exactly this reason.

Measured (same-state A/B at landing): alloc −23%, gc_churn −25%,
fnv −6%, objects net −1% after the CSE cache, closures/mandelbrot flat.

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
