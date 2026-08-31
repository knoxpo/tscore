# runtime.fs / runtime.net / runtime.http

Own, promise-native API surface (Deno-style; no Node compat layer).
All blocking work happens off the realm thread; results return through
the Completer/Wake contract (the timer wheel's pattern).

## runtime.fs

Dedicated 4-thread blocking-I/O pool (libuv model — never the CPU
scheduler pool). UTF-8 strings v1 (no bytes type in the value model; a
Bytes handle is the upgrade path).

- `readFile(path) -> Promise<string>` / `writeFile(path, s)` /
  `appendFile(path, s)`
- `readDir(path) -> Promise<string[]>` (sorted)
- `stat(path) -> Promise<{size, isFile, isDir, modifiedMs}>`
- `mkdir(path)` (recursive) / `remove(path)` (file or dir) /
  `exists(path) -> Promise<boolean>`

## runtime.net (TCP)

A single kqueue reactor thread drives every connection: nonblocking
sockets, readiness-queued reads/writes, results through the
Completer/Wake contract.

- `listen(port | {port}) -> Listener` (synchronous bind; acceptor thread)
- `accept(listener) -> Promise<Conn>`
- `connect(host, port) -> Promise<Conn>`
- `read(conn, maxBytes?) -> Promise<string | undefined>` (undefined = EOF)
- `write(conn, s) -> Promise<void>` / `close(conn | listener)`

Handles are `Foreign::Handle` objects (`__netConn`/`__netListener`
hidden fields) — they travel between realms by shared core like
channels.

## runtime.http

`serve(port, handler, {workers?})` — HTTP/1.1 with keep-alive and
chunked request bodies. Connection threads parse; handlers (sync or
async) run on the realm event loop — or, with `workers: N`, on N
isolated handler realms fed from a shared queue (the handler is
structured-cloned once per worker; main-realm globals/modules are not
visible inside it, same rule as parallel.map). Returns a string (200
text) or `{status?, headers?, body?}`; `serve` never returns.
Measured: 125k req/s at workers:8 (0.74x node cluster, wrk -c64).

## runtime.bytes + fs binary

Immutable byte buffers as handles: `fs.readBytes`/`fs.writeBytes`,
`bytes.size/slice/toString/fromString`.
