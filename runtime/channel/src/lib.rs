//! tsr-channel
//!
//! Bounded async channels with backpressure (spec Phase 5). Global-style
//! API — `Channel.create/send/recv/close(ch, ...)` — so handles are plain
//! objects carrying a hidden `__channel` foreign ref and stay fully
//! portable across realms (parallel chunks, scope children, actors).
//! ponytail: method syntax (`ch.send(v)`) when method dispatch lands.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tsr_memory::{Foreign, PromiseError, Value};
use tsr_realm::{Completer, Realm, RtError};
use tsr_task::portable::{clone_out, rehydrate};
use tsr_task::PortableValue;

pub const HANDLE_KIND: &str = "channel";
pub const HANDLE_FIELD: &str = "__channel";

pub struct ChannelCore {
    capacity: usize,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    q: VecDeque<PortableValue>,
    /// Senders parked by backpressure, carrying their value.
    send_waiters: VecDeque<(PortableValue, Completer)>,
    recv_waiters: VecDeque<Completer>,
    closed: bool,
}

/// Build a handle object for an existing core in this realm.
pub fn make_handle(realm: &mut Realm, core: Arc<ChannelCore>) -> Value {
    let f = realm
        .heap
        .alloc_foreign(Foreign::Handle(HANDLE_KIND, core));
    let mut obj = tsr_memory::Obj::default();
    obj.set(Arc::from(HANDLE_FIELD), Value::foreign(f));
    Value::object(realm.heap.alloc_obj(obj))
}

fn core_of(realm: &Realm, v: Value) -> Result<Arc<ChannelCore>, RtError> {
    let err = || RtError::new("expected a channel (Channel.create(...) handle)");
    let obj = v.as_object().ok_or_else(err)?;
    let f = realm
        .heap
        .obj(obj)
        .get(HANDLE_FIELD)
        .and_then(|v| v.as_foreign())
        .ok_or_else(err)?;
    match realm.heap.foreign(f) {
        Foreign::Handle(HANDLE_KIND, any) => any
            .clone()
            .downcast::<ChannelCore>()
            .map_err(|_| err()),
        _ => Err(err()),
    }
}

/// Install the `Channel` global.
pub fn install(realm: &mut Realm) {
    let create = realm.add_native(|realm, args| {
        let capacity = match args.get(realm, 0).as_object() {
            Some(r) => match realm.heap.obj(r).get("capacity") {
                Some(v) if v.is_number() && v.as_number() >= 0.0 => {
                    v.as_number() as usize
                }
                Some(_) => {
                    return Err(RtError::new(
                        "Channel.create: capacity must be a non-negative number",
                    ))
                }
                None => 1024,
            },
            None => 1024,
        };
        let core = Arc::new(ChannelCore {
            capacity,
            inner: Mutex::new(Inner::default()),
        });
        Ok(make_handle(realm, core))
    });

    let send = realm.add_native(|realm, args| {
        let core = core_of(realm, args.get(realm, 0))?;
        let v = args.get(realm, 1);
        let pv = clone_out(&realm.heap, v).map_err(RtError::new)?;
        let mut inner = core.inner.lock().unwrap();
        if inner.closed {
            return Err(RtError::new("Channel.send: channel is closed"));
        }
        if let Some(waiter) = inner.recv_waiters.pop_front() {
            // direct handoff to a parked receiver
            waiter.settle(Ok(pv));
            return Ok(Value::UNDEFINED);
        }
        if inner.q.len() < core.capacity {
            inner.q.push_back(pv);
            return Ok(Value::UNDEFINED);
        }
        // backpressure: park this sender until a receiver frees a slot
        let (promise, completer) = realm.promise_pair();
        inner.send_waiters.push_back((pv, completer));
        Ok(promise)
    });

    let recv = realm.add_native(|realm, args| {
        let core = core_of(realm, args.get(realm, 0))?;
        let mut inner = core.inner.lock().unwrap();
        if let Some(pv) = inner.q.pop_front() {
            // a queue slot freed: promote a parked sender
            if let Some((wv, wc)) = inner.send_waiters.pop_front() {
                inner.q.push_back(wv);
                wc.settle(Ok(PortableValue::Undefined));
            }
            drop(inner);
            return Ok(rehydrate(&pv, &mut realm.heap));
        }
        // rendezvous with a parked sender (capacity 0, or racing senders)
        if let Some((wv, wc)) = inner.send_waiters.pop_front() {
            wc.settle(Ok(PortableValue::Undefined));
            drop(inner);
            return Ok(rehydrate(&wv, &mut realm.heap));
        }
        if inner.closed {
            return Ok(Value::UNDEFINED); // drained + closed
        }
        let (promise, completer) = realm.promise_pair();
        inner.recv_waiters.push_back(completer);
        Ok(promise)
    });

    let close = realm.add_native(|realm, args| {
        let core = core_of(realm, args.get(realm, 0))?;
        let mut inner = core.inner.lock().unwrap();
        inner.closed = true;
        for waiter in inner.recv_waiters.drain(..) {
            waiter.settle(Ok(PortableValue::Undefined));
        }
        for (_, waiter) in inner.send_waiters.drain(..) {
            waiter.settle(Err(PromiseError {
                msg: "Channel.send: channel closed while waiting".into(),
                cancelled: false,
                span: None,
                source: None,
            }));
        }
        Ok(Value::UNDEFINED)
    });

    realm.set_global_obj(
        "Channel",
        vec![("create", create), ("send", send), ("recv", recv), ("close", close)],
    );
}
