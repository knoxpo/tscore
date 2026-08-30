# M6: lazy function compilation

> **Status: SHIPPED** (M6a–M6d). Parse row 21.7ms → ~12-13.5ms end-to-end
> on 50k LOC — statistical tie with bun (13.5 ± 3.9 same session); emit
> at startup 10ms → 0.2ms, resolve 4.4ms → 1.9ms. Remaining floor is oxc
> parse (~6.4ms) + process startup (~1.5ms). Deviations from the plan
> below: spans are preserved by left-padding the re-parsed snippet to its
> original file offset (no `env`-relative remapping, no LazyFile capture
> tables — the fill re-resolves the snippet with the stored upvalue names
> pre-seeded); parameter-pattern errors stay eager via a header-time scan;
> resolve fast-path used borrowed AST names + linear-scan scopes with a
> side index past 32 entries rather than oxc Atom plumbing.

Target: win the parse+compile row (~50k LOC: tscore 21.7ms vs bun 11.8ms)
and cut startup further, with zero regression on steady-state rows.

## Why we lose today

`tsc_parser::compile` is fully eager: oxc parses the whole file (~6ms),
the resolver walks the whole AST for capture analysis (~3.5ms), and the
emitter walks it again producing bytecode + running the optimizer for all
12,500 functions (~10ms), before a single instruction executes. Process
startup adds ~1.5ms. Bun (JSC) only *pre-parses* inner function bodies —
boundaries, captures, syntax validation — and materializes bytecode at
first call. The benchmark calls two trivial functions, so JSC compiles
roughly two.

Parallel emit was considered and rejected: the oxc arena AST is not
`Sync`, and sharding the emitter's scope stack across threads is worse
than not doing the work at all.

## Design

Keep eager: oxc parse (full-file syntax validation stays at startup),
resolve (capture/mutation sets), top-level emit, and per-function
*signatures*. Make lazy: inner function **body emission + optimizer**.

### 1. Split FunctionProto into header + lazily-filled body

```rust
pub struct FunctionProto {
    // header: known at startup, immutable
    pub name: Arc<str>,
    pub arity: u8,
    pub is_async: bool,
    pub arg_types: Vec<TypeHint>,
    pub upvals: Vec<UpvalSrc>,          // see §2
    pub jit: JitState,
    body: OnceLock<ProtoBody>,          // filled on first call
    lazy: Option<LazySource>,           // None = filled at construction
}

pub struct ProtoBody {
    pub n_regs: u8,
    pub code: Vec<Instr>,
    pub consts: Vec<Const>,
    pub spans: Vec<u32>,
    pub protos: Vec<Arc<FunctionProto>>,
}

struct LazySource {
    /// Byte range of the function text inside the retained source.
    span: (u32, u32),
    /// Shared handle to the original source + per-file capture tables.
    file: Arc<LazyFile>,
    /// Name -> upval index for this function's environment (§2).
    env: Vec<(Arc<str>, u8)>,
}
```

`proto.body()` returns `&ProtoBody`, filling via `OnceLock::get_or_init`
on first use (thread-safe by construction). Hot paths resolve the body
once per frame entry and pass `&ProtoBody` down — the per-call cost is
one `Acquire` load, same class as the existing tier check. Mechanical
refactor: every `proto.code` / `proto.consts` / `proto.protos` /
`proto.spans` / `proto.n_regs` read in interp / jit / tier1 / tier2 /
types / optimizer goes through the body reference.

### 2. Captures without the enclosing emitter

Today `UpvalSrc` lists are produced while the *parent* is being emitted
(the emitter threads captures through its live `FuncState` stack). A lazy
child compiled years later has no enclosing scopes. Fix: derive each
function's upval table at startup from resolver data.

- Extend `Resolver` to record, per function node (keyed by its span
  start): the ordered list of free names it references that resolve to an
  enclosing function's binding, with the binding's `captured`/`mutated`
  classification. This is the same walk it already does — the extra work
  is appending to a `HashMap<u32, Vec<CaptureRec>>`.
- Parent emission consumes that table to build the child's `upvals:
  Vec<UpvalSrc>` (register numbers come from the parent's own scopes,
  which are live during parent emission — parents always emit before
  children can be called, because the child closure is created by the
  parent's `Op::Closure`).
- The child's lazy fill gets `env`: name → upval index. During its
  delayed emission, a free name resolves: own scopes → `env` → global.
  Nested functions inside a lazy body emit eagerly *within that fill*
  (their parent emitter is live again), so laziness is one level deep per
  fill — exactly JSC's behavior.

### 3. Fill = re-parse the snippet

At fill time, run oxc on the retained source slice for the function
(arena created and dropped inside the fill; the full-file AST is *not*
kept alive — it dies at startup as today). Then run the existing emitter
with the `env` table injected as a pre-populated outer scope, then the
optimizer. Re-parsing a 4-line function is microseconds; it only ever
happens for functions that actually run.

`LazyFile` retains: `Arc<str>` source + the resolver capture tables.
Memory cost: the source text (already in memory during startup; now kept)
— trivial.

### 4. Thread boundary

`clone_out(PortableValue::Closure)` force-fills the proto before it
crosses to a worker (`proto.body()` call). Workers then share the filled
`Arc` — no cross-thread fill, no `Sync` requirement on the fill path
beyond what `OnceLock` provides. Same force-fill applies in
`Chunk`/actor spawn paths.

### 5. Error semantics (deliberate divergence)

- Full-file *syntax* errors: unchanged — oxc still parses everything at
  startup.
- *Subset* errors ("not supported in M1: …") inside a never-called
  function: today reported at startup, after M6 reported at first call
  (as a thrown runtime error carrying the original span). Document in
  language-subset-m1.md. If this proves painful, a cheap subset-scan walk
  at startup (statement kinds only, no emission) restores eager reporting
  for ~1ms.

### 6. Resolve fast-path (needed to actually win, not just tie)

Eager cost after M6 ≈ oxc 6ms + resolve 3.5ms + top-level emit ~0.5ms +
startup 1.5ms ≈ 11.5ms — a tie with bun's 11.8. Shave resolve:

- `String` keys → oxc `Atom` slices (no per-binding allocation).
- Scope maps → small `Vec<(&str, ..)>` linear scans (scopes are tiny).
- Skip descending into function bodies that declare no captures? No —
  captures are discovered by the walk. But the walk itself is cheap once
  allocation-free; target ≤1.5ms. End state ≈ 9.5ms < 11.8ms.

## Milestones

1. **M6a — body split, still eager.** Introduce `ProtoBody` +
   `proto.body()` accessor, refactor all readers, fill at construction.
   Pure refactor; every benchmark must be flat (A/B interleaved).
2. **M6b — resolver capture tables.** Per-function upval derivation;
   parent emit consumes tables instead of threading through `FuncState`.
   Goldens byte-identical.
3. **M6c — lazy fill.** `LazySource`, snippet re-parse, `env` scope
   injection, force-fill at portable boundary. Gate behind
   `TSC_NO_LAZY=1` escape hatch. Parse row target ≤ 12ms.
4. **M6d — resolve fast-path.** Atoms + Vec scopes. Parse row target
   ≤ 10ms. Win the row.

## Verification

- Full ritual per milestone: `cargo test --workspace` (48 suites), golden
  differential (`TSC_NO_JIT=1` vs `TSC_OSR_THRESHOLD=10
  TSC_JIT_THRESHOLD=1`), `RUNS=5 benchmarks/compare/run_compare.sh` with
  no-regression sweep, interleaved-binary A/B for the parse row
  (hyperfine already does this).
- New tests: lazy fill under concurrent first-call from main + worker
  (force-fill path); subset error inside never-called function (deferred
  report); deeply nested closures across lazy boundaries; OSR/tier-up of
  a lazily-filled proto.
- `TSC_COMPILE_PHASES=1` gains a `filled=N/M protos` line at exit.

## Risks

| risk | mitigation |
|---|---|
| body() accessor on hot paths | resolve once per frame entry, pass &ProtoBody down; A/B in M6a proves flatness before any laziness lands |
| capture-table drift vs emitter's current logic | M6b lands alone with byte-identical goldens |
| deferred subset errors surprise users | documented divergence + optional startup subset-scan |
| re-parse snippet disagrees with full-file parse (context-sensitive syntax) | snippet is re-parsed as a function expression in isolation; the subset has no context-sensitive constructs (no ASI hazards accepted, no with/eval); differential suite covers |
| multicore timed regions pay first-call fill | one small function per benchmark, ~10µs; mc rows already noise-bound at ±5% |
