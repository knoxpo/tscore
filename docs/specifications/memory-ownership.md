# Memory & Ownership (M1)

## Per-realm heaps

```text
Realm A → Heap A (arena)      Realm B → Heap B      …
```

No globally shared JS heap. `Gc<T>` is a handle into its realm's arena and
never escapes it — enforced by the PortableValue clone boundary, so no
cross-thread synchronization exists anywhere in the memory system.

## Realm kinds

- **Main realm** — lives for the whole program; needs collection.
- **Scratch realm** — one per `parallel.map` chunk task; bump-allocate, run,
  **free the whole arena on task death**. Region-based memory management: the
  hot parallel path never runs GC at all.

## GC (main realm only, P4)

Stop-the-realm mark-sweep, triggered at an allocation threshold (256k
allocations) at safepoints: loop back-edges and closure calls. Roots are the
realm's register stack plus globals (conservative over-approximation: stale
slots above the live frame top may retain garbage one cycle — safe, never
corrupting). Non-moving: the sweep rebuilds per-arena free lists and
allocation reuses slots, so `Ref` handles stay stable. Natives must not hold
unrooted `Ref`s across interpreter re-entry — scratch realms therefore run
with GC disabled (they drop wholesale anyway).

Later phases per the roadmap: generational → concurrent marking → parallel
collection → NUMA-aware placement. None of it blocks M1.

## Object layout

- `Obj`: inline `Vec<(InternedStr, Value)>`, linear scan ≤ 8 fields, spills
  to hash map. No shapes/hidden classes at M1.
- `Arr`: `Vec<Value>`. Strings: per-realm interned for field names, immutable
  heap strings otherwise.

## PortableValue — the cross-realm data contract

```rust
enum PortableValue { Number(f64), Bool(bool), Null, Undefined,
    Str(Arc<str>),
    Array(Vec<PortableValue>),
    Object(Vec<(Arc<str>, PortableValue)>),
    Closure { proto: Arc<FunctionProto>, upvalues: Vec<PortableValue> } }
```

- `clone_out(realm_value) -> PortableValue`: deep copy; detects cycles
  (visited set) → error; closures clone their captured environment
  recursively; anything else non-cloneable → error.
- `rehydrate(portable, &mut realm) -> Value`: allocates realm-local copies.

Ownership *transfer* (zero-copy `transfer(buffer)`) and `SharedBuffer` are
M2+; the M1 escape hatch for data-heavy workloads is planned as shared
immutable typed arrays.
