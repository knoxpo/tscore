//! tss-async
//!
//! Structured concurrency (M3): `task.scope`, child task handles,
//! cooperative cancellation, timeouts, error propagation. See
//! docs/specifications/structured-concurrency-m3.md.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tsr_memory::Value;
use tsr_realm::interp::call_value;
use tsr_realm::{Realm, RtError};
use tsr_scheduler::Batch;
use tsr_task::portable::{clone_out, rehydrate};
use tsr_task::PortableValue;

#[derive(Clone, Debug)]
struct Failure {
    msg: String,
    cancelled: bool,
}

struct ChildState {
    cancel: Arc<AtomicBool>,
    done: AtomicBool,
    result: Mutex<Option<Result<PortableValue, Failure>>>,
}

struct ScopeState {
    batch: Arc<Batch>,
    children: Mutex<Vec<Arc<ChildState>>>,
    cancel_all: AtomicBool,
    first_error: Mutex<Option<Failure>>,
    finished: AtomicBool,
}

impl ScopeState {
    /// Record a failure (first one wins) and cancel every child.
    fn fail(&self, f: Failure) {
        if !f.cancelled {
            self.first_error.lock().unwrap().get_or_insert(f);
        }
        self.cancel_children();
    }

    fn cancel_children(&self) {
        self.cancel_all.store(true, Ordering::SeqCst);
        for c in self.children.lock().unwrap().iter() {
            c.cancel.store(true, Ordering::SeqCst);
        }
    }
}

/// Install `task.scope` and `Runtime.checkCancellation` into a realm.
pub fn install(realm: &mut Realm) {
    let scope = realm.add_native(task_scope);
    realm.set_global_obj("task", vec![("scope", scope)]);

    let check = realm.add_native(|realm, _| {
        if let Some(c) = &realm.cancel {
            if c.load(Ordering::Relaxed) {
                return Err(RtError::cancelled());
            }
        }
        Ok(Value::Undefined)
    });
    realm.set_global_obj("Runtime", vec![("checkCancellation", check)]);
}

fn task_scope(realm: &mut Realm, args: &[Value]) -> Result<Value, RtError> {
    let cb = match args.first() {
        Some(&f @ Value::Closure(_)) => f,
        _ => return Err(RtError::new("task.scope(fn): expected a function")),
    };
    let timeout_ms = match args.get(1) {
        Some(&Value::Object(r)) => match realm.heap.obj(r).get("timeout") {
            Some(Value::Number(n)) if n > 0.0 => Some(n),
            Some(_) => return Err(RtError::new("task.scope: timeout must be a positive number")),
            None => None,
        },
        None | Some(&Value::Undefined) => None,
        _ => return Err(RtError::new("task.scope: options must be an object")),
    };

    let scope = Arc::new(ScopeState {
        batch: Batch::new(),
        children: Mutex::new(Vec::new()),
        cancel_all: AtomicBool::new(false),
        first_error: Mutex::new(None),
        finished: AtomicBool::new(false),
    });

    if let Some(ms) = timeout_ms {
        let s = scope.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(ms as u64));
            if !s.finished.load(Ordering::SeqCst) {
                s.fail(Failure {
                    msg: format!("scope timed out after {}ms", ms as u64),
                    cancelled: false,
                });
            }
        });
    }

    // scope object handed to the callback: { spawn, cancel }
    let s = scope.clone();
    let spawn = realm.add_native(move |realm, args| spawn_child(realm, args, &s));
    let s = scope.clone();
    let cancel = realm.add_native(move |_, _| {
        s.cancel_children();
        Ok(Value::Undefined)
    });
    let mut obj = tsr_memory::Obj::default();
    obj.set(Arc::from("spawn"), spawn);
    obj.set(Arc::from("cancel"), cancel);
    let scope_obj = Value::Object(realm.heap.alloc_obj(obj));

    // callback runs synchronously in the caller's realm
    let cb_result = call_value(realm, cb, &[scope_obj]);
    if let Err(e) = &cb_result {
        scope.fail(Failure { msg: e.msg.clone(), cancelled: e.cancelled });
    }

    // structured: never exit while a child runs
    tss_parallel::shared_pool().wait(&scope.batch);
    scope.finished.store(true, Ordering::SeqCst);

    if let Some(f) = scope.first_error.lock().unwrap().take() {
        return Err(RtError::new(format!("task scope failed: {}", f.msg)));
    }
    cb_result
}

fn spawn_child(
    realm: &mut Realm,
    args: &[Value],
    scope: &Arc<ScopeState>,
) -> Result<Value, RtError> {
    let f = match args.first() {
        Some(&f @ Value::Closure(_)) => f,
        _ => return Err(RtError::new("scope.spawn(fn): expected a function")),
    };
    if scope.finished.load(Ordering::SeqCst) {
        return Err(RtError::new("scope.spawn: scope already exited"));
    }
    let pv_fn = clone_out(&realm.heap, f).map_err(RtError::new)?;

    let child = Arc::new(ChildState {
        cancel: Arc::new(AtomicBool::new(scope.cancel_all.load(Ordering::SeqCst))),
        done: AtomicBool::new(false),
        result: Mutex::new(None),
    });
    scope.children.lock().unwrap().push(child.clone());

    let c = child.clone();
    let s = scope.clone();
    tss_parallel::shared_pool().submit(
        &scope.batch,
        Box::new(move |_ctx| {
            let outcome = if c.cancel.load(Ordering::SeqCst)
                || s.cancel_all.load(Ordering::SeqCst)
            {
                Err(Failure { msg: "task cancelled".into(), cancelled: true })
            } else {
                run_child(&pv_fn, &c.cancel)
            };
            if let Err(f) = &outcome {
                if !f.cancelled {
                    s.fail(f.clone());
                }
            }
            *c.result.lock().unwrap() = Some(outcome);
            c.done.store(true, Ordering::SeqCst);
        }),
    );

    // handle object: { cancel, join }
    let c = child.clone();
    let cancel = realm.add_native(move |_, _| {
        c.cancel.store(true, Ordering::SeqCst);
        Ok(Value::Undefined)
    });
    let c = child;
    let join = realm.add_native(move |realm, _| {
        tss_parallel::shared_pool().help_until(|| c.done.load(Ordering::SeqCst));
        let guard = c.result.lock().unwrap();
        match guard.as_ref().expect("done without result") {
            Ok(pv) => Ok(rehydrate(pv, &mut realm.heap)),
            Err(f) if f.cancelled => Err(RtError::cancelled()),
            Err(f) => Err(RtError::new(format!("task failed: {}", f.msg))),
        }
    });
    let mut obj = tsr_memory::Obj::default();
    obj.set(Arc::from("cancel"), cancel);
    obj.set(Arc::from("join"), join);
    Ok(Value::Object(realm.heap.alloc_obj(obj)))
}

/// Run one child task in a scratch realm wired to its cancel flag.
fn run_child(pv_fn: &PortableValue, cancel: &Arc<AtomicBool>) -> Result<PortableValue, Failure> {
    let mut realm = Realm::new();
    realm.gc_enabled = false; // scratch realm: drops wholesale
    realm.cancel = Some(cancel.clone());
    tsr_io::install(&mut realm);
    tss_parallel::install(&mut realm, None);
    install(&mut realm); // nested scopes cascade cancellation
    let f = rehydrate(pv_fn, &mut realm.heap);
    match call_value(&mut realm, f, &[]) {
        Ok(v) => clone_out(&realm.heap, v)
            .map_err(|msg| Failure { msg, cancelled: false }),
        Err(e) => Err(Failure { msg: e.msg, cancelled: e.cancelled }),
    }
}
