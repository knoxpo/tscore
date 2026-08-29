//! tsr-actor
//!
//! Actor foundation (M2): one realm + bounded mailbox + dedicated OS thread
//! per actor. Sequential locally, parallel globally. See
//! docs/specifications/actors-m2.md for semantics and ceilings.

use crossbeam_channel::{bounded, Receiver, Sender};
use std::sync::Arc;
use tsr_memory::Value;
use tsr_realm::interp::call_value;
use tsr_realm::{Realm, RtError};
use tsr_task::portable::{clone_out, rehydrate};
use tsr_task::PortableValue;

/// ponytail: fixed mailbox bound = free backpressure; configurable at M3
const MAILBOX_CAP: usize = 1024;

enum Msg {
    /// (message, reply slot — Some for send, None for post)
    Deliver(PortableValue, Option<Sender<Result<PortableValue, String>>>),
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
        let pv_setup = clone_out(&realm.heap, setup).map_err(RtError::new)?;
        let handle = Arc::new(spawn_actor(pv_setup)?);

        // handle object: { send, post, stop } natives capturing the mailbox
        let h = handle.clone();
        let send = realm.add_native(move |realm, args| {
            let msg = message_arg(realm, args, &h)?;
            let (reply_tx, reply_rx) = bounded(1);
            h.tx.send(Msg::Deliver(msg, Some(reply_tx)))
                .map_err(|_| stopped(&h))?;
            match reply_rx.recv() {
                Ok(Ok(pv)) => Ok(rehydrate(&pv, &mut realm.heap)),
                Ok(Err(e)) => Err(RtError::new(format!("actor '{}': {e}", h.name))),
                Err(_) => Err(stopped(&h)),
            }
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
fn spawn_actor(pv_setup: PortableValue) -> Result<ActorHandle, RtError> {
    let (tx, rx) = bounded::<Msg>(MAILBOX_CAP);
    let (ready_tx, ready_rx) = bounded::<Result<String, String>>(1);

    std::thread::Builder::new()
        .name("tscore-actor".into())
        .spawn(move || actor_main(pv_setup, rx, ready_tx))
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
) {
    let mut realm = Realm::new();
    tsr_io::install(&mut realm);
    install(&mut realm); // actors can spawn actors
    // note: no `parallel` inside actors at M2 — would nest cleanly, add on ask

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
                    (Some(tx), r) => {
                        let _ = tx.send(r);
                    }
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
            Some(s) => realm.heap.str_at(s).clone(),
            None => return Err("message has no string 'type' field".into()),
        },
        None => return Err("message must be an object".into()),
    };
    let handler = handlers.as_object().and_then(|r| realm.heap.obj(r).get(&msg_type));
    let Some(handler) = handler else {
        return Err(format!("no handler for message type '{msg_type}'"));
    };
    let result = call_value(realm, handler, &[msg]).map_err(|e| e.msg)?;
    clone_out(&realm.heap, result)
}

/// Actor display name = sorted handler names, for error messages.
fn actor_name(realm: &Realm, handlers: Value) -> String {
    match handlers.as_object() {
        Some(r) => {
            let mut names: Vec<&str> =
                realm.heap.obj(r).fields.iter().map(|(k, _)| &**k).collect();
            names.sort_unstable();
            format!("{{{}}}", names.join(","))
        }
        None => "<actor>".into(),
    }
}
