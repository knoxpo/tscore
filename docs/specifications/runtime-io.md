# runtime.fs / runtime.path / runtime.net / runtime.http

Own, promise-native API surface (Deno-style; no Node compat layer).
All blocking work happens off the realm thread; results return through
the Completer/Wake contract (the timer wheel's pattern).

## runtime.fs

Dedicated 4-thread blocking-I/O pool (libuv model — never the CPU
scheduler pool). Anything that touches the filesystem returns a promise;
`cwd` and `tempDir` are process state, not disk traversal, and are the
only two synchronous members.

- `readFile(path) -> Promise<string>` (strict UTF-8) / `writeFile(path, s)` /
  `appendFile(path, s)`
- `readDir(path) -> Promise<{name, isFile, isDir, isSymlink}[]>` (sorted by
  name). The type comes back in the dirent, so a tree walk costs no extra
  syscall per entry; only a filesystem reporting an unknown type falls back
  to a stat. **This replaced the earlier `string[]` shape.**
- `stat(path)` / `lstat(path)` -> `Promise<{size, isFile, isDir, isSymlink,
  modifiedMs}>`. `stat` follows symlinks, `lstat` does not.
- `mkdir(path)` (recursive) / `remove(path)` (file or dir) /
  `exists(path) -> Promise<boolean>`
- `rename(from, to)` / `copy(from, to)` / `truncate(path, len)` /
  `realPath(path) -> Promise<string>`
- `makeTempDir() -> Promise<string>`. Uses `create_dir`, never
  `create_dir_all`: it must fail on an existing path so that a returned
  directory is provably one this process created, and not an
  attacker-planted symlink.
- `symlink(target, path)` / `readLink(path)` / `chmod(path, mode)` — Unix
  only; they reject with "not supported on this platform" elsewhere.
- `cwd() -> string` / `tempDir() -> string` — synchronous.
- `readBytes(path) -> Promise<Bytes>` / `writeBytes(path, bytes)`

## runtime.bytes

Immutable byte buffers as `Foreign::Handle` objects, portable across realms
by shared core.

- `size` / `at` (single byte, `undefined` past the end) / `slice` (clamped) /
  `concat` / `equals` / `indexOf` (naive scan)
- `toString` (lossy UTF-8) / `decode` (strict UTF-8, matching `readFile`) /
  `fromString`
- `toHex` / `fromHex`, `toBase64` / `fromBase64` (RFC 4648, padded),
  `toArray` / `fromArray`

Decoders validate rather than saturate: a bad hex digit, a bad base64
length or padding, or an out-of-range array element raises instead of
silently substituting a zero.

## runtime.path

Pure string manipulation, POSIX semantics, all synchronous — no syscalls.

- `join(...parts)` / `dirname` / `basename` / `extname` / `normalize` /
  `isAbsolute`

`normalize` is lexical: it collapses `.`, `..` and duplicate separators
without consulting the filesystem, so it does not resolve symlinks (that is
`fs.realPath`). A trailing separator is always dropped — one spelling per
path, which is where this diverges from Node.

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
chunked request bodies.

Each worker owns its connections end to end: one kqueue per worker, all
workers watching the same listening socket, and parse → handler →
response on the same thread. There is no per-request channel and no
cross-thread handoff — an earlier design that shipped requests to a
dispatch loop capped out at roughly half this throughput. Connection
fds are registered level-triggered once (re-arming per request costs a
syscall), request parsing is zero-copy into the connection buffer, and
the request object is built from a cached shape.

`workers: 1` (default) turns the calling thread into the event loop, so
the handler keeps running in the caller's realm and can close over
program state. `workers: N` gives each thread its own realm and a
structured clone of the handler — main-realm globals and modules are
not visible inside it, the same rule as parallel.map. Handlers return a
string (200 text) or `{status?, headers?, body?}`; `serve` never
returns.

Measured (`benchmarks/compare/http_bench.py`, which reports CPU as well
as req/s): 142.9k req/s on ~1.1 cores = 128.3k req/s per core, against
bun 124.3k and node 70.5k per core.

Read the per-core column, not the totals: this machine's loopback stack
caps HTTP throughput around 130-160k req/s regardless of engine — four
independent server processes with four independent clients total the
same as one server alone. `workers: 8` buys about +13% for ~2x the CPU
here, so `workers: 1` is the default; multi-worker is for machines
where the network path is not the bottleneck.
