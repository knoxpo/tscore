# Actors (M2)

Long-lived isolated execution: each actor owns a realm (heap + globals),
a bounded mailbox, and a dedicated OS thread that drains it.

## API

```typescript
const counter = actor(() => {
    let n = 0;                       // actor state = captured locals
    return {
        add: (msg) => { n = n + msg.by; },
        get: () => n,
    };
});

counter.post({ type: "add", by: 2 });   // fire-and-forget, queued
const v = counter.send({ type: "get" }); // blocks for the reply
counter.stop();                          // drain-and-exit
```

- The setup closure is structured-cloned into the actor realm and runs
  there, synchronously — `actor()` returns only after setup succeeds (a
  setup error fails `actor()` in the caller, with the actor's message).
- It returns an object whose fields are message handlers; `msg.type`
  selects the handler, which receives the whole message.
- Messages and replies cross realms as PortableValue structured clones —
  the same rules as `parallel.map` captures: no natives, no cycles.

## Semantics

- **Sequential locally, parallel globally**: one actor processes one
  message at a time in mailbox order; different actors run concurrently.
- **Backpressure**: the mailbox is bounded (1024); `post`/`send` block the
  caller when it is full.
- **Failure isolation**: a handler error replies the error to a `send`er
  (runtime error in the caller) or logs it for a `post`; the actor keeps
  running and other actors are untouched. Actor realms GC independently.
- **`stop()`**: the mailbox closes, queued messages drain, the thread
  exits. Messages after stop are a runtime error.

## M2 ceilings (documented, deliberate)

- One OS thread per actor — worker-affine per the roadmap; scheduler
  integration when actor counts outgrow threads.
- Actor handles are realm-local natives: they work in the realm that
  called `actor()` and cannot yet be captured into `parallel.map` or other
  actors. Portable actor references arrive with channels (M3).
- `send` blocks; it becomes awaitable when async lands (M3).
