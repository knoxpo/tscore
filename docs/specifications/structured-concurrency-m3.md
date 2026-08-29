# Structured Concurrency (M3)

Every spawned task belongs to a scope; the scope cannot exit while any
child runs. No orphans, no detached work.

## API

```typescript
const result = task.scope((scope) => {
    const a = scope.spawn(() => expensiveA());
    const b = scope.spawn(() => expensiveB());
    scope.spawn(() => background());        // never joined: still awaited
    return a.join() + b.join();             // scope returns callback's value
}, { timeout: 5000 });                       // optional, ms
```

- The callback runs synchronously in the caller's realm; `spawn(fn)`
  structured-clones `fn` (parallel.map capture rules) and starts it on the
  work-stealing pool immediately.
- `handle.join()` blocks — and helps execute pool work — until that child
  finishes; returns its value cloned into the caller realm.
- `handle.cancel()` / `scope.cancel()` request cooperative cancellation.
- Scope exit blocks until every child completed or was cancelled.

## Error propagation

First child error (or callback error) → all remaining children are
cancelled → scope re-raises the error in the parent:

```text
Parent errored/cancelled → children cancelled → resources (realms) freed
```

Cancellation is not an error: cancelled children are simply discarded
(joining one raises "task cancelled").

## Cancellation points

Cooperative, at interpreter safepoints (the GC's): loop back-edges and
closure calls — a cancelled task stops at its next backward jump or call.
Explicit check: `Runtime.checkCancellation()`. Cascade: cancelling a task
whose callback runs a nested scope errors that scope, which cancels its
own children — cancellation flows down the tree.

## Timeouts

`{ timeout: ms }` arms a watchdog; on expiry the scope cancels all
children and errors with "scope timed out".

## M3 ceilings (documented, deliberate)

- `task.scope` is blocking; `async`/`await` + Promises arrive with the
  event loop milestone, making scopes awaitable.
- Scope/handle objects are realm-local natives (as actor handles are).
- `parallel.map` chunks inside a child do not yet inherit its cancel flag.
- Child realms are scratch realms: GC off, dropped wholesale at task end.
