# Async/Await, Promises, Event Loop (M3 completion)

The spec's headline API is now real: `await parallel.map(...)`,
`await handle.send(...)`, `await Channel.recv(ch)`, `await sleep(ms)`,
top-level `await`.

## Model

- **One coroutine = one suspendable frame.** JS guarantees `await` appears
  only directly in async fn bodies, so suspension never crosses a sync
  frame. Sync calls stay Rust-recursive (the measured-faster design);
  async frames snapshot their registers into a heap `Coroutine` on
  suspension and restore them onto the register stack on resume.
- Calling an async function runs its body **synchronously to the first
  await** (JS-true), then returns a Promise.
- `<main>` is compiled async — top-level await for free.
- Promises live in the **foreign arena** (NaN-box `TAG_SPECIAL` payloads
  ≥ 8), alongside coroutines and shared handles (channels).
- Rejections carry the runtime error (there is no try/catch in the
  subset): a rejected await re-raises in the awaiting frame and cascades
  through promise chains to main. Unobserved rejections warn at sweep.

## Event loop (per realm)

Microtask queue of ready coroutines + a crossbeam wake channel for
cross-thread completions (timers, pool batches, actor replies, channel
handoffs) via `Realm::promise_pair() -> (promise, Completer)`. The
`Completer` is Send, settles exactly once (dropping unsettled reports an
error), and keeps its promise GC-pinned until processed.

Loop: drain microtasks → drain wakes → main settled? done. Otherwise:
no external completers outstanding → **deadlock error**; else help the
work-stealing pool while waiting (a `--workers 1` pool has zero threads —
the realm thread executes jobs itself, preserving the honest serial
baseline).

Every realm gets its own loop: parallel chunks, scope children, and actor
handlers drive async callbacks/handlers to settlement locally.

## Async API surface

| API | returns |
|---|---|
| `sleep(ms)` | Promise (thread-per-timer; timer wheel later) |
| `parallel.map / parallel.for` | Promise (last-finishing chunk settles) |
| `actorHandle.send(msg)` | Promise (actor replies via Completer) |
| `Channel.send / Channel.recv` | value when immediate, Promise when parked |
| `task.scope`, `handle.join` | still blocking internally (sync island); `await` passes their values through — async scope bodies are the documented follow-up |

`await <non-promise>` is identity. Divergence from full JS, documented:
settled/plain awaits skip the microtask tick (no `.then` in the subset to
observe the difference).

## Channels (spec Phase 5)

```typescript
const ch = Channel.create({ capacity: 2 });
await Channel.send(ch, value);      // parks when full (backpressure)
const v = await Channel.recv(ch);   // undefined once closed + drained
Channel.close(ch);
```

Global-style API (ponytail: method syntax when method dispatch lands).
Handles are plain objects with a hidden `__channel` foreign ref and are
**portable**: structured clone carries the shared core across realms —
channels work inside parallel chunks, scope children, and actors.
`close` resolves parked receivers with `undefined` and rejects parked
senders. A `recv` that nothing will ever send to blocks (its completer
counts as external work) — like Node with an open handle.

## Cancellation composition

`parallel.map` chunks inherit the calling task's cancel flag: cancelling
a scope child cancels its inner parallel work at the next safepoint.

## Actors (updated)

`actor(setup, { mailbox: N })` — capacity option. Handlers may be async
(`churn: async (m) => { ... await ... }`): the actor drives its own loop
per delivery. Actor realms include `parallel.*`, `task.scope`, and
`Channel`. Actor handles remain realm-local (portable actor refs arrive
with method dispatch).
