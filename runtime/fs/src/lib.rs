//! tsr-fs
//!
//! `runtime.fs.*` — promise-native filesystem API. Blocking syscalls run
//! on a small dedicated I/O thread pool (libuv model — NOT the CPU
//! scheduler pool, which must never block); results come back through the
//! Completer/Wake contract, exactly like the timer wheel.
//!
//! v1 is UTF-8 strings end to end (the Value model has no bytes type yet;
//! a Bytes handle is the upgrade path for binary files).

use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use tsr_memory::{PromiseError, Value};
use tsr_realm::{NativeArgs, Realm, RtError};
use tsr_task::PortableValue;

type Job = Box<dyn FnOnce() + Send + 'static>;

const IO_THREADS: usize = 4;

/// Process-wide blocking-I/O pool: N threads draining one queue.
fn io_submit(job: Job) {
    static TX: OnceLock<Mutex<mpsc::Sender<Job>>> = OnceLock::new();
    let tx = TX.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<Job>();
        let rx = Arc::new(Mutex::new(rx));
        for i in 0..IO_THREADS {
            let rx = rx.clone();
            std::thread::Builder::new()
                .name(format!("tscore-io-{i}"))
                .spawn(move || loop {
                    let job = {
                        let guard = rx.lock().unwrap();
                        guard.recv()
                    };
                    match job {
                        Ok(j) => j(),
                        Err(_) => return,
                    }
                })
                .expect("spawn io thread");
        }
        Mutex::new(tx)
    });
    let _ = tx.lock().unwrap().send(job);
}

fn path_arg(realm: &Realm, args: NativeArgs, i: usize, who: &str) -> Result<PathBuf, RtError> {
    match args.get(realm, i).as_str_ref() {
        Some(r) => Ok(PathBuf::from(realm.heap.str_at(r))),
        None => Err(RtError::new(format!("{who}: expected a path string"))),
    }
}

fn str_arg(realm: &Realm, args: NativeArgs, i: usize, who: &str) -> Result<String, RtError> {
    match args.get(realm, i).as_str_ref() {
        Some(r) => Ok(realm.heap.str_at(r).to_string()),
        None => Err(RtError::new(format!("{who}: expected a string"))),
    }
}

/// Run `op` on the I/O pool; settle the returned promise with its result.
fn async_op(
    realm: &mut Realm,
    op: impl FnOnce() -> Result<PortableValue, String> + Send + 'static,
) -> Value {
    let (promise, completer) = realm.promise_pair();
    io_submit(Box::new(move || match op() {
        Ok(v) => completer.settle(Ok(v)),
        Err(msg) => completer.settle(Err(PromiseError {
            msg,
            cancelled: false,
            span: None,
            source: None,
        })),
    }));
    promise
}

const BYTES_KIND: &str = "bytes";

fn make_bytes(realm: &mut Realm, data: Arc<Vec<u8>>) -> Value {
    let f = realm
        .heap
        .alloc_foreign(tsr_memory::Foreign::Handle(BYTES_KIND, data));
    let mut obj = tsr_memory::Obj::default();
    obj.set(Arc::from("__bytes"), Value::foreign(f));
    Value::object(realm.heap.alloc_obj(obj))
}

fn bytes_of(realm: &Realm, v: Value, who: &str) -> Result<Arc<Vec<u8>>, RtError> {
    let err = || RtError::new(format!("{who}: expected a Bytes handle"));
    let obj = v.as_object().ok_or_else(err)?;
    let f = realm
        .heap
        .obj(obj)
        .get("__bytes")
        .and_then(|v| v.as_foreign())
        .ok_or_else(err)?;
    match realm.heap.foreign(f) {
        tsr_memory::Foreign::Handle(BYTES_KIND, any) => {
            any.clone().downcast::<Vec<u8>>().map_err(|_| err())
        }
        _ => Err(err()),
    }
}

pub fn install(realm: &mut Realm) {
    let read_file = realm.add_native(|realm, args| {
        let p = path_arg(realm, args, 0, "fs.readFile")?;
        Ok(async_op(realm, move || {
            std::fs::read_to_string(&p)
                .map(|s| PortableValue::Str(Arc::from(s.as_str())))
                .map_err(|e| format!("fs.readFile {}: {e}", p.display()))
        }))
    });
    let write_file = realm.add_native(|realm, args| {
        let p = path_arg(realm, args, 0, "fs.writeFile")?;
        let contents = str_arg(realm, args, 1, "fs.writeFile")?;
        Ok(async_op(realm, move || {
            std::fs::write(&p, contents)
                .map(|_| PortableValue::Undefined)
                .map_err(|e| format!("fs.writeFile {}: {e}", p.display()))
        }))
    });
    let append_file = realm.add_native(|realm, args| {
        let p = path_arg(realm, args, 0, "fs.appendFile")?;
        let contents = str_arg(realm, args, 1, "fs.appendFile")?;
        Ok(async_op(realm, move || {
            use std::io::Write;
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&p)
                .and_then(|mut f| f.write_all(contents.as_bytes()))
                .map(|_| PortableValue::Undefined)
                .map_err(|e| format!("fs.appendFile {}: {e}", p.display()))
        }))
    });
    let read_dir = realm.add_native(|realm, args| {
        let p = path_arg(realm, args, 0, "fs.readDir")?;
        Ok(async_op(realm, move || {
            std::fs::read_dir(&p)
                .map_err(|e| format!("fs.readDir {}: {e}", p.display()))
                .map(|it| {
                    let mut names: Vec<PortableValue> = it
                        .filter_map(|e| e.ok())
                        .map(|e| {
                            PortableValue::Str(Arc::from(
                                e.file_name().to_string_lossy().as_ref(),
                            ))
                        })
                        .collect();
                    // deterministic order for scripts and tests
                    names.sort_by(|a, b| match (a, b) {
                        (PortableValue::Str(x), PortableValue::Str(y)) => x.cmp(y),
                        _ => std::cmp::Ordering::Equal,
                    });
                    PortableValue::Array(names)
                })
        }))
    });
    let stat = realm.add_native(|realm, args| {
        let p = path_arg(realm, args, 0, "fs.stat")?;
        Ok(async_op(realm, move || {
            std::fs::metadata(&p)
                .map(|m| {
                    let modified_ms = m
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_millis() as f64)
                        .unwrap_or(0.0);
                    PortableValue::Object(vec![
                        (Arc::from("size"), PortableValue::Number(m.len() as f64)),
                        (Arc::from("isFile"), PortableValue::Bool(m.is_file())),
                        (Arc::from("isDir"), PortableValue::Bool(m.is_dir())),
                        (Arc::from("modifiedMs"), PortableValue::Number(modified_ms)),
                    ])
                })
                .map_err(|e| format!("fs.stat {}: {e}", p.display()))
        }))
    });
    let mkdir = realm.add_native(|realm, args| {
        let p = path_arg(realm, args, 0, "fs.mkdir")?;
        Ok(async_op(realm, move || {
            std::fs::create_dir_all(&p)
                .map(|_| PortableValue::Undefined)
                .map_err(|e| format!("fs.mkdir {}: {e}", p.display()))
        }))
    });
    let remove = realm.add_native(|realm, args| {
        let p = path_arg(realm, args, 0, "fs.remove")?;
        Ok(async_op(realm, move || {
            let r = if p.is_dir() {
                std::fs::remove_dir_all(&p)
            } else {
                std::fs::remove_file(&p)
            };
            r.map(|_| PortableValue::Undefined)
                .map_err(|e| format!("fs.remove {}: {e}", p.display()))
        }))
    });
    let exists = realm.add_native(|realm, args| {
        let p = path_arg(realm, args, 0, "fs.exists")?;
        Ok(async_op(realm, move || {
            Ok(PortableValue::Bool(p.exists()))
        }))
    });

    let read_bytes = realm.add_native(|realm, args| {
        let p = path_arg(realm, args, 0, "fs.readBytes")?;
        Ok(async_op(realm, move || {
            std::fs::read(&p)
                .map(|b| PortableValue::Handle(BYTES_KIND, Arc::new(b)))
                .map_err(|e| format!("fs.readBytes {}: {e}", p.display()))
        }))
    });
    let write_bytes = realm.add_native(|realm, args| {
        let p = path_arg(realm, args, 0, "fs.writeBytes")?;
        let b = bytes_of(realm, args.get(realm, 1), "fs.writeBytes")?;
        Ok(async_op(realm, move || {
            std::fs::write(&p, b.as_slice())
                .map(|_| PortableValue::Undefined)
                .map_err(|e| format!("fs.writeBytes {}: {e}", p.display()))
        }))
    });

    let mut fs = tsr_memory::Obj::default();
    for (k, v) in [
        ("readFile", read_file),
        ("writeFile", write_file),
        ("appendFile", append_file),
        ("readDir", read_dir),
        ("stat", stat),
        ("mkdir", mkdir),
        ("remove", remove),
        ("exists", exists),
        ("readBytes", read_bytes),
        ("writeBytes", write_bytes),
    ] {
        fs.set(Arc::from(k), v);
    }
    let fs_ref = realm.heap.alloc_obj(fs);

    // runtime.bytes — immutable byte-buffer helpers (v1)
    let b_len = realm.add_native(|realm, args| {
        let b = bytes_of(realm, args.get(realm, 0), "bytes.size")?;
        Ok(Value::number(b.len() as f64))
    });
    let b_slice = realm.add_native(|realm, args| {
        let b = bytes_of(realm, args.get(realm, 0), "bytes.slice")?;
        let from = args.get(realm, 1);
        let to = args.get(realm, 2);
        let from = if from.is_number() { from.as_number().max(0.0) as usize } else { 0 };
        let to = if to.is_number() {
            (to.as_number().max(0.0) as usize).min(b.len())
        } else {
            b.len()
        };
        let out: Vec<u8> = b.get(from..to.max(from)).unwrap_or(&[]).to_vec();
        Ok(make_bytes(realm, Arc::new(out)))
    });
    let b_to_string = realm.add_native(|realm, args| {
        let b = bytes_of(realm, args.get(realm, 0), "bytes.toString")?;
        let s = String::from_utf8_lossy(&b).into_owned();
        Ok(realm.alloc_string(&s))
    });
    let b_from_string = realm.add_native(|realm, args| {
        let s = str_arg(realm, args, 0, "bytes.fromString")?;
        Ok(make_bytes(realm, Arc::new(s.into_bytes())))
    });
    let mut bytes_ns = tsr_memory::Obj::default();
    for (k, v) in [
        ("size", b_len),
        ("slice", b_slice),
        ("toString", b_to_string),
        ("fromString", b_from_string),
    ] {
        bytes_ns.set(Arc::from(k), v);
    }
    let bytes_ref = realm.heap.alloc_obj(bytes_ns);
    // merge into the existing `runtime` global ({cpu, gc, stats})
    if let Some(rt) = realm.globals.get("runtime").copied().and_then(|v| v.as_object()) {
        realm.heap.barrier_obj(rt);
        realm.heap.obj_mut(rt).set(Arc::from("fs"), Value::object(fs_ref));
        realm.heap.obj_mut(rt).set(Arc::from("bytes"), Value::object(bytes_ref));
    } else {
        realm.set_global_obj(
            "runtime",
            vec![
                ("fs", Value::object(fs_ref)),
                ("bytes", Value::object(bytes_ref)),
            ],
        );
    }
}
