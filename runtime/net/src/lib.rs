//! tsr-net
//!
//! `runtime.net` — TCP primitives — and `runtime.http` — a minimal
//! HTTP/1.1 server with keep-alive.
//!
//! Concurrency model v1: one OS thread per connection doing blocking I/O
//! (matches the thread-per-actor precedent; a kqueue/io_uring reactor is
//! the upgrade path). Results reach the realm through the Completer/Wake
//! contract. Handles cross into TS as `Foreign::Handle` objects (the
//! channel-crate pattern), so they also travel between realms by shared
//! core.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{mpsc, Arc, Mutex};
use tsr_memory::{Foreign, PromiseError, Value};
use tsr_realm::{Completer, NativeArgs, Realm, RtError};
use tsr_task::PortableValue;

const LISTENER_KIND: &str = "netListener";
const CONN_KIND: &str = "netConn";

// ---------------- handle plumbing (channel-crate pattern) ----------------

fn make_handle(realm: &mut Realm, kind: &'static str, core: Arc<dyn std::any::Any + Send + Sync>) -> Value {
    let f = realm.heap.alloc_foreign(Foreign::Handle(kind, core));
    let obj = realm.heap.alloc_obj_host();
    realm.heap.obj_set(obj, Arc::from(format!("__{kind}")), Value::foreign(f));
    let r = obj;
    Value::object(r)
}

fn core_of<T: Send + Sync + 'static>(
    realm: &Realm,
    v: Value,
    kind: &'static str,
    who: &str,
) -> Result<Arc<T>, RtError> {
    let err = || RtError::new(format!("{who}: expected a {kind} handle"));
    let obj = v.as_object().ok_or_else(err)?;
    let f = realm
        .heap
        .obj(obj)
        .get(&format!("__{kind}"))
        .and_then(|v| v.as_foreign())
        .ok_or_else(err)?;
    match realm.heap.foreign(f) {
        Foreign::Handle(k, any) if *k == kind => {
            any.clone().downcast::<T>().map_err(|_| err())
        }
        _ => Err(err()),
    }
}

// ---------------- TCP ----------------

struct ListenerCore {
    inner: Mutex<ListenerState>,
}

struct ListenerState {
    pending: VecDeque<TcpStream>,
    waiters: VecDeque<Completer>,
    closed: bool,
}

/// Reactor-backed connection: nonblocking fd, per-conn op queues drained
/// by the single kqueue reactor thread (see `reactor` module).
struct ConnCore {
    id: u64,
}

fn spawn_conn(stream: TcpStream) -> Arc<ConnCore> {
    let id = reactor::register_conn(stream);
    Arc::new(ConnCore { id })
}

mod reactor {
    //! One kqueue reactor thread for every runtime.net connection:
    //! nonblocking sockets, readiness-driven read/write queues, results
    //! through the Completer/Wake contract. Commands arrive on an mpsc
    //! and the thread is woken with an EVFILT_USER event.

    use super::*;
    use std::collections::HashMap;
    use std::os::fd::{AsRawFd, RawFd};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::OnceLock;

    pub enum Cmd {
        Register(u64, TcpStream),
        Read(u64, usize, Completer),
        Write(u64, Vec<u8>, Completer),
        Close(u64),
    }

    struct Conn {
        stream: TcpStream,
        reads: VecDeque<(usize, Completer)>,
        writes: VecDeque<(Vec<u8>, usize, Completer)>,
    }

    struct Shared {
        tx: mpsc::Sender<Cmd>,
        kq: RawFd,
    }

    static NEXT_ID: AtomicU64 = AtomicU64::new(1);
    static SHARED: OnceLock<Shared> = OnceLock::new();
    const WAKE_IDENT: usize = 0x7ac0;

    fn shared() -> &'static Shared {
        SHARED.get_or_init(|| {
            let kq = unsafe { libc::kqueue() };
            assert!(kq >= 0, "kqueue() failed");
            // user event used to wake the reactor when commands arrive
            let ev = libc::kevent {
                ident: WAKE_IDENT,
                filter: libc::EVFILT_USER,
                flags: libc::EV_ADD | libc::EV_CLEAR,
                fflags: 0,
                data: 0,
                udata: std::ptr::null_mut(),
            };
            unsafe {
                libc::kevent(kq, &ev, 1, std::ptr::null_mut(), 0, std::ptr::null())
            };
            let (tx, rx) = mpsc::channel::<Cmd>();
            std::thread::Builder::new()
                .name("tscore-net-reactor".into())
                .spawn(move || run(kq, rx))
                .expect("spawn reactor");
            Shared { tx, kq }
        })
    }

    fn wake() {
        let s = shared();
        let ev = libc::kevent {
            ident: WAKE_IDENT,
            filter: libc::EVFILT_USER,
            flags: 0,
            fflags: libc::NOTE_TRIGGER,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        unsafe {
            libc::kevent(s.kq, &ev, 1, std::ptr::null_mut(), 0, std::ptr::null())
        };
    }

    fn send(cmd: Cmd) {
        let _ = shared().tx.send(cmd);
        wake();
    }

    pub fn register_conn(stream: TcpStream) -> u64 {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        stream.set_nonblocking(true).ok();
        send(Cmd::Register(id, stream));
        id
    }

    pub fn read(id: u64, max: usize, c: Completer) {
        send(Cmd::Read(id, max, c));
    }

    pub fn write(id: u64, buf: Vec<u8>, c: Completer) {
        send(Cmd::Write(id, buf, c));
    }

    pub fn close(id: u64) {
        send(Cmd::Close(id));
    }

    fn arm(kq: RawFd, fd: RawFd, filter: i16, id: u64) {
        let ev = libc::kevent {
            ident: fd as usize,
            filter,
            flags: libc::EV_ADD | libc::EV_ONESHOT,
            fflags: 0,
            data: 0,
            udata: id as *mut libc::c_void,
        };
        unsafe {
            libc::kevent(kq, &ev, 1, std::ptr::null_mut(), 0, std::ptr::null())
        };
    }

    fn fail(c: Completer, msg: String) {
        c.settle(Err(PromiseError { msg, cancelled: false, span: None, source: None }));
    }

    fn run(kq: RawFd, rx: mpsc::Receiver<Cmd>) {
        let mut conns: HashMap<u64, Conn> = HashMap::new();
        let mut events = vec![
            libc::kevent {
                ident: 0,
                filter: 0,
                flags: 0,
                fflags: 0,
                data: 0,
                udata: std::ptr::null_mut(),
            };
            64
        ];
        loop {
            // drain commands
            while let Ok(cmd) = rx.try_recv() {
                match cmd {
                    Cmd::Register(id, stream) => {
                        conns.insert(id, Conn {
                            stream,
                            reads: VecDeque::new(),
                            writes: VecDeque::new(),
                        });
                    }
                    Cmd::Read(id, max, c) => {
                        if let Some(conn) = conns.get_mut(&id) {
                            conn.reads.push_back((max, c));
                            pump_read(kq, id, conn);
                        } else {
                            fail(c, "net.read: connection closed".into());
                        }
                    }
                    Cmd::Write(id, buf, c) => {
                        if let Some(conn) = conns.get_mut(&id) {
                            conn.writes.push_back((buf, 0, c));
                            pump_write(kq, id, conn);
                        } else {
                            fail(c, "net.write: connection closed".into());
                        }
                    }
                    Cmd::Close(id) => {
                        conns.remove(&id); // drop closes the fd
                    }
                }
            }
            // wait for readiness
            let n = unsafe {
                libc::kevent(
                    kq,
                    std::ptr::null(),
                    0,
                    events.as_mut_ptr(),
                    events.len() as i32,
                    std::ptr::null(),
                )
            };
            for ev in events.iter().take(n.max(0) as usize) {
                if ev.filter == libc::EVFILT_USER {
                    continue; // command wakeup: handled at loop top
                }
                let id = ev.udata as u64;
                let Some(conn) = conns.get_mut(&id) else { continue };
                if ev.filter == libc::EVFILT_READ {
                    pump_read(kq, id, conn);
                } else if ev.filter == libc::EVFILT_WRITE {
                    pump_write(kq, id, conn);
                }
            }
        }
    }

    /// Serve queued reads while the socket has data; re-arm on WouldBlock.
    fn pump_read(kq: RawFd, id: u64, conn: &mut Conn) {
        while let Some((max, _)) = conn.reads.front() {
            let mut buf = vec![0u8; (*max).clamp(1, 1 << 20)];
            match conn.stream.read(&mut buf) {
                Ok(0) => {
                    let (_, c) = conn.reads.pop_front().unwrap();
                    c.settle(Ok(PortableValue::Undefined)); // EOF
                }
                Ok(n) => {
                    let (_, c) = conn.reads.pop_front().unwrap();
                    c.settle(Ok(PortableValue::Str(Arc::from(
                        String::from_utf8_lossy(&buf[..n]).as_ref(),
                    ))));
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    arm(kq, conn.stream.as_raw_fd(), libc::EVFILT_READ, id);
                    return;
                }
                Err(e) => {
                    let (_, c) = conn.reads.pop_front().unwrap();
                    fail(c, format!("net.read: {e}"));
                }
            }
        }
    }

    fn pump_write(kq: RawFd, id: u64, conn: &mut Conn) {
        while let Some((buf, off, _)) = conn.writes.front_mut() {
            match conn.stream.write(&buf[*off..]) {
                Ok(n) => {
                    *off += n;
                    if *off >= buf.len() {
                        let (_, _, c) = conn.writes.pop_front().unwrap();
                        c.settle(Ok(PortableValue::Undefined));
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    arm(kq, conn.stream.as_raw_fd(), libc::EVFILT_WRITE, id);
                    return;
                }
                Err(e) => {
                    let (_, _, c) = conn.writes.pop_front().unwrap();
                    fail(c, format!("net.write: {e}"));
                }
            }
        }
    }
}

pub fn install(realm: &mut Realm) {
    // ---- runtime.net ----
    let listen = realm.add_native(|realm, args| {
        let port = match args.get(realm, 0).kind() {
            tsr_memory::Kind::Number(n) => n as u16,
            _ => {
                // {port: n} object form
                let v = args.get(realm, 0);
                let o = v
                    .as_object()
                    .ok_or_else(|| RtError::new("net.listen: expected port or {port}"))?;
                realm
                    .heap
                    .obj(o)
                    .get("port")
                    .filter(|v| v.is_number())
                    .map(|v| v.as_number() as u16)
                    .ok_or_else(|| RtError::new("net.listen: expected {port: number}"))?
            }
        };
        let listener = TcpListener::bind(("127.0.0.1", port))
            .map_err(|e| RtError::new(format!("net.listen: {e}")))?;
        let core = Arc::new(ListenerCore {
            inner: Mutex::new(ListenerState {
                pending: VecDeque::new(),
                waiters: VecDeque::new(),
                closed: false,
            }),
        });
        let acceptor = core.clone();
        std::thread::Builder::new()
            .name("tscore-net-accept".into())
            .spawn(move || {
                for stream in listener.incoming() {
                    let Ok(stream) = stream else { continue };
                    let mut st = acceptor.inner.lock().unwrap();
                    if st.closed {
                        return;
                    }
                    if let Some(w) = st.waiters.pop_front() {
                        let conn = spawn_conn(stream);
                        w.settle(Ok(PortableValue::Handle(CONN_KIND, conn)));
                    } else {
                        st.pending.push_back(stream);
                    }
                }
            })
            .expect("spawn acceptor");
        Ok(make_handle(realm, LISTENER_KIND, core))
    });

    let accept = realm.add_native(|realm, args| {
        let core: Arc<ListenerCore> =
            core_of(realm, args.get(realm, 0), LISTENER_KIND, "net.accept")?;
        let (promise, completer) = realm.promise_pair();
        let mut st = core.inner.lock().unwrap();
        if let Some(stream) = st.pending.pop_front() {
            let conn = spawn_conn(stream);
            completer.settle(Ok(PortableValue::Handle(CONN_KIND, conn)));
        } else {
            st.waiters.push_back(completer);
        }
        Ok(promise)
    });

    let read = realm.add_native(|realm, args| {
        let core: Arc<ConnCore> = core_of(realm, args.get(realm, 0), CONN_KIND, "net.read")?;
        let max = match args.get(realm, 1).kind() {
            tsr_memory::Kind::Number(n) if n > 0.0 => n as usize,
            _ => 64 * 1024,
        };
        let (promise, completer) = realm.promise_pair();
        reactor::read(core.id, max, completer);
        Ok(promise)
    });

    let write = realm.add_native(|realm, args| {
        let core: Arc<ConnCore> = core_of(realm, args.get(realm, 0), CONN_KIND, "net.write")?;
        let s = match args.get(realm, 1).as_str_ref() {
            Some(r) => realm.heap.str_at(r).to_string(),
            None => return Err(RtError::new("net.write: expected a string")),
        };
        let (promise, completer) = realm.promise_pair();
        reactor::write(core.id, s.into_bytes(), completer);
        Ok(promise)
    });

    let connect = realm.add_native(|realm, args| {
        let host = match args.get(realm, 0).as_str_ref() {
            Some(r) => realm.heap.str_at(r).to_string(),
            None => return Err(RtError::new("net.connect(host, port)")),
        };
        let port = match args.get(realm, 1).kind() {
            tsr_memory::Kind::Number(n) => n as u16,
            _ => return Err(RtError::new("net.connect(host, port)")),
        };
        let (promise, completer) = realm.promise_pair();
        std::thread::Builder::new()
            .name("tscore-net-connect".into())
            .spawn(move || match TcpStream::connect((host.as_str(), port)) {
                Ok(stream) => {
                    let conn = spawn_conn(stream);
                    completer.settle(Ok(PortableValue::Handle(CONN_KIND, conn)));
                }
                Err(e) => completer.settle(Err(PromiseError {
                    msg: format!("net.connect {host}:{port}: {e}"),
                    cancelled: false,
                    span: None,
                    source: None,
                })),
            })
            .expect("spawn connect thread");
        Ok(promise)
    });

    let close = realm.add_native(|realm, args| {
        let v = args.get(realm, 0);
        if let Ok(core) = core_of::<ConnCore>(realm, v, CONN_KIND, "net.close") {
            reactor::close(core.id);
            return Ok(Value::UNDEFINED);
        }
        if let Ok(core) = core_of::<ListenerCore>(realm, v, LISTENER_KIND, "net.close") {
            core.inner.lock().unwrap().closed = true;
            return Ok(Value::UNDEFINED);
        }
        Err(RtError::new("net.close: expected a net handle"))
    });

    let net = realm.heap.alloc_obj_host();
    for (k, v) in [
        ("listen", listen),
        ("accept", accept),
        ("read", read),
        ("write", write),
        ("connect", connect),
        ("close", close),
    ] {
        realm.heap.obj_set(net, Arc::from(k), v);
    }
    let net_ref = net;

    // ---- runtime.http ----
    let serve = realm.add_native(|realm, args| http_serve(realm, args));
    let http = realm.heap.alloc_obj_host();
    realm.heap.obj_set(http, Arc::from("serve"), serve);
    let http_ref = http;

    if let Some(rt) = realm.globals.get("runtime").copied().and_then(|v| v.as_object()) {
        realm.heap.barrier_obj(rt);
        realm.heap.obj_set(rt, Arc::from("net"), Value::object(net_ref));
        realm.heap.obj_set(rt, Arc::from("http"), Value::object(http_ref));
    } else {
        realm.set_global_obj(
            "runtime",
            vec![
                ("net", Value::object(net_ref)),
                ("http", Value::object(http_ref)),
            ],
        );
    }
}

// ---------------- HTTP/1.1 server ----------------
//
// Each worker owns its connections end to end: one kqueue per worker,
// all workers watching the same listening socket, and parse → handler →
// response all on the same thread. There is no per-request channel and
// no cross-thread handoff — those cost more than the work itself
// (measured: the previous hand-off design capped out around half this
// throughput).

use std::os::fd::{AsRawFd, RawFd};

/// Per-connection state: an input buffer that keeps whatever a partial
/// read left behind, and an output buffer for a partially-written
/// response. Both are reused for the connection's lifetime.
struct HttpConn {
    stream: TcpStream,
    inbuf: Vec<u8>,
    outbuf: Vec<u8>,
    out_off: usize,
    keep_alive: bool,
}

/// A parsed request as ranges into the connection's input buffer — the
/// hot path allocates nothing here; realm strings are built directly
/// from the slices when the request object is constructed.
struct ParsedRequest {
    method: Range,
    path: Range,
    headers: Vec<(Range, Range)>,
    /// Owned only when the body arrived chunked (it must be reassembled).
    body: BodyRef,
    keep_alive: bool,
    /// Bytes of `inbuf` this request consumed.
    consumed: usize,
}

type Range = (usize, usize);

enum BodyRef {
    Slice(Range),
    Owned(String),
}

impl BodyRef {
    fn as_str<'a>(&'a self, buf: &'a [u8]) -> &'a str {
        match self {
            BodyRef::Slice((s, e)) => {
                std::str::from_utf8(&buf[*s..*e]).unwrap_or("")
            }
            BodyRef::Owned(s) => s,
        }
    }
}

fn slice_str<'a>(buf: &'a [u8], r: Range) -> &'a str {
    std::str::from_utf8(&buf[r.0..r.1]).unwrap_or("")
}

/// Parse one request out of `buf`. None = need more bytes.
fn parse_request(buf: &[u8]) -> Option<ParsedRequest> {
    let head_end = find_headers_end(buf)?;
    // request line
    let line_end = buf.windows(2).position(|w| w == b"\r\n")?;
    let line = &buf[..line_end];
    let sp1 = line.iter().position(|&b| b == b' ')?;
    let rest = &line[sp1 + 1..];
    let sp2 = rest.iter().position(|&b| b == b' ')?;
    let method = (0, sp1);
    let path = (sp1 + 1, sp1 + 1 + sp2);

    let mut headers = Vec::with_capacity(8);
    let mut content_len = 0usize;
    let mut chunked = false;
    let mut keep_alive = true;
    let mut pos = line_end + 2;
    while pos + 1 < head_end {
        let rel = buf[pos..head_end].windows(2).position(|w| w == b"\r\n")?;
        if rel == 0 {
            break; // blank line: end of headers
        }
        let end = pos + rel;
        if let Some(c) = buf[pos..end].iter().position(|&b| b == b':') {
            let k = (pos, pos + c);
            let mut vs = pos + c + 1;
            while vs < end && buf[vs] == b' ' {
                vs += 1;
            }
            let kb = &buf[k.0..k.1];
            // only the three headers that steer framing are inspected;
            // everything else is passed through untouched
            if kb.eq_ignore_ascii_case(b"content-length") {
                content_len = slice_str(buf, (vs, end)).trim().parse().unwrap_or(0);
            } else if kb.eq_ignore_ascii_case(b"transfer-encoding")
                && slice_str(buf, (vs, end)).trim().eq_ignore_ascii_case("chunked")
            {
                chunked = true;
            } else if kb.eq_ignore_ascii_case(b"connection")
                && slice_str(buf, (vs, end)).trim().eq_ignore_ascii_case("close")
            {
                keep_alive = false;
            }
            headers.push((k, (vs, end)));
        }
        pos = end + 2;
    }
    if chunked {
        let (body, used) = parse_chunked(&buf[head_end..])?;
        return Some(ParsedRequest {
            method,
            path,
            headers,
            body: BodyRef::Owned(body),
            keep_alive,
            consumed: head_end + used,
        });
    }
    if buf.len() < head_end + content_len {
        return None; // body still arriving
    }
    Some(ParsedRequest {
        method,
        path,
        headers,
        body: BodyRef::Slice((head_end, head_end + content_len)),
        keep_alive,
        consumed: head_end + content_len,
    })
}

fn find_headers_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

/// Decode a chunked body; returns (body, bytes consumed). None = partial.
fn parse_chunked(buf: &[u8]) -> Option<(String, usize)> {
    let mut out = Vec::new();
    let mut i = 0usize;
    loop {
        let nl = buf[i..].windows(2).position(|w| w == b"\r\n")? + i;
        let size_txt = std::str::from_utf8(&buf[i..nl]).ok()?;
        let size = usize::from_str_radix(
            size_txt.split(';').next().unwrap_or("").trim(),
            16,
        )
        .ok()?;
        i = nl + 2;
        if size == 0 {
            // trailing CRLF after the terminator
            if buf.len() < i + 2 {
                return None;
            }
            return Some((String::from_utf8_lossy(&out).into_owned(), i + 2));
        }
        if buf.len() < i + size + 2 {
            return None;
        }
        out.extend_from_slice(&buf[i..i + size]);
        i += size + 2;
    }
}

fn status_text(s: u16) -> &'static str {
    match s {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "",
    }
}

fn response_parts(realm: &Realm, v: Value) -> (u16, Vec<(String, String)>, String) {
    // string result = 200 text; object = {status?, headers?, body?}
    if let Some(r) = v.as_str_ref() {
        return (200, Vec::new(), realm.heap.str_at(r).to_string());
    }
    if let Some(o) = v.as_object() {
        let obj = realm.heap.obj(o);
        let status = obj
            .get("status")
            .filter(|v| v.is_number())
            .map(|v| v.as_number() as u16)
            .unwrap_or(200);
        let body = obj
            .get("body")
            .and_then(|v| v.as_str_ref())
            .map(|r| realm.heap.str_at(r).to_string())
            .unwrap_or_default();
        let mut hdrs = Vec::new();
        if let Some(hv) = obj.get("headers").and_then(|v| v.as_object()) {
            for (k, v) in realm.heap.obj(hv).entries() {
                if let Some(sr) = v.as_str_ref() {
                    hdrs.push((k.to_string(), realm.heap.str_at(sr).to_string()));
                }
            }
        }
        return (status, hdrs, body);
    }
    (200, Vec::new(), String::new())
}

/// Header names repeat on every request, so `Arc::from(name)` was one
/// malloc per header per request, paid even by handlers that never read
/// a header. Interning makes it one malloc per distinct name per worker.
///
/// Names are worth ~2% throughput at 12 headers/request and ~3% at 18,
/// nothing at 2. Values are what move memory: `handle_request` builds
/// the request object eagerly whether or not the handler ever looks at
/// it, so a string per header value per request was the bulk of the
/// server's RSS under load. Interning them roughly halves it (median
/// 397MB -> 222MB at 18 headers/request over 5 paired runs) at no
/// measurable throughput cost.
///
/// This is a mitigation, not the cure. The request object is still
/// materialised eagerly; a handler that never reads a header still pays
/// to build one. The cure is lazy properties, which this object model
/// has no room for: the optimizing compiler lowers GetField to a bare slot load with
/// no sentinel or accessor check, so making a field lazy would put a
/// branch on the hottest opcode in the runtime to serve one subsystem.
///
/// Bounded because the name is attacker-chosen: a peer sending random
/// header names would otherwise grow this map without limit. Past the
/// cap we stop inserting and fall back to allocating, so the behaviour
/// is unchanged and only the speedup is lost.
struct HeaderNames {
    map: std::collections::HashMap<Box<str>, Arc<str>>,
    values: std::collections::HashMap<Box<str>, Value>,
}

/// Enough for the real header vocabulary (~100 standard + app-specific)
/// with room to spare; small enough that a hostile peer cannot grow the
/// map into a memory problem.
const HEADER_NAME_CAP: usize = 1024;

/// Interned values are pinned as GC roots for the worker's lifetime and
/// every root is re-scanned on every collection, so this cap is the one
/// that matters: it bounds both the retained bytes and the root-scan
/// cost. 512 covers the values that actually repeat (user-agent, accept,
/// accept-encoding, host...); per-user values like cookies will fill the
/// rest and then fall through to plain allocation, which is exactly the
/// behaviour we had before.
const HEADER_VALUE_CAP: usize = 512;

/// Long values are the ones least likely to repeat across clients
/// (cookies, bearer tokens) and the most expensive to retain forever, so
/// they are never interned.
const HEADER_VALUE_MAX_LEN: usize = 128;

impl HeaderNames {
    fn new() -> Self {
        Self {
            map: std::collections::HashMap::new(),
            values: std::collections::HashMap::new(),
        }
    }

    fn intern(&mut self, name: &str) -> Arc<str> {
        if let Some(a) = self.map.get(name) {
            return a.clone();
        }
        let a: Arc<str> = Arc::from(name);
        if self.map.len() < HEADER_NAME_CAP {
            self.map.insert(Box::from(name), a.clone());
        }
        a
    }

    /// A header value as a JS string, reusing the heap string when this
    /// worker has seen the same bytes before.
    ///
    /// Safe to share because JS strings are immutable and this runtime
    /// exposes no in-place mutation: a handler cannot reach through one
    /// request's header value and change what a later request sees.
    ///
    /// The string is promoted straight to the old generation and pinned
    /// in `const_roots`, so it neither moves under the copying nursery
    /// nor gets collected while the cache still points at it.
    fn intern_value(&mut self, realm: &mut Realm, s: &str) -> Value {
        if let Some(v) = self.values.get(s) {
            return *v;
        }
        if s.len() > HEADER_VALUE_MAX_LEN || self.values.len() >= HEADER_VALUE_CAP {
            return realm.alloc_string(s);
        }
        let v = Value::str_ref(
            realm.heap.promote_str(tsr_memory::HStr::Shared(Arc::from(s))),
        );
        realm.const_roots.push(v);
        self.values.insert(Box::from(s), v);
        v
    }
}

/// Run the handler for one parsed request and append the wire response
/// to `out`. Everything happens on the worker's own thread and realm.
fn handle_request(
    realm: &mut Realm,
    handler: Value,
    req: &ParsedRequest,
    buf: &[u8],
    out: &mut Vec<u8>,
    names: &mut HeaderNames,
) {
    // request shape is fixed: build it once per process and allocate the
    // object in one shot (per-field `set` would walk shape transitions
    // on every request)
    static REQ_SHAPE: std::sync::OnceLock<&'static tsr_memory::ShapeData> =
        std::sync::OnceLock::new();
    let shape = *REQ_SHAPE.get_or_init(|| {
        let mut s = tsr_memory::empty_shape();
        for k in ["method", "path", "headers", "body"] {
            s = s.with_field(Arc::from(k));
        }
        s
    });
    let headers = realm.heap.alloc_obj_host();
    let mut lower = [0u8; 64];
    for (k, v) in &req.headers {
        // header names are exposed lowercased (what handlers expect);
        // normal names fit the stack buffer, so this allocates nothing
        let raw = &buf[k.0..k.1];
        let name: &str = if raw.len() <= lower.len() {
            for (i, b) in raw.iter().enumerate() {
                lower[i] = b.to_ascii_lowercase();
            }
            std::str::from_utf8(&lower[..raw.len()]).unwrap_or("")
        } else {
            slice_str(buf, *k)
        };
        let val = names.intern_value(realm, slice_str(buf, *v));
        realm.heap.obj_set(headers, names.intern(name), val);
    }
    let headers_v = Value::object(headers);
    let vals = [
        realm.alloc_string(slice_str(buf, req.method)),
        realm.alloc_string(slice_str(buf, req.path)),
        headers_v,
        realm.alloc_string(req.body.as_str(buf)),
    ];
    let req_v = Value::object(realm.heap.alloc_obj_lit(shape, &vals));

    let outcome = tsr_realm::interpreter::call_value(realm, handler, &[req_v])
        .and_then(|r| tss_parallel::settle_if_promise(realm, r));
    let (status, hdrs, body) = match outcome {
        Ok(v) => response_parts(realm, v),
        Err(e) => (500, Vec::new(), format!("handler error: {}\n", e.msg)),
    };

    // status line + headers, written straight into the output buffer
    out.extend_from_slice(b"HTTP/1.1 ");
    let mut num = [0u8; 8];
    let n = fmt_u16(status, &mut num);
    out.extend_from_slice(&num[..n]);
    out.push(b' ');
    out.extend_from_slice(status_text(status).as_bytes());
    out.extend_from_slice(b"\r\ncontent-length: ");
    let n = fmt_usize(body.len(), &mut num);
    out.extend_from_slice(&num[..n]);
    out.extend_from_slice(b"\r\n");
    if !req.keep_alive {
        out.extend_from_slice(b"connection: close\r\n");
    }
    for (k, v) in &hdrs {
        out.extend_from_slice(k.as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(v.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(body.as_bytes());
}

fn fmt_u16(v: u16, buf: &mut [u8; 8]) -> usize {
    fmt_usize(v as usize, buf)
}

fn fmt_usize(mut v: usize, buf: &mut [u8; 8]) -> usize {
    if v == 0 {
        buf[0] = b'0';
        return 1;
    }
    let mut tmp = [0u8; 20];
    let mut i = 0;
    while v > 0 {
        tmp[i] = b'0' + (v % 10) as u8;
        v /= 10;
        i += 1;
    }
    let n = i.min(8);
    for j in 0..n {
        buf[j] = tmp[i - 1 - j];
    }
    n
}

/// `runtime.http.serve(port, handler, {workers?})`.
///
/// workers = 1 (default): the calling thread becomes the event loop and
/// the handler runs in the caller's realm, so it can close over program
/// state. workers > 1: N threads, each with its own realm and a
/// structured clone of the handler (same isolation rule as parallel.map);
/// the caller parks. Never returns.
fn http_serve(realm: &mut Realm, args: NativeArgs) -> Result<Value, RtError> {
    let port = match args.get(realm, 0).kind() {
        tsr_memory::Kind::Number(n) => n as u16,
        _ => return Err(RtError::new("http.serve(port, handler)")),
    };
    let handler = args.get(realm, 1);
    if handler.as_closure().is_none() {
        return Err(RtError::new("http.serve: handler must be a function"));
    }
    let workers = args
        .get(realm, 2)
        .as_object()
        .and_then(|o| realm.heap.obj(o).get("workers"))
        .filter(|v| v.is_number())
        .map(|v| (v.as_number() as usize).max(1))
        .unwrap_or(1);
    let listener = TcpListener::bind(("127.0.0.1", port))
        .map_err(|e| RtError::new(format!("http.serve: {e}")))?;
    listener.set_nonblocking(true).ok();
    eprintln!("tscore http: listening on 127.0.0.1:{port} ({workers} worker(s))");

    if workers == 1 {
        worker_loop(realm, handler, &listener);
        return Ok(Value::UNDEFINED);
    }

    let pv = tsr_task::portable::clone_out(&realm.heap, handler)
        .map_err(|e| RtError::new(format!("http.serve: handler not portable: {e}")))?;
    let listener = Arc::new(listener);
    for i in 0..workers {
        let pv = pv.clone();
        let listener = listener.clone();
        std::thread::Builder::new()
            .name(format!("tscore-http-{i}"))
            .stack_size(16 << 20)
            .spawn(move || {
                let mut wr = Realm::new();
                tsr_io::install(&mut wr);
                tsr_channel::install(&mut wr);
                let h = tsr_task::portable::rehydrate(&pv, &mut wr.heap);
                worker_loop(&mut wr, h, &listener);
            })
            .expect("spawn http worker");
    }
    loop {
        std::thread::park();
    }
}

/// One worker: a kqueue watching the shared listener plus every
/// connection this worker accepted. All parsing, handler execution and
/// writing happen here — no channels, no handoff.
fn worker_loop(realm: &mut Realm, handler: Value, listener: &TcpListener) {
    use std::collections::HashMap;
    let kq = unsafe { libc::kqueue() };
    assert!(kq >= 0, "kqueue() failed");
    let lfd = listener.as_raw_fd();
    arm_read(kq, lfd, u64::MAX);

    let mut conns: HashMap<RawFd, HttpConn> = HashMap::new();
    let mut events = vec![
        libc::kevent {
            ident: 0,
            filter: 0,
            flags: 0,
            fflags: 0,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        256
    ];
    let mut scratch = vec![0u8; 64 * 1024];
    let mut names = HeaderNames::new();

    loop {
        let n = unsafe {
            libc::kevent(
                kq,
                std::ptr::null(),
                0,
                events.as_mut_ptr(),
                events.len() as i32,
                std::ptr::null(),
            )
        };
        if n < 0 {
            continue;
        }
        for ev in events.iter().take(n as usize) {
            let fd = ev.ident as RawFd;
            if fd == lfd {
                // drain the accept queue; other workers race us and lose
                // harmlessly with EWOULDBLOCK
                loop {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            stream.set_nodelay(true).ok();
                            stream.set_nonblocking(true).ok();
                            let cfd = stream.as_raw_fd();
                            conns.insert(cfd, HttpConn {
                                stream,
                                inbuf: Vec::with_capacity(2048),
                                outbuf: Vec::with_capacity(2048),
                                out_off: 0,
                                keep_alive: true,
                            });
                            arm_read(kq, cfd, cfd as u64);
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(_) => break,
                    }
                }
                continue; // level-triggered: still registered
            }
            if ev.filter == libc::EVFILT_WRITE {
                if !flush_out(kq, fd, conns.get_mut(&fd)) {
                    conns.remove(&fd);
                }
                continue;
            }
            if !conns.contains_key(&fd) {
                continue;
            }
            // read everything available, serving each complete request
            let mut alive = true;
            {
                let conn = conns.get_mut(&fd).unwrap();
                loop {
                    match conn.stream.read(&mut scratch) {
                        Ok(0) => {
                            alive = false;
                            break;
                        }
                        Ok(k) => conn.inbuf.extend_from_slice(&scratch[..k]),
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(_) => {
                            alive = false;
                            break;
                        }
                    }
                }
            }
            while alive {
                // take the buffers out so the handler can borrow the realm
                let (mut inbuf, mut out) = {
                    let conn = conns.get_mut(&fd).unwrap();
                    (std::mem::take(&mut conn.inbuf), std::mem::take(&mut conn.outbuf))
                };
                let parsed = parse_request(&inbuf);
                let Some(req) = parsed else {
                    let conn = conns.get_mut(&fd).unwrap();
                    conn.inbuf = inbuf;
                    conn.outbuf = out;
                    break;
                };
                handle_request(realm, handler, &req, &inbuf, &mut out, &mut names);
                let (consumed, keep) = (req.consumed, req.keep_alive);
                drop(req);
                inbuf.drain(..consumed);
                let conn = conns.get_mut(&fd).unwrap();
                conn.inbuf = inbuf;
                conn.outbuf = out;
                conn.keep_alive = keep;
                if !keep {
                    alive = false;
                }
            }
            let closed = !alive;
            if !flush_out(kq, fd, conns.get_mut(&fd)) || closed {
                conns.remove(&fd); // drop closes the fd, removing it from kq
            }
        }
    }
}

/// Register a fd for readability once, level-triggered: the loop drains
/// to WouldBlock each time, so re-arming per request (EV_ONESHOT) would
/// just be an extra syscall on the hot path.
fn arm_read(kq: RawFd, fd: RawFd, udata: u64) {
    let ev = libc::kevent {
        ident: fd as usize,
        filter: libc::EVFILT_READ,
        flags: libc::EV_ADD,
        fflags: 0,
        data: 0,
        udata: udata as *mut libc::c_void,
    };
    unsafe { libc::kevent(kq, &ev, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
}

fn arm_write(kq: RawFd, fd: RawFd) {
    let ev = libc::kevent {
        ident: fd as usize,
        filter: libc::EVFILT_WRITE,
        flags: libc::EV_ADD | libc::EV_ONESHOT,
        fflags: 0,
        data: 0,
        udata: fd as *mut libc::c_void,
    };
    unsafe { libc::kevent(kq, &ev, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
}

/// Write as much of the pending response as the socket accepts. Returns
/// false when the connection is finished (error, or a completed
/// non-keep-alive response).
fn flush_out(kq: RawFd, fd: RawFd, conn: Option<&mut HttpConn>) -> bool {
    let Some(conn) = conn else { return false };
    while conn.out_off < conn.outbuf.len() {
        match conn.stream.write(&conn.outbuf[conn.out_off..]) {
            Ok(0) => return false,
            Ok(n) => conn.out_off += n,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                arm_write(kq, fd);
                return true;
            }
            Err(_) => return false,
        }
    }
    conn.outbuf.clear();
    conn.out_off = 0;
    conn.keep_alive
}

#[cfg(test)]
mod header_name_tests {
    use super::{HeaderNames, HEADER_NAME_CAP, HEADER_VALUE_CAP, HEADER_VALUE_MAX_LEN};

    #[test]
    fn interns_repeats_and_stays_bounded() {
        let mut n = HeaderNames::new();

        // the point of the cache: a repeated name reuses one allocation
        let a = n.intern("content-type");
        let b = n.intern("content-type");
        assert!(std::sync::Arc::ptr_eq(&a, &b));
        assert_eq!(&*a, "content-type");

        // distinct names stay distinct
        assert_eq!(&*n.intern("accept"), "accept");
        assert_eq!(n.map.len(), 2);

        // a hostile peer sending unique names cannot grow the map past
        // the cap, and names past it still return the right string
        for i in 0..HEADER_NAME_CAP * 2 {
            let name = format!("x-{i}");
            assert_eq!(&*n.intern(&name), name);
        }
        assert_eq!(n.map.len(), HEADER_NAME_CAP);
    }

    #[test]
    fn value_interning_reuses_shares_and_stays_bounded() {
        let mut realm = tsr_realm::Realm::new();
        let mut n = HeaderNames::new();

        // a repeated value is the same heap string, not a fresh copy
        let a = n.intern_value(&mut realm, "gzip, deflate, br");
        let b = n.intern_value(&mut realm, "gzip, deflate, br");
        assert_eq!(a.bits(), b.bits());

        // ...and interning must not merge values that merely look close
        let c = n.intern_value(&mut realm, "gzip, deflate");
        assert_ne!(a.bits(), c.bits());
        assert_eq!(realm.heap.str_at(c.as_str_ref().unwrap()), "gzip, deflate");

        // every interned value is pinned, or the copying nursery would
        // move the string out from under the cache
        assert_eq!(realm.const_roots.len(), n.values.len());

        // long values are never interned: each call is its own string
        let long = "x".repeat(HEADER_VALUE_MAX_LEN + 1);
        let l1 = n.intern_value(&mut realm, &long);
        let l2 = n.intern_value(&mut realm, &long);
        assert_ne!(l1.bits(), l2.bits());
        assert_eq!(realm.heap.str_at(l1.as_str_ref().unwrap()), long);

        // a peer sending unique values cannot grow the cache past the
        // cap, and values past it still read back correctly
        for i in 0..HEADER_VALUE_CAP * 2 {
            let v = format!("v-{i}");
            let got = n.intern_value(&mut realm, &v);
            assert_eq!(realm.heap.str_at(got.as_str_ref().unwrap()), v);
        }
        assert_eq!(n.values.len(), HEADER_VALUE_CAP);
    }
}
