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

One OS thread per connection with a per-connection job queue (reads and
writes serialize naturally). kqueue/io_uring reactor is the upgrade path.

- `listen(port | {port}) -> Listener` (synchronous bind; acceptor thread)
- `accept(listener) -> Promise<Conn>`
- `read(conn, maxBytes?) -> Promise<string | undefined>` (undefined = EOF)
- `write(conn, s) -> Promise<void>` / `close(conn | listener)`

Handles are `Foreign::Handle` objects (`__netConn`/`__netListener`
hidden fields) — they travel between realms by shared core like
channels.

## runtime.http

`serve(port, handler)` — minimal HTTP/1.1 with keep-alive. Connection
threads parse requests; the handler (sync or async) runs on the realm's
event loop; `serve` never returns (a server is the program). Handler
returns a string (200 text) or `{status?, headers?, body?}`. Sequential
dispatch v1 — parallel handler realms are the upgrade path.
