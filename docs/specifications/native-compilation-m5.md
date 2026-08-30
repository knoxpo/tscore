# Typed IR + Native Compilation (M5)

Hand-rolled ARM64 JIT, two tiers, hot-function compilation (threshold 50
calls, `TSC_JIT_THRESHOLD` override, `TSC_NO_JIT` / `TSC_NO_TIER2` kill
switches). Interpreter remains the general path; async functions stay
interpreted.

## Encoder + pages

`tsr-jit::asm`: ~55 AArch64 instructions, every encoding byte-exact
against clang-assembled fixtures. Executable memory: one MAP_JIT
reservation, bump-allocated; Apple Silicon W^X (per-thread
`pthread_jit_write_protect_np`, `sys_icache_invalidate`) behind three
functions in platform/macos.

## Tier-1 (baseline templates)

Every bytecode register keeps its `realm.stack` slot — GC rooting,
safepoints, and error paths unchanged. Inline NaN-box fast paths for
arithmetic/comparisons/branches; single-step helper fallback shares exact
interpreter semantics. Eligibility: any non-async proto, EXCEPT functions
with calls inside loops (the helper hop per iteration loses to the
interpreter's inline native-call arm — measured, not assumed).

## Tier-2 (typed, unboxed)

Key insight: NaN-boxing means a number's Value bits ARE its f64 bits, so
d-registers hold raw Value bits and arithmetic runs on them directly — no
boxing, no guards where `tsc-types` proves operands numeric.

- `tsc-types`: forward dataflow over bytecode from TS `: number`
  annotations (flat lattice, fixpoint over joins). Qualifies functions
  and marks provably-numeric branch conditions.
- vregs 0-7 live in callee-saved d8-d15; higher vregs stay in slots.
  Guards exist only at entry (annotated args; violation deopts to pc 0)
  — deopt resumes in the interpreter via the same `start_pc` machinery
  async uses. Repeated deopts demote the function to Tier-1.
- Disqualifiers: async, upvalues, string/object ops, non-number dataflow,
  and Call (spill-everything at call sites regressed call-in-loop
  functions; liveness-based spilling is the designed upgrade).
- d-registers never hold heap references (facts admit only
  number/bool/undefined), so GC needs no JIT frame maps.

## Measured (Apple M5 Max, steady state)

| workload | interp | Tier-1 | Tier-2 | node | bun |
|---|---|---|---|---|---|
| annotated numeric loop | 495ms | 290ms | **70ms** | 66ms | 20ms |
| fib(35) (call-bound) | 393ms | 348ms | — | — | 31ms |
| mandelbrot ×8 cores | — | — | **17ms** | 22ms (1 core) | — |

Single-core numeric loops now match Node; the JIT composes with the
work-stealing scheduler (compiled code is cached on the Arc-shared proto,
so every worker realm shares one compilation).

## Known limits (deliberate, documented)

- No OSR: a hot loop in a function called once stays interpreted.
- Calls: Tier-2 excludes them; Tier-1 excludes them in loops. Direct
  native→native calls + liveness spilling are the next tier of work.
- bun/JSC remains ahead on peak single-core (int32 specialization,
  bounds-check elimination, inlining — out of M5 scope).

## Inline heap reads (post-M5 push)

Tier-1 templates inline the read hit paths — no helper call:

- `GetField`: tag check → shape-id load → per-pc IC compare → direct
  values-slot load (~19 instructions; helper only on IC miss, which
  also fills the cache).
- `GetIndex`: tag + exact-integer index check (fcvtzs round-trip) +
  unsigned bounds → direct element load.
- `Len` on arrays: direct length load.

Writes stay in helpers (write barriers + shape transitions belong in
Rust). Reads cannot allocate, so arena data pointers are stable within
an inline sequence and are re-loaded per op — no caching discipline.

**Runtime layout probing** instead of `#[repr(C)]`: at startup, struct
offsets (Realm→objs/arrs data pointers, Obj internals, Vec internals,
Arc/ShapeData deltas) are discovered by scanning sentinel-filled
structures, then self-verified by reading known values back through the
discovered offsets. Probe failure or ambiguity disables the inline
paths — a std/compiler layout change degrades speed, never correctness.

Also fixed here: JS array-index semantics (`arr[-1]`, `arr[1.5]` are
undefined, not element 0/1 — f64→usize saturation bug); OSR (on-stack
replacement) at interpreter back-edges — Tier-1's slot-resident
registers make any loop header a valid entry point.
