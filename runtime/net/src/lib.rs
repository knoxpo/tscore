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
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{mpsc, Arc, Mutex};
use tsr_memory::{Foreign, Obj, PromiseError, Value};
use tsr_realm::{Completer, NativeArgs, Realm, RtError};
use tsr_task::{portable::clone_out, PortableValue};

const LISTENER_KIND: &str = "netListener";
const CONN_KIND: &str = "netConn";

// ---------------- handle plumbing (channel-crate pattern) ----------------

fn make_handle(realm: &mut Realm, kind: &'static str, core: Arc<dyn std::any::Any + Send + Sync>) -> Value {
    let f = realm.heap.alloc_foreign(Foreign::Handle(kind, core));
    let mut obj = Obj::default();
    obj.set(Arc::from(format!("__{kind}")), Value::foreign(f));
    let r = realm.heap.alloc_obj(obj);
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
        Connect(u64, TcpStream, Completer),
        Close(u64),
    }

    struct Conn {
        stream: TcpStream,
        reads: VecDeque<(usize, Completer)>,
        writes: VecDeque<(Vec<u8>, usize, Completer)>,
        connecting: Option<Completer>,
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
                            connecting: None,
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
                    Cmd::Connect(id, stream, c) => {
                        let fd = stream.as_raw_fd();
                        conns.insert(id, Conn {
                            stream,
                            reads: VecDeque::new(),
                            writes: VecDeque::new(),
                            connecting: Some(c),
                        });
                        arm(kq, fd, libc::EVFILT_WRITE, id);
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
                    if let Some(c) = conn.connecting.take() {
                        // connect completion: check SO_ERROR
                        let fd = conn.stream.as_raw_fd();
                        let mut err: libc::c_int = 0;
                        let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
                        unsafe {
                            libc::getsockopt(
                                fd,
                                libc::SOL_SOCKET,
                                libc::SO_ERROR,
                                &mut err as *mut _ as *mut libc::c_void,
                                &mut len,
                            );
                        }
                        if err == 0 {
                            c.settle(Ok(PortableValue::Number(id as f64)));
                        } else {
                            fail(c, format!("net.connect: errno {err}"));
                            conns.remove(&id);
                            continue;
                        }
                    }
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

    let mut net = Obj::default();
    for (k, v) in [
        ("listen", listen),
        ("accept", accept),
        ("read", read),
        ("write", write),
        ("connect", connect),
        ("close", close),
    ] {
        net.set(Arc::from(k), v);
    }
    let net_ref = realm.heap.alloc_obj(net);

    // ---- runtime.http ----
    let serve = realm.add_native(|realm, args| http_serve(realm, args));
    let mut http = Obj::default();
    http.set(Arc::from("serve"), serve);
    let http_ref = realm.heap.alloc_obj(http);

    if let Some(rt) = realm.globals.get("runtime").copied().and_then(|v| v.as_object()) {
        realm.heap.barrier_obj(rt);
        realm.heap.obj_mut(rt).set(Arc::from("net"), Value::object(net_ref));
        realm.heap.obj_mut(rt).set(Arc::from("http"), Value::object(http_ref));
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

struct HttpRequest {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: String,
    reply: mpsc::Sender<(u16, Vec<(String, String)>, String)>,
}

/// `runtime.http.serve(port, handler)`: connection threads parse
/// requests; the handler runs on the realm's event loop (async handlers
/// are driven to completion). Never returns — a server IS the program.
/// ponytail: sequential dispatch on the realm; parallel handler realms
/// are the upgrade path if a benchmark demands it.
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
    let (req_tx, req_rx) = mpsc::channel::<HttpRequest>();

    std::thread::Builder::new()
        .name("tscore-http-accept".into())
        .spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let tx = req_tx.clone();
                std::thread::Builder::new()
                    .name("tscore-http-conn".into())
                    .spawn(move || http_conn(stream, tx))
                    .ok();
            }
        })
        .expect("spawn http acceptor");

    eprintln!("tscore http: listening on 127.0.0.1:{port} ({workers} worker(s))");
    if workers > 1 {
        // parallel dispatch: N handler realms, each with a rehydrated copy
        // of the handler, pulling from the shared queue. Handler realms
        // are isolated (parallel.map rule: only captured state travels).
        let pv = tsr_task::portable::clone_out(&realm.heap, handler)
            .map_err(|e| RtError::new(format!("http.serve: handler not portable: {e}")))?;
        let shared_rx = Arc::new(Mutex::new(req_rx));
        for i in 0..workers {
            let pv = pv.clone();
            let rx = shared_rx.clone();
            std::thread::Builder::new()
                .name(format!("tscore-http-worker-{i}"))
                .spawn(move || {
                    let mut wr = Realm::new();
                    tsr_io_install_min(&mut wr);
                    let h = tsr_task::portable::rehydrate(&pv, &mut wr.heap);
                    loop {
                        let req = {
                            let guard = rx.lock().unwrap();
                            guard.recv()
                        };
                        let Ok(req) = req else { return };
                        dispatch_one(&mut wr, h, req);
                    }
                })
                .expect("spawn http worker");
        }
        // park the calling realm forever (a server IS the program)
        loop {
            std::thread::park();
        }
    }
    // dispatch loop: build the request object, run the handler (driving
    // any awaits), ship the reply back to the connection thread
    loop {
        let Ok(req) = req_rx.recv() else {
            return Ok(Value::UNDEFINED);
        };
        dispatch_one(realm, handler, req);
    }
}

/// Minimal realm setup for http worker realms (console/Math/timers; the
/// handler must not rely on module namespaces or main-realm globals).
fn tsr_io_install_min(realm: &mut Realm) {
    // io installs console/Math/Date/performance/sleep; channel lets
    // handlers talk to the rest of the app
    tsr_io::install(realm);
    tsr_channel::install(realm);
}

fn dispatch_one(realm: &mut Realm, handler: Value, req: HttpRequest) {
    let mut headers = Obj::default();
    for (k, v) in &req.headers {
        headers.set(Arc::from(k.as_str()), realm.alloc_string(v));
    }
    let headers_v = Value::object(realm.heap.alloc_obj(headers));
    let mut ro = Obj::default();
    ro.set(Arc::from("method"), realm.alloc_string(&req.method));
    ro.set(Arc::from("path"), realm.alloc_string(&req.path));
    ro.set(Arc::from("headers"), headers_v);
    ro.set(Arc::from("body"), realm.alloc_string(&req.body));
    let req_v = Value::object(realm.heap.alloc_obj(ro));

    let outcome = tsr_realm::interp::call_value(realm, handler, &[req_v])
        .and_then(|r| tss_parallel::settle_if_promise(realm, r));
    let (status, hdrs, body) = match outcome {
        Ok(v) => response_parts(realm, v),
        Err(e) => (500, Vec::new(), format!("handler error: {}\n", e.msg)),
    };
    let _ = req.reply.send((status, hdrs, body));
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

fn http_conn(stream: TcpStream, tx: mpsc::Sender<HttpRequest>) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
    let mut stream = stream;
    loop {
        // request line
        let mut line = String::new();
        if reader.read_line(&mut line).map(|n| n == 0).unwrap_or(true) {
            return;
        }
        let mut parts = line.split_whitespace();
        let (Some(method), Some(path)) = (parts.next(), parts.next()) else { return };
        let method = method.to_string();
        let path = path.to_string();
        // headers
        let mut headers = Vec::new();
        let mut content_len = 0usize;
        let mut keep_alive = true;
        loop {
            let mut h = String::new();
            if reader.read_line(&mut h).map(|n| n == 0).unwrap_or(true) {
                return;
            }
            let t = h.trim_end();
            if t.is_empty() {
                break;
            }
            if let Some((k, v)) = t.split_once(':') {
                let k = k.trim().to_ascii_lowercase();
                let v = v.trim().to_string();
                if k == "content-length" {
                    content_len = v.parse().unwrap_or(0);
                }
                if k == "connection" && v.eq_ignore_ascii_case("close") {
                    keep_alive = false;
                }
                headers.push((k, v));
            }
        }
        // body: content-length or chunked transfer-encoding
        let mut body = String::new();
        let chunked = headers
            .iter()
            .any(|(k, v)| k == "transfer-encoding" && v.eq_ignore_ascii_case("chunked"));
        if chunked {
            loop {
                let mut szline = String::new();
                if reader.read_line(&mut szline).map(|n| n == 0).unwrap_or(true) {
                    return;
                }
                let sz = usize::from_str_radix(
                    szline.trim().split(';').next().unwrap_or("").trim(),
                    16,
                )
                .unwrap_or(0);
                if sz == 0 {
                    let mut trail = String::new();
                    let _ = reader.read_line(&mut trail); // trailing CRLF
                    break;
                }
                let mut buf = vec![0u8; sz];
                if reader.read_exact(&mut buf).is_err() {
                    return;
                }
                body.push_str(&String::from_utf8_lossy(&buf));
                let mut crlf = String::new();
                let _ = reader.read_line(&mut crlf);
            }
        } else if content_len > 0 {
            let mut buf = vec![0u8; content_len];
            if reader.read_exact(&mut buf).is_err() {
                return;
            }
            body = String::from_utf8_lossy(&buf).into_owned();
        }
        // dispatch + reply
        let (reply_tx, reply_rx) = mpsc::channel();
        if tx
            .send(HttpRequest { method, path, headers, body, reply: reply_tx })
            .is_err()
        {
            return;
        }
        let Ok((status, hdrs, body)) = reply_rx.recv() else { return };
        let mut out = format!(
            "HTTP/1.1 {status} {}\r\ncontent-length: {}\r\n",
            status_text(status),
            body.len()
        );
        for (k, v) in &hdrs {
            out.push_str(&format!("{k}: {v}\r\n"));
        }
        out.push_str("\r\n");
        out.push_str(&body);
        if stream.write_all(out.as_bytes()).is_err() {
            return;
        }
        if !keep_alive {
            return;
        }
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
