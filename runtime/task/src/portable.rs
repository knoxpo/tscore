//! Structured clone between realms.

use std::collections::HashSet;
use std::sync::Arc;
use tsc_ir::FunctionProto;
use tsr_memory::{Closure, Foreign, Heap, Kind, Obj, Value};

/// Realm-independent value tree. `Send + Sync`: safe to hand to any worker.
#[derive(Clone)]
pub enum PortableValue {
    Number(f64),
    Bool(bool),
    Null,
    Undefined,
    Str(Arc<str>),
    Array(Vec<PortableValue>),
    Object(Vec<(Arc<str>, PortableValue)>),
    Closure {
        proto: Arc<FunctionProto>,
        upvals: Vec<PortableValue>,
    },
    /// Shared runtime handle (channel, actor ref): kind name + opaque core.
    /// Rehydrates to an object with a hidden `__<kind>` foreign field.
    Handle(&'static str, Arc<dyn std::any::Any + Send + Sync>),
}

impl std::fmt::Debug for PortableValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PortableValue::Number(n) => write!(f, "Number({n})"),
            PortableValue::Bool(b) => write!(f, "Bool({b})"),
            PortableValue::Null => write!(f, "Null"),
            PortableValue::Undefined => write!(f, "Undefined"),
            PortableValue::Str(s) => write!(f, "Str({s:?})"),
            PortableValue::Array(a) => f.debug_tuple("Array").field(a).finish(),
            PortableValue::Object(o) => f.debug_tuple("Object").field(o).finish(),
            PortableValue::Closure { proto, .. } => {
                write!(f, "Closure({})", proto.name)
            }
            PortableValue::Handle(kind, _) => write!(f, "Handle({kind})"),
        }
    }
}

/// Deep-copy a realm value into a portable tree.
/// Errors on cycles and on values that cannot cross realms (natives).
pub fn clone_out(heap: &Heap, v: Value) -> Result<PortableValue, String> {
    let mut visiting = HashSet::new();
    clone_rec(heap, v, &mut visiting)
}

fn clone_rec(
    heap: &Heap,
    v: Value,
    visiting: &mut HashSet<(u8, u32)>,
) -> Result<PortableValue, String> {
    Ok(match v.kind() {
        Kind::Number(n) => PortableValue::Number(n),
        Kind::Bool(b) => PortableValue::Bool(b),
        Kind::Null => PortableValue::Null,
        Kind::Undefined => PortableValue::Undefined,
        Kind::Str(r) => PortableValue::Str(heap.str_arc(r)),
        Kind::Array(r) => {
            if !visiting.insert((0, r)) {
                return Err("cannot capture cyclic data across realms".into());
            }
            let items = heap
                .arr(r)
                .iter()
                .map(|&x| clone_rec(heap, x, visiting))
                .collect::<Result<_, _>>()?;
            visiting.remove(&(0, r));
            PortableValue::Array(items)
        }
        Kind::Object(r) => {
            // hidden-handle objects (channels, actor refs) travel as their
            // shared core, not as field-by-field clones
            for (k, v) in heap.obj(r).entries() {
                if k.starts_with("__") {
                    if let Some(f) = v.as_foreign() {
                        if let Foreign::Handle(kind, any) = heap.foreign(f) {
                            return Ok(PortableValue::Handle(kind, any.clone()));
                        }
                    }
                }
            }
            if !visiting.insert((1, r)) {
                return Err("cannot capture cyclic data across realms".into());
            }
            let fields = heap
                .obj(r)
                .entries()
                .map(|(k, x)| Ok((k.clone(), clone_rec(heap, x, visiting)?)))
                .collect::<Result<_, String>>()?;
            visiting.remove(&(1, r));
            PortableValue::Object(fields)
        }
        Kind::Closure(r) => {
            if !visiting.insert((2, r)) {
                return Err("cannot capture cyclic closure across realms".into());
            }
            let c = heap.closure(r);
            let upvals = c
                .upvals
                .iter()
                .map(|&entry| match entry.as_cell() {
                    Some(cell) => clone_rec(heap, *heap.cell(cell), visiting),
                    None => clone_rec(heap, entry, visiting),
                })
                .collect::<Result<_, _>>()?;
            let pv = PortableValue::Closure { proto: c.proto.clone(), upvals };
            visiting.remove(&(2, r));
            pv
        }
        Kind::Cell(_) => return Err("internal: cell escaped registers".into()),
        Kind::Foreign(r) => {
            let what = match heap.foreign(r) {
                Foreign::Promise(_) => "a promise",
                Foreign::Coroutine(_) => "a coroutine",
                Foreign::Handle(kind, _) => kind,
                Foreign::Free => "a freed value",
            };
            return Err(format!("cannot capture {what} across realms"));
        }
        Kind::Native(_) => {
            return Err(
                "cannot capture a native function across realms (reference it \
                 by global name inside the callback instead)"
                    .into(),
            )
        }
    })
}

/// Allocate a realm-local copy of a portable tree.
pub fn rehydrate(pv: &PortableValue, heap: &mut Heap) -> Value {
    match pv {
        PortableValue::Number(n) => Value::number(*n),
        PortableValue::Bool(b) => Value::bool(*b),
        PortableValue::Null => Value::NULL,
        PortableValue::Undefined => Value::UNDEFINED,
        PortableValue::Str(s) => Value::str_ref(heap.alloc_str(s.clone())),
        PortableValue::Array(items) => {
            let vals: Vec<Value> = items.iter().map(|x| rehydrate(x, heap)).collect();
            Value::array(heap.alloc_arr(vals))
        }
        PortableValue::Object(fields) => {
            let mut obj = Obj::default();
            for (k, x) in fields {
                let v = rehydrate(x, heap);
                obj.set(k.clone(), v);
            }
            Value::object(heap.alloc_obj(obj))
        }
        PortableValue::Handle(kind, any) => {
            let f = heap.alloc_foreign(Foreign::Handle(kind, any.clone()));
            let mut obj = Obj::default();
            obj.set(Arc::from(format!("__{kind}").as_str()), Value::foreign(f));
            Value::object(heap.alloc_obj(obj))
        }
        PortableValue::Closure { proto, upvals } => {
            let cells: Vec<Value> = upvals
                .iter()
                .map(|x| {
                    let v = rehydrate(x, heap);
                    Value::cell(heap.alloc_cell(v))
                })
                .collect();
            Value::closure(heap.alloc_closure(Closure {
                proto: proto.clone(),
                upvals: cells,
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_nested() {
        let mut a = Heap::new();
        let s = a.alloc_str(Arc::from("hi"));
        let inner = a.alloc_arr(vec![Value::number(1.0), Value::str_ref(s)]);
        let mut obj = Obj::default();
        obj.set(Arc::from("xs"), Value::array(inner));
        obj.set(Arc::from("ok"), Value::bool(true));
        let o = a.alloc_obj(obj);

        let pv = clone_out(&a, Value::object(o)).unwrap();
        let mut b = Heap::new();
        let v = rehydrate(&pv, &mut b);
        let Some(r) = v.as_object() else { panic!() };
        let xs = b.obj(r).get("xs").unwrap();
        let Some(xr) = xs.as_array() else { panic!() };
        assert_eq!(b.arr(xr).len(), 2);
        // mutation isolation: heap a untouched by heap b writes
        b.arr_mut(xr).push(Value::number(3.0));
        assert_eq!(a.arr(inner).len(), 2);
    }

    #[test]
    fn cycle_detected() {
        let mut h = Heap::new();
        let arr = h.alloc_arr(vec![]);
        h.arr_mut(arr).push(Value::array(arr));
        assert!(clone_out(&h, Value::array(arr)).unwrap_err().contains("cyclic"));
    }

    #[test]
    fn sibling_references_are_not_cycles() {
        let mut h = Heap::new();
        let shared = h.alloc_arr(vec![Value::number(1.0)]);
        let outer = h.alloc_arr(vec![Value::array(shared), Value::array(shared)]);
        clone_out(&h, Value::array(outer)).unwrap();
    }
}
