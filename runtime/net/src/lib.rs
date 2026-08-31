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

/// One long-lived I/O thread per connection, draining a job queue —
/// per-connection ops serialize naturally, no spawn per read.
struct ConnCore {
    tx: mpsc::Sender<ConnJob>,
}

enum ConnJob {
    Read(usize, Completer),
    Write(String, Completer),
    Close,
}

fn spawn_conn(stream: TcpStream) -> Arc<ConnCore> {
    let (tx, rx) = mpsc::channel::<ConnJob>();
    std::thread::Builder::new()
        .name("tscore-net-conn".into())
        .spawn(move || {
            let mut stream = stream;
            for job in rx {
                match job {
                    ConnJob::Read(max, c) => {
                        let mut buf = vec![0u8; max.clamp(1, 1 << 20)];
                        match stream.read(&mut buf) {
                            Ok(0) => c.settle(Ok(PortableValue::Undefined)), // EOF
                            Ok(n) => c.settle(Ok(PortableValue::Str(Arc::from(
                                String::from_utf8_lossy(&buf[..n]).as_ref(),
                            )))),
                            Err(e) => c.settle(Err(PromiseError {
                                msg: format!("net.read: {e}"),
                                cancelled: false,
                                span: None,
                            })),
                        }
                    }
                    ConnJob::Write(s, c) => match stream.write_all(s.as_bytes()) {
                        Ok(()) => c.settle(Ok(PortableValue::Undefined)),
                        Err(e) => c.settle(Err(PromiseError {
                            msg: format!("net.write: {e}"),
                            cancelled: false,
                            span: None,
                        })),
                    },
                    ConnJob::Close => return,
                }
            }
        })
        .expect("spawn conn thread");
    Arc::new(ConnCore { tx })
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
        let _ = core.tx.send(ConnJob::Read(max, completer));
        Ok(promise)
    });

    let write = realm.add_native(|realm, args| {
        let core: Arc<ConnCore> = core_of(realm, args.get(realm, 0), CONN_KIND, "net.write")?;
        let s = match args.get(realm, 1).as_str_ref() {
            Some(r) => realm.heap.str_at(r).to_string(),
            None => return Err(RtError::new("net.write: expected a string")),
        };
        let (promise, completer) = realm.promise_pair();
        let _ = core.tx.send(ConnJob::Write(s, completer));
        Ok(promise)
    });

    let close = realm.add_native(|realm, args| {
        let v = args.get(realm, 0);
        if let Ok(core) = core_of::<ConnCore>(realm, v, CONN_KIND, "net.close") {
            let _ = core.tx.send(ConnJob::Close);
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

    eprintln!("tscore http: listening on 127.0.0.1:{port}");
    // dispatch loop: build the request object, run the handler (driving
    // any awaits), ship the reply back to the connection thread
    loop {
        let Ok(req) = req_rx.recv() else {
            return Ok(Value::UNDEFINED);
        };
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
        // body
        let mut body = String::new();
        if content_len > 0 {
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
