//! tsr-fs
//!
//! `runtime.fs` — promise-native filesystem API — plus `runtime.bytes`
//! (immutable byte buffers) and `runtime.path` (POSIX path strings).
//!
//! Blocking syscalls run on a small dedicated I/O thread pool (libuv model
//! — NOT the CPU scheduler pool, which must never block); results come back
//! through the Completer/Wake contract, exactly like the timer wheel.
//!
//! Everything that touches the filesystem returns a promise. `cwd` and
//! `tempDir` are process state rather than disk traversal, so they return
//! directly; they are the only synchronous members of `runtime.fs`.

mod bytes;
mod path;

use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use tsr_memory::{PromiseError, Value};
use tsr_realm::{NativeArgs, Realm, RtError};
use tsr_task::PortableValue;

use bytes::{bytes_of, BYTES_KIND};

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

pub(crate) fn str_arg(
    realm: &Realm,
    args: NativeArgs,
    i: usize,
    who: &str,
) -> Result<String, RtError> {
    match args.get(realm, i).as_str_ref() {
        Some(r) => Ok(realm.heap.str_at(r).to_string()),
        None => Err(RtError::new(format!("{who}: expected a string"))),
    }
}

fn num_arg(realm: &Realm, args: NativeArgs, i: usize, who: &str) -> Result<f64, RtError> {
    let v = args.get(realm, i);
    if v.is_number() {
        Ok(v.as_number())
    } else {
        Err(RtError::new(format!(
            "{who}: expected a number, got {}",
            v.type_of()
        )))
    }
}

/// Run `op` on the I/O pool; settle the returned promise with its result.
///
/// The closure is `Send + 'static`, so every realm-heap read (paths, string
/// contents, the `Arc<Vec<u8>>` behind a Bytes handle) must already have
/// happened on the realm thread by the time it is built.
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

/// `{size, isFile, isDir, isSymlink, modifiedMs}` from a metadata read.
fn stat_value(m: &std::fs::Metadata, is_symlink: bool) -> PortableValue {
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
        (Arc::from("isSymlink"), PortableValue::Bool(is_symlink)),
        (Arc::from("modifiedMs"), PortableValue::Number(modified_ms)),
    ])
}

/// Create a fresh directory under the OS temp dir and return its path.
///
/// `create_dir` (not `create_dir_all`) is load-bearing: it fails when the
/// path already exists, including when an attacker planted a symlink there,
/// so a successful call proves this process created the directory.
fn make_temp_dir() -> Result<PathBuf, String> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let base = std::env::temp_dir();
    let pid = std::process::id();
    for _ in 0..64 {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let candidate = base.join(format!("tscore-{pid}-{nanos:08x}-{n:x}"));
        match std::fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("fs.makeTempDir {}: {e}", candidate.display())),
        }
    }
    Err("fs.makeTempDir: no unused name after 64 attempts".into())
}

fn fs_members(realm: &mut Realm) -> Vec<(&'static str, Value)> {
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
    // `{name, isFile, isDir, isSymlink}` per entry: the file type comes back
    // in the dirent, so a tree walk costs no extra syscall. Only a
    // filesystem that reports an unknown type falls back to a stat.
    let read_dir = realm.add_native(|realm, args| {
        let p = path_arg(realm, args, 0, "fs.readDir")?;
        Ok(async_op(realm, move || {
            let it =
                std::fs::read_dir(&p).map_err(|e| format!("fs.readDir {}: {e}", p.display()))?;
            let mut entries: Vec<(String, PortableValue)> = Vec::new();
            for e in it {
                let e = e.map_err(|e| format!("fs.readDir {}: {e}", p.display()))?;
                let name = e.file_name().to_string_lossy().into_owned();
                let ft = e.file_type().ok();
                let (is_file, is_dir, is_link) = match ft {
                    Some(t) => (t.is_file(), t.is_dir(), t.is_symlink()),
                    None => match e.metadata() {
                        Ok(m) => (m.is_file(), m.is_dir(), m.file_type().is_symlink()),
                        Err(_) => (false, false, false),
                    },
                };
                entries.push((
                    name.clone(),
                    PortableValue::Object(vec![
                        (
                            Arc::from("name"),
                            PortableValue::Str(Arc::from(name.as_str())),
                        ),
                        (Arc::from("isFile"), PortableValue::Bool(is_file)),
                        (Arc::from("isDir"), PortableValue::Bool(is_dir)),
                        (Arc::from("isSymlink"), PortableValue::Bool(is_link)),
                    ]),
                ));
            }
            // deterministic order for scripts and tests
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            Ok(PortableValue::Array(
                entries.into_iter().map(|(_, v)| v).collect(),
            ))
        }))
    });
    let stat = realm.add_native(|realm, args| {
        let p = path_arg(realm, args, 0, "fs.stat")?;
        Ok(async_op(realm, move || {
            // `metadata` follows symlinks, so the target is never a link
            std::fs::metadata(&p)
                .map(|m| stat_value(&m, false))
                .map_err(|e| format!("fs.stat {}: {e}", p.display()))
        }))
    });
    let lstat = realm.add_native(|realm, args| {
        let p = path_arg(realm, args, 0, "fs.lstat")?;
        Ok(async_op(realm, move || {
            std::fs::symlink_metadata(&p)
                .map(|m| {
                    let link = m.file_type().is_symlink();
                    stat_value(&m, link)
                })
                .map_err(|e| format!("fs.lstat {}: {e}", p.display()))
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
        Ok(async_op(realm, move || Ok(PortableValue::Bool(p.exists()))))
    });
    let rename = realm.add_native(|realm, args| {
        let from = path_arg(realm, args, 0, "fs.rename")?;
        let to = path_arg(realm, args, 1, "fs.rename")?;
        Ok(async_op(realm, move || {
            std::fs::rename(&from, &to)
                .map(|_| PortableValue::Undefined)
                .map_err(|e| format!("fs.rename {} -> {}: {e}", from.display(), to.display()))
        }))
    });
    let copy = realm.add_native(|realm, args| {
        let from = path_arg(realm, args, 0, "fs.copy")?;
        let to = path_arg(realm, args, 1, "fs.copy")?;
        Ok(async_op(realm, move || {
            std::fs::copy(&from, &to)
                .map(|_| PortableValue::Undefined)
                .map_err(|e| format!("fs.copy {} -> {}: {e}", from.display(), to.display()))
        }))
    });
    let real_path = realm.add_native(|realm, args| {
        let p = path_arg(realm, args, 0, "fs.realPath")?;
        Ok(async_op(realm, move || {
            std::fs::canonicalize(&p)
                .map(|c| PortableValue::Str(Arc::from(c.to_string_lossy().as_ref())))
                .map_err(|e| format!("fs.realPath {}: {e}", p.display()))
        }))
    });
    let truncate = realm.add_native(|realm, args| {
        let p = path_arg(realm, args, 0, "fs.truncate")?;
        let len = num_arg(realm, args, 1, "fs.truncate")?;
        if len < 0.0 || len.fract() != 0.0 {
            return Err(RtError::new(format!(
                "fs.truncate: length {len} must be a non-negative integer"
            )));
        }
        let len = len as u64;
        Ok(async_op(realm, move || {
            std::fs::OpenOptions::new()
                .write(true)
                .open(&p)
                .and_then(|f| f.set_len(len))
                .map(|_| PortableValue::Undefined)
                .map_err(|e| format!("fs.truncate {}: {e}", p.display()))
        }))
    });
    let make_temp = realm.add_native(|realm, _args| {
        Ok(async_op(realm, move || {
            make_temp_dir().map(|p| PortableValue::Str(Arc::from(p.to_string_lossy().as_ref())))
        }))
    });
    let read_link = realm.add_native(|realm, args| {
        let p = path_arg(realm, args, 0, "fs.readLink")?;
        Ok(async_op(realm, move || {
            std::fs::read_link(&p)
                .map(|t| PortableValue::Str(Arc::from(t.to_string_lossy().as_ref())))
                .map_err(|e| format!("fs.readLink {}: {e}", p.display()))
        }))
    });
    let symlink = realm.add_native(|realm, args| {
        let target = path_arg(realm, args, 0, "fs.symlink")?;
        let link = path_arg(realm, args, 1, "fs.symlink")?;
        Ok(async_op(realm, move || {
            #[cfg(unix)]
            {
                std::os::unix::fs::symlink(&target, &link)
                    .map(|_| PortableValue::Undefined)
                    .map_err(|e| {
                        format!("fs.symlink {} -> {}: {e}", link.display(), target.display())
                    })
            }
            #[cfg(not(unix))]
            {
                let _ = (&target, &link);
                Err("fs.symlink: not supported on this platform".to_string())
            }
        }))
    });
    let chmod = realm.add_native(|realm, args| {
        let p = path_arg(realm, args, 0, "fs.chmod")?;
        let mode = num_arg(realm, args, 1, "fs.chmod")?;
        if mode < 0.0 || mode.fract() != 0.0 || mode > 0o7777 as f64 {
            return Err(RtError::new(format!(
                "fs.chmod: mode {mode} must be an integer in 0..=0o7777"
            )));
        }
        Ok(async_op(realm, move || {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode as u32))
                    .map(|_| PortableValue::Undefined)
                    .map_err(|e| format!("fs.chmod {}: {e}", p.display()))
            }
            #[cfg(not(unix))]
            {
                let _ = (&p, mode);
                Err("fs.chmod: not supported on this platform".to_string())
            }
        }))
    });

    // The two synchronous members: process state, not disk traversal.
    let cwd = realm.add_native(|realm, _args| {
        let d = std::env::current_dir().map_err(|e| RtError::new(format!("fs.cwd: {e}")))?;
        let s = d.to_string_lossy().into_owned();
        Ok(realm.alloc_string(&s))
    });
    let temp_dir = realm.add_native(|realm, _args| {
        let s = std::env::temp_dir().to_string_lossy().into_owned();
        Ok(realm.alloc_string(&s))
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

    vec![
        ("readFile", read_file),
        ("writeFile", write_file),
        ("appendFile", append_file),
        ("readDir", read_dir),
        ("stat", stat),
        ("lstat", lstat),
        ("mkdir", mkdir),
        ("remove", remove),
        ("exists", exists),
        ("rename", rename),
        ("copy", copy),
        ("realPath", real_path),
        ("truncate", truncate),
        ("makeTempDir", make_temp),
        ("symlink", symlink),
        ("readLink", read_link),
        ("chmod", chmod),
        ("cwd", cwd),
        ("tempDir", temp_dir),
        ("readBytes", read_bytes),
        ("writeBytes", write_bytes),
    ]
}

/// Build a namespace object from `(name, native)` pairs.
fn namespace(realm: &mut Realm, members: Vec<(&'static str, Value)>) -> Value {
    let mut obj = tsr_memory::Obj::default();
    for (k, v) in members {
        obj.set(Arc::from(k), v);
    }
    Value::object(realm.heap.alloc_obj(obj))
}

pub fn install(realm: &mut Realm) {
    let fs_ns = {
        let m = fs_members(realm);
        namespace(realm, m)
    };
    let bytes_ns = {
        let m = bytes::members(realm);
        namespace(realm, m)
    };
    let path_ns = {
        let m = path::members(realm);
        namespace(realm, m)
    };

    // merge into the existing `runtime` global ({cpu, gc, stats})
    if let Some(rt) = realm
        .globals
        .get("runtime")
        .copied()
        .and_then(|v| v.as_object())
    {
        realm.heap.barrier_obj(rt);
        realm.heap.obj_mut(rt).set(Arc::from("fs"), fs_ns);
        realm.heap.obj_mut(rt).set(Arc::from("bytes"), bytes_ns);
        realm.heap.obj_mut(rt).set(Arc::from("path"), path_ns);
    } else {
        realm.set_global_obj(
            "runtime",
            vec![("fs", fs_ns), ("bytes", bytes_ns), ("path", path_ns)],
        );
    }
}

#[cfg(test)]
mod temp_dir_tests {
    use super::make_temp_dir;

    /// Every call must hand back a directory this process just created —
    /// a name collision must not silently return someone else's directory.
    #[test]
    fn make_temp_dir_is_fresh_each_time() {
        let a = make_temp_dir().unwrap();
        let b = make_temp_dir().unwrap();
        assert_ne!(a, b);
        assert!(a.is_dir() && b.is_dir());
        let _ = std::fs::remove_dir_all(&a);
        let _ = std::fs::remove_dir_all(&b);
    }
}
