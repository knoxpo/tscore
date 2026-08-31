# ES Modules

Full static + dynamic ESM over the existing engine — zero new opcodes,
zero JIT changes.

## Model

- Each module gets a **namespace object** registered in the realm's
  globals under the reserved key `"\0mod:<canonical-path>"` (NUL prefix:
  unspellable from TS). Existing GetGlobal/GetField machinery and GC
  tracing cover everything.
- **Immutable exports** publish their value into the ns at initialization.
- **Mutable exports** (reassigned anywhere, incl. every `function`
  declaration) live in a cell; the module publishes the *cell* into the
  ns. `GetField` auto-derefs cell-valued fields (only namespaces can hold
  cells — user objects can't obtain one), so importers always read the
  live binding, statically or dynamically.
- Namespace objects carry three hidden pad fields, pushing exports into
  overflow slots — the JIT's inline field templates (slots < 3) never
  serve them, so the deref lives only in the helper path: **zero cost on
  ordinary object access**.

## Pipeline

- The CLI routes entries containing module syntax (word-boundary raw-text
  check) through `tsc_modules::compile_graph`; plain scripts keep the
  single-file path (incl. the parallel frontend) untouched.
- Discovery: BFS parse from the entry, extracting deps + export metadata
  (mutability from the resolver's mutated set — module-local, so cycles
  need no fixpoint). Re-export mutability flattens transitively
  (`export * from` expands to the dep's effective set).
- Per-file compilation via `tsc_parser::compile_module` with an import
  map (`local name -> dep key + export`) consumed at the emitter's
  `Place::Global` hook. Assignment to an import is a compile error.
- Evaluation: instantiate all namespaces first (cycle safety), then
  post-order DFS; cycles are **undefined-then-live** (documented
  deviation: no TDZ throw in v1). Each module body may use top-level
  await — it is driven to completion before its importers run.

## Resolution

- Relative: exact, then `.ts`, then `/index.ts`.
- Bare: `tscore.json` (walked up from the entry) — `{"moduleDirs":
  ["modules", ...]}`; tries `<root>/<dir>/<spec>.ts` then
  `<dir>/<spec>/index.ts`.

## Dynamic import()

Lowered to the hidden native `__import(spec, importerId)`. Cache hit
returns an already-fulfilled promise with the same namespace object;
misses compile and graft the new subgraph onto the live registry
(modules already present are not re-instantiated).

## Known deviations (v1)

- Cycles: undefined-then-live, no TDZ.
- Namespace objects crossing realm boundaries (actors/parallel) fail on
  mutable exports (cells don't clone across realms).
- Errors carry per-module source attribution (`RtError.source`).
