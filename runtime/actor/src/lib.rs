//! tsr-actor
//!
//! Actor foundation (M2): one realm + bounded mailbox + dedicated OS thread
//! per actor. Sequential locally, parallel globally. See
//! docs/specifications/actors-m2.md for semantics and ceilings.

use crossbeam_channel::{bounded, Receiver, Sender};
use std::sync::Arc;
use tsr_memory::{PromiseError, Value};
use tsr_realm::interp::call_value;
use tsr_realm::{Completer, Realm, RtError};
use tsr_task::portable::{clone_out, rehydrate};
use tsr_task::PortableValue;

const DEFAULT_MAILBOX_CAP: usize = 1024;

pub use tss_parallel::ACTOR_COUNT_HOOK as ACTOR_COUNT;

enum Msg {
    /// (message, reply completer — Some for send, None for post)
    Deliver(PortableValue, Option<Completer>),
    Stop,
}

struct ActorHandle {
    tx: Sender<Msg>,
    name: String,
}

/// Install the `actor` global into a realm.
pub fn install(realm: &mut Realm) {
    let spawn = realm.add_native(|realm, args| {
        let setup = match args.first() {
            Some(&f) if f.as_closure().is_some() => f,
            _ => return Err(RtError::new("actor(setupFn): expected a function")),
        };
        let opts = args.get(1).and_then(|v| v.as_object());
        let mailbox_cap = match opts {
            Some(r) => match realm.heap.obj(r).get("mailbox") {
                Some(v) if v.is_number() && v.as_number() >= 1.0 => {
                    v.as_number() as usize
                }
                Some(_) => {
                    return Err(RtError::new(
                        "actor: mailbox capacity must be a positive number",
                    ))
                }
                None => DEFAULT_MAILBOX_CAP,
            },
            None => DEFAULT_MAILBOX_CAP,
        };
        let max_heap = opts
            .and_then(|r| realm.heap.obj(r).get("maxHeap"))
            .filter(|v| v.is_number())
            .map(|v| v.as_number() as usize);
        let pv_setup = clone_out(&realm.heap, setup).map_err(RtError::new)?;
        let handle = Arc::new(spawn_actor(pv_setup, mailbox_cap, max_heap)?);

        // handle object: { send, post, stop } natives capturing the mailbox
        let h = handle.clone();
        let send = realm.add_native(move |realm, args| {
            let msg = message_arg(realm, args, &h)?;
            let (promise, completer) = realm.promise_pair();
            h.tx.send(Msg::Deliver(msg, Some(completer)))
                .map_err(|_| stopped(&h))?;
            Ok(promise) // settles when the actor replies
        });
        let h = handle.clone();
        let post = realm.add_native(move |realm, args| {
            let msg = message_arg(realm, args, &h)?;
            h.tx.send(Msg::Deliver(msg, None)).map_err(|_| stopped(&h))?;
            Ok(Value::UNDEFINED)
        });
        let h = handle.clone();
        let stop = realm.add_native(move |_, _| {
            let _ = h.tx.send(Msg::Stop); // already stopped is fine
            Ok(Value::UNDEFINED)
        });

        let mut obj = tsr_memory::Obj::default();
        obj.set(Arc::from("send"), send);
        obj.set(Arc::from("post"), post);
        obj.set(Arc::from("stop"), stop);
        Ok(Value::object(realm.heap.alloc_obj(obj)))
    });
    realm.set_global("actor", spawn);
}

fn message_arg(
    realm: &Realm,
    args: &[Value],
    h: &ActorHandle,
) -> Result<PortableValue, RtError> {
    match args.first() {
        Some(&v) if v.as_object().is_some() => clone_out(&realm.heap, v).map_err(RtError::new),
        _ => Err(RtError::new(format!(
            "actor '{}': message must be an object with a 'type' field",
            h.name
        ))),
    }
}

fn stopped(h: &ActorHandle) -> RtError {
    RtError::new(format!("actor '{}' is stopped", h.name))
}

/// Boot the actor thread; blocks until setup ran (or failed) inside it.
fn spawn_actor(
    pv_setup: PortableValue,
    mailbox_cap: usize,
    max_heap: Option<usize>,
) -> Result<ActorHandle, RtError> {
    let (tx, rx) = bounded::<Msg>(mailbox_cap);
    let (ready_tx, ready_rx) = bounded::<Result<String, String>>(1);

    std::thread::Builder::new()
        .name("tscore-actor".into())
        .stack_size(32 << 20)
        .spawn(move || actor_main(pv_setup, rx, ready_tx, max_heap))
        .map_err(|e| RtError::new(format!("cannot spawn actor thread: {e}")))?;

    match ready_rx.recv() {
        Ok(Ok(name)) => Ok(ActorHandle { tx, name }),
        Ok(Err(e)) => Err(RtError::new(format!("actor setup failed: {e}"))),
        Err(_) => Err(RtError::new("actor thread died during setup")),
    }
}

fn actor_main(
    pv_setup: PortableValue,
    rx: Receiver<Msg>,
    ready_tx: Sender<Result<String, String>>,
    max_heap: Option<usize>,
) {
    ACTOR_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let _guard = scopeguard();
    let mut realm = Realm::new();
    realm.max_heap_bytes = max_heap;
    tsr_io::install(&mut realm);
    install(&mut realm); // actors can spawn actors
    tsr_channel::install(&mut realm);
    tss_parallel::install(&mut realm, None); // parallel.* inside handlers
    tss_async::install(&mut realm); // task.scope inside handlers

    let setup = rehydrate(&pv_setup, &mut realm.heap);
    let handlers = match call_value(&mut realm, setup, &[]) {
        Ok(v) if v.as_object().is_some() => v,
        Ok(v) => {
            let _ = ready_tx.send(Err(format!(
                "setup must return an object of handlers, got {}",
                v.type_of()
            )));
            return;
        }
        Err(e) => {
            let _ = ready_tx.send(Err(e.msg));
            return;
        }
    };
    // root the handlers object for this realm's GC
    realm.set_global("__handlers", handlers);
    let name = actor_name(&realm, handlers);
    let _ = ready_tx.send(Ok(name.clone()));

    while let Ok(msg) = rx.recv() {
        match msg {
            Msg::Stop => return,
            Msg::Deliver(pv, reply) => {
                let result = deliver(&mut realm, handlers, &pv);
                match (reply, result) {
                    (Some(completer), Ok(pv)) => completer.settle(Ok(pv)),
                    (Some(completer), Err(e)) => completer.settle(Err(PromiseError {
                        msg: format!("actor '{name}': {e}"),
                        cancelled: false,
                        span: None,
                    })),
                    (None, Err(e)) => {
                        eprintln!("actor '{name}': posted message failed: {e}");
                    }
                    (None, Ok(_)) => {}
                }
            }
        }
    }
}

fn deliver(
    realm: &mut Realm,
    handlers: Value,
    pv: &PortableValue,
) -> Result<PortableValue, String> {
    let msg = rehydrate(pv, &mut realm.heap);
    let msg_type = match msg.as_object() {
        Some(r) => match realm.heap.obj(r).get("type").and_then(|v| v.as_str_ref()) {
            Some(s) => realm.heap.str_at(s),
            None => return Err("message has no string 'type' field".into()),
        },
        None => return Err("message must be an object".into()),
    };
    let handler = handlers.as_object().and_then(|r| realm.heap.obj(r).get(&msg_type));
    let Some(handler) = handler else {
        return Err(format!("no handler for message type '{msg_type}'"));
    };
    // async handlers return a promise: drive this actor's event loop
    let result = call_value(realm, handler, &[msg])
        .and_then(|v| tss_parallel::settle_if_promise(realm, v))
        .map_err(|e| e.msg)?;
    clone_out(&realm.heap, result)
}

struct ActorCountGuard;
impl Drop for ActorCountGuard {
    fn drop(&mut self) {
        ACTOR_COUNT.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}
fn scopeguard() -> ActorCountGuard {
    ActorCountGuard
}

/// Actor display name = sorted handler names, for error messages.
fn actor_name(realm: &Realm, handlers: Value) -> String {
    match handlers.as_object() {
        Some(r) => {
            let mut names: Vec<&str> =
                realm.heap.obj(r).shape.fields.iter().map(|k| &**k).collect();
            names.sort_unstable();
            format!("{{{}}}", names.join(","))
        }
        None => "<actor>".into(),
    }
}
