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

- Relative: the specifier's *source* form first (`./x.js` -> `x.ts`,
  `.mjs` -> `.mts`, `.cjs` -> `.cts`, `.js` -> `.tsx`), then the literal
  path, then `.ts`/`.tsx`/`.mts`/`.cts`, then `/index.ts(x)`. Writing the
  emitted `.js` extension is mandatory under `moduleResolution: nodenext`,
  so that form has to resolve.
- Bare, in order: `tscore.json` `paths` prefixes, then its `moduleDirs`
  (walked up from the entry, `{"moduleDirs": ["modules", ...]}`), then
  `node_modules` walked up from the importer.
- A `node_modules` package's entry comes from `package.json`: `exports`
  as a string, or as an object taking `"."` then the first of
  `"import"`/`"default"`; else `main`; else the package root's index
  candidates. A deep import (`pkg/sub/path`) addresses the file directly.
  No wildcards, no `imports`, no self-reference.

## import.meta

`import.meta.url` / `.filename` / `.dirname` are constant-folded per
module from its canonical path. `import.meta` is not a first-class object
— only these member reads compile; anything else is a compile error.

## Dynamic import()

Lowered to the hidden native `__import(spec, importerId)`. Cache hit
returns an already-fulfilled promise with the same namespace object;
misses compile and graft the new subgraph onto the live registry
(modules already present are not re-instantiated).

## Known deviations (v1)

- Cycles: a binding read before its module has evaluated throws
  "cannot access module binding before initialization" (the TDZ
  sentinel), rather than yielding undefined. A named re-export that
  materializes mid-cycle copies that sentinel; the source module repairs
  its dependents' namespaces when it finishes evaluating, so the value is
  correct for every read after the cycle completes.
- Namespace objects **snapshot** when they cross a realm boundary
  (actors/parallel/channels): cells deref to plain values, uninitialized
  exports arrive as `undefined`, and the hidden pads are dropped. Live
  bindings are live only within the realm that owns them.
- Errors carry per-module source attribution (`RtError.source`).
