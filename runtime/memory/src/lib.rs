//! tsr-memory
//!
//! Per-realm arena heap and value representation. `Ref` handles are indices
//! into their owning realm's heap and must never cross realms — the
//! PortableValue clone boundary (tsr-task) is the only way data moves.
//! M1: bump-style Vec arenas, free-on-realm-death; mark-sweep for the main
//! realm arrives in tsr-gc (P4).

use std::sync::Arc;
use tsc_ir::FunctionProto;

/// Index into one of the realm heap's arenas.
pub type Ref = u32;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Value {
    Number(f64),
    Bool(bool),
    Null,
    Undefined,
    Str(Ref),
    Object(Ref),
    Array(Ref),
    Closure(Ref),
    Cell(Ref),
    /// Index into the realm's native function table.
    Native(u32),
}

#[derive(Debug)]
pub struct Closure {
    pub proto: Arc<FunctionProto>,
    /// Cell refs captured per proto.upvals.
    pub upvals: Vec<Ref>,
}

/// Shapeless object: linear scan is fine at M1 field counts.
#[derive(Default, Debug)]
pub struct Obj {
    pub fields: Vec<(Arc<str>, Value)>,
}

impl Obj {
    pub fn get(&self, name: &str) -> Option<Value> {
        self.fields.iter().find(|(k, _)| &**k == name).map(|(_, v)| *v)
    }
    pub fn set(&mut self, name: Arc<str>, v: Value) {
        match self.fields.iter_mut().find(|(k, _)| **k == *name) {
            Some((_, slot)) => *slot = v,
            None => self.fields.push((name, v)),
        }
    }
}

pub struct Heap {
    pub strs: Vec<Arc<str>>,
    pub objs: Vec<Obj>,
    pub arrs: Vec<Vec<Value>>,
    pub closures: Vec<Closure>,
    pub cells: Vec<Value>,
    // free lists rebuilt by the sweep (tsr-gc); alloc reuses them.
    // ponytail: non-moving collector — no compaction, refs stay stable
    pub free_strs: Vec<Ref>,
    pub free_objs: Vec<Ref>,
    pub free_arrs: Vec<Ref>,
    pub free_closures: Vec<Ref>,
    pub free_cells: Vec<Ref>,
    pub allocs_since_gc: usize,
    pub gc_threshold: usize,
}

impl Default for Heap {
    fn default() -> Self {
        Heap {
            strs: Vec::new(),
            objs: Vec::new(),
            arrs: Vec::new(),
            closures: Vec::new(),
            cells: Vec::new(),
            free_strs: Vec::new(),
            free_objs: Vec::new(),
            free_arrs: Vec::new(),
            free_closures: Vec::new(),
            free_cells: Vec::new(),
            allocs_since_gc: 0,
            gc_threshold: 1 << 18, // 256k allocations between collections
        }
    }
}

macro_rules! alloc {
    ($fn_name:ident, $get:ident, $get_mut:ident, $field:ident, $free:ident, $t:ty) => {
        pub fn $fn_name(&mut self, v: $t) -> Ref {
            self.allocs_since_gc += 1;
            if let Some(r) = self.$free.pop() {
                self.$field[r as usize] = v;
                return r;
            }
            self.$field.push(v);
            (self.$field.len() - 1) as Ref
        }
        pub fn $get(&self, r: Ref) -> &$t {
            &self.$field[r as usize]
        }
        pub fn $get_mut(&mut self, r: Ref) -> &mut $t {
            &mut self.$field[r as usize]
        }
    };
}

impl Heap {
    pub fn new() -> Self {
        Self::default()
    }
    alloc!(alloc_str, str_at, str_at_mut, strs, free_strs, Arc<str>);
    alloc!(alloc_obj, obj, obj_mut, objs, free_objs, Obj);
    alloc!(alloc_arr, arr, arr_mut, arrs, free_arrs, Vec<Value>);
    alloc!(alloc_closure, closure, closure_mut, closures, free_closures, Closure);
    alloc!(alloc_cell, cell, cell_mut, cells, free_cells, Value);

    pub fn needs_gc(&self) -> bool {
        self.allocs_since_gc >= self.gc_threshold
    }
}

impl Value {
    pub fn truthy(self, heap: &Heap) -> bool {
        match self {
            Value::Bool(b) => b,
            Value::Number(n) => n != 0.0 && !n.is_nan(),
            Value::Null | Value::Undefined => false,
            Value::Str(r) => !heap.str_at(r).is_empty(),
            _ => true,
        }
    }

    pub fn type_of(self) -> &'static str {
        match self {
            Value::Number(_) => "number",
            Value::Bool(_) => "boolean",
            Value::Null => "object",
            Value::Undefined => "undefined",
            Value::Str(_) => "string",
            Value::Object(_) | Value::Array(_) | Value::Cell(_) => "object",
            Value::Closure(_) | Value::Native(_) => "function",
        }
    }

    /// `===` semantics (strings by content, references by identity).
    pub fn strict_eq(self, other: Value, heap: &Heap) -> bool {
        match (self, other) {
            (Value::Str(a), Value::Str(b)) => a == b || heap.str_at(a) == heap.str_at(b),
            (a, b) => a == b,
        }
    }

    pub fn display(self, heap: &Heap) -> String {
        match self {
            Value::Number(n) => fmt_number(n),
            Value::Bool(b) => b.to_string(),
            Value::Null => "null".into(),
            Value::Undefined => "undefined".into(),
            Value::Str(r) => heap.str_at(r).to_string(),
            Value::Array(r) => {
                let items: Vec<String> =
                    heap.arr(r).iter().map(|v| v.display(heap)).collect();
                format!("[ {} ]", items.join(", "))
            }
            Value::Object(r) => {
                let fields: Vec<String> = heap
                    .obj(r)
                    .fields
                    .iter()
                    .map(|(k, v)| format!("{k}: {}", v.display(heap)))
                    .collect();
                format!("{{ {} }}", fields.join(", "))
            }
            Value::Closure(_) | Value::Native(_) => "[Function]".into(),
            Value::Cell(_) => "[Cell]".into(),
        }
    }
}

/// JS-style number formatting: integers print without a fraction.
pub fn fmt_number(n: f64) -> String {
    if n.is_nan() {
        "NaN".into()
    } else if n.is_infinite() {
        if n > 0.0 { "Infinity".into() } else { "-Infinity".into() }
    } else if n == n.trunc() && n.abs() < 1e21 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

/// ECMAScript ToInt32.
pub fn to_int32(x: f64) -> i32 {
    if !x.is_finite() || x == 0.0 {
        return 0;
    }
    let m = x.trunc() % 4294967296.0;
    let m = if m < 0.0 { m + 4294967296.0 } else { m };
    m as u32 as i32
}

pub fn to_uint32(x: f64) -> u32 {
    to_int32(x) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn int32_semantics() {
        assert_eq!(to_int32(f64::NAN), 0);
        assert_eq!(to_int32(-1.0), -1);
        assert_eq!(to_int32(4294967296.0), 0);
        assert_eq!(to_int32(2147483648.0), -2147483648);
        assert_eq!(to_int32(3.9), 3);
        assert_eq!(to_int32(-3.9), -3);
    }

    #[test]
    fn number_format() {
        assert_eq!(fmt_number(3.0), "3");
        assert_eq!(fmt_number(3.5), "3.5");
        assert_eq!(fmt_number(-0.0), "0");
    }
}
