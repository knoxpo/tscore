//! Structured clone between realms.

use std::collections::HashSet;
use std::sync::Arc;
use tsc_ir::FunctionProto;
use tsr_memory::{Closure, Heap, Obj, Value};

/// Realm-independent value tree. `Send + Sync`: safe to hand to any worker.
#[derive(Clone, Debug)]
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
    Ok(match v {
        Value::Number(n) => PortableValue::Number(n),
        Value::Bool(b) => PortableValue::Bool(b),
        Value::Null => PortableValue::Null,
        Value::Undefined => PortableValue::Undefined,
        Value::Str(r) => PortableValue::Str(heap.str_at(r).clone()),
        Value::Array(r) => {
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
        Value::Object(r) => {
            if !visiting.insert((1, r)) {
                return Err("cannot capture cyclic data across realms".into());
            }
            let fields = heap
                .obj(r)
                .fields
                .iter()
                .map(|(k, x)| Ok((k.clone(), clone_rec(heap, *x, visiting)?)))
                .collect::<Result<_, String>>()?;
            visiting.remove(&(1, r));
            PortableValue::Object(fields)
        }
        Value::Closure(r) => {
            if !visiting.insert((2, r)) {
                return Err("cannot capture cyclic closure across realms".into());
            }
            let c = heap.closure(r);
            let upvals = c
                .upvals
                .iter()
                .map(|&cell| clone_rec(heap, *heap.cell(cell), visiting))
                .collect::<Result<_, _>>()?;
            let pv = PortableValue::Closure { proto: c.proto.clone(), upvals };
            visiting.remove(&(2, r));
            pv
        }
        Value::Cell(_) => return Err("internal: cell escaped registers".into()),
        Value::Native(_) => {
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
        PortableValue::Number(n) => Value::Number(*n),
        PortableValue::Bool(b) => Value::Bool(*b),
        PortableValue::Null => Value::Null,
        PortableValue::Undefined => Value::Undefined,
        PortableValue::Str(s) => Value::Str(heap.alloc_str(s.clone())),
        PortableValue::Array(items) => {
            let vals: Vec<Value> = items.iter().map(|x| rehydrate(x, heap)).collect();
            Value::Array(heap.alloc_arr(vals))
        }
        PortableValue::Object(fields) => {
            let mut obj = Obj::default();
            for (k, x) in fields {
                let v = rehydrate(x, heap);
                obj.set(k.clone(), v);
            }
            Value::Object(heap.alloc_obj(obj))
        }
        PortableValue::Closure { proto, upvals } => {
            let cells: Vec<u32> = upvals
                .iter()
                .map(|x| {
                    let v = rehydrate(x, heap);
                    heap.alloc_cell(v)
                })
                .collect();
            Value::Closure(heap.alloc_closure(Closure {
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
        let inner = a.alloc_arr(vec![Value::Number(1.0), Value::Str(s)]);
        let mut obj = Obj::default();
        obj.set(Arc::from("xs"), Value::Array(inner));
        obj.set(Arc::from("ok"), Value::Bool(true));
        let o = a.alloc_obj(obj);

        let pv = clone_out(&a, Value::Object(o)).unwrap();
        let mut b = Heap::new();
        let v = rehydrate(&pv, &mut b);
        let Value::Object(r) = v else { panic!() };
        let xs = b.obj(r).get("xs").unwrap();
        let Value::Array(xr) = xs else { panic!() };
        assert_eq!(b.arr(xr).len(), 2);
        // mutation isolation: heap a untouched by heap b writes
        b.arr_mut(xr).push(Value::Number(3.0));
        assert_eq!(a.arr(inner).len(), 2);
    }

    #[test]
    fn cycle_detected() {
        let mut h = Heap::new();
        let arr = h.alloc_arr(vec![]);
        h.arr_mut(arr).push(Value::Array(arr));
        assert!(clone_out(&h, Value::Array(arr)).unwrap_err().contains("cyclic"));
    }

    #[test]
    fn sibling_references_are_not_cycles() {
        let mut h = Heap::new();
        let shared = h.alloc_arr(vec![Value::Number(1.0)]);
        let outer = h.alloc_arr(vec![Value::Array(shared), Value::Array(shared)]);
        clone_out(&h, Value::Array(outer)).unwrap();
    }
}
