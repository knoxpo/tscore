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

/// NaN-boxed value: 8 bytes. Real doubles occupy every bit pattern whose
/// top 16 bits are ≤ 0xFFF8 (hardware NaNs are 0x7FF8/0xFFF8-prefixed and
/// user code cannot craft payload NaNs — `Value::number` canonicalizes any
/// NaN input). Tags 0xFFF9..=0xFFFF encode non-number values with a 32-bit
/// payload (heap `Ref` / native index / singleton id).
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Value(u64);

const TAG_SHIFT: u32 = 48;
const TAG_SPECIAL: u64 = 0xFFF9; // payload: 0 null, 1 undefined, 2 false, 3 true
const TAG_STR: u64 = 0xFFFA;
const TAG_OBJ: u64 = 0xFFFB;
const TAG_ARR: u64 = 0xFFFC;
const TAG_CLOSURE: u64 = 0xFFFD;
const TAG_CELL: u64 = 0xFFFE;
const TAG_NATIVE: u64 = 0xFFFF;
const CANON_NAN: u64 = 0x7FF8_0000_0000_0000;

/// Decoded view of a [`Value`] for match sites off the hot path.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
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

impl Value {
    pub const NULL: Value = Value((TAG_SPECIAL << TAG_SHIFT) | 0);
    pub const UNDEFINED: Value = Value((TAG_SPECIAL << TAG_SHIFT) | 1);
    pub const FALSE: Value = Value((TAG_SPECIAL << TAG_SHIFT) | 2);
    pub const TRUE: Value = Value((TAG_SPECIAL << TAG_SHIFT) | 3);

    #[inline(always)]
    pub fn number(n: f64) -> Value {
        if n.is_nan() {
            Value(CANON_NAN)
        } else {
            Value(n.to_bits())
        }
    }

    /// For values already known non-NaN (int-derived); skips the
    /// canonicalization branch.
    #[inline(always)]
    pub fn number_unchecked(n: f64) -> Value {
        debug_assert!(!n.is_nan());
        Value(n.to_bits())
    }

    #[inline(always)]
    pub fn bool(b: bool) -> Value {
        if b { Value::TRUE } else { Value::FALSE }
    }

    #[inline(always)]
    fn tagged(tag: u64, payload: u32) -> Value {
        Value((tag << TAG_SHIFT) | payload as u64)
    }

    #[inline(always)]
    pub fn str_ref(r: Ref) -> Value {
        Value::tagged(TAG_STR, r)
    }
    #[inline(always)]
    pub fn object(r: Ref) -> Value {
        Value::tagged(TAG_OBJ, r)
    }
    #[inline(always)]
    pub fn array(r: Ref) -> Value {
        Value::tagged(TAG_ARR, r)
    }
    #[inline(always)]
    pub fn closure(r: Ref) -> Value {
        Value::tagged(TAG_CLOSURE, r)
    }
    #[inline(always)]
    pub fn cell(r: Ref) -> Value {
        Value::tagged(TAG_CELL, r)
    }
    #[inline(always)]
    pub fn native(i: u32) -> Value {
        Value::tagged(TAG_NATIVE, i)
    }

    #[inline(always)]
    fn tag(self) -> u64 {
        self.0 >> TAG_SHIFT
    }
    #[inline(always)]
    fn payload(self) -> u32 {
        self.0 as u32
    }

    #[inline(always)]
    pub fn is_number(self) -> bool {
        self.tag() < TAG_SPECIAL
    }
    #[inline(always)]
    pub fn as_number(self) -> f64 {
        f64::from_bits(self.0)
    }
    #[inline(always)]
    pub fn as_closure(self) -> Option<Ref> {
        (self.tag() == TAG_CLOSURE).then(|| self.payload())
    }
    #[inline(always)]
    pub fn as_cell(self) -> Option<Ref> {
        (self.tag() == TAG_CELL).then(|| self.payload())
    }
    #[inline(always)]
    pub fn as_array(self) -> Option<Ref> {
        (self.tag() == TAG_ARR).then(|| self.payload())
    }
    #[inline(always)]
    pub fn as_object(self) -> Option<Ref> {
        (self.tag() == TAG_OBJ).then(|| self.payload())
    }
    #[inline(always)]
    pub fn as_str_ref(self) -> Option<Ref> {
        (self.tag() == TAG_STR).then(|| self.payload())
    }

    #[inline(always)]
    pub fn kind(self) -> Kind {
        if self.is_number() {
            return Kind::Number(self.as_number());
        }
        let p = self.payload();
        match self.tag() {
            TAG_SPECIAL => match p {
                0 => Kind::Null,
                1 => Kind::Undefined,
                2 => Kind::Bool(false),
                _ => Kind::Bool(true),
            },
            TAG_STR => Kind::Str(p),
            TAG_OBJ => Kind::Object(p),
            TAG_ARR => Kind::Array(p),
            TAG_CLOSURE => Kind::Closure(p),
            TAG_CELL => Kind::Cell(p),
            _ => Kind::Native(p),
        }
    }
}

impl std::fmt::Debug for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.kind().fmt(f)
    }
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
    #[inline(always)]
    pub fn truthy(self, heap: &Heap) -> bool {
        if self.is_number() {
            let n = self.as_number();
            return n != 0.0 && !n.is_nan();
        }
        match self.kind() {
            Kind::Bool(b) => b,
            Kind::Null | Kind::Undefined => false,
            Kind::Str(r) => !heap.str_at(r).is_empty(),
            _ => true,
        }
    }

    pub fn type_of(self) -> &'static str {
        match self.kind() {
            Kind::Number(_) => "number",
            Kind::Bool(_) => "boolean",
            Kind::Null => "object",
            Kind::Undefined => "undefined",
            Kind::Str(_) => "string",
            Kind::Object(_) | Kind::Array(_) | Kind::Cell(_) => "object",
            Kind::Closure(_) | Kind::Native(_) => "function",
        }
    }

    /// `===` semantics (numbers by f64 compare — NaN≠NaN, ±0 equal;
    /// strings by content; references by identity).
    #[inline(always)]
    pub fn strict_eq(self, other: Value, heap: &Heap) -> bool {
        if self.is_number() || other.is_number() {
            return self.is_number()
                && other.is_number()
                && self.as_number() == other.as_number();
        }
        if let (Some(a), Some(b)) = (self.as_str_ref(), other.as_str_ref()) {
            return a == b || heap.str_at(a) == heap.str_at(b);
        }
        self == other
    }

    pub fn display(self, heap: &Heap) -> String {
        match self.kind() {
            Kind::Number(n) => fmt_number(n),
            Kind::Bool(b) => b.to_string(),
            Kind::Null => "null".into(),
            Kind::Undefined => "undefined".into(),
            Kind::Str(r) => heap.str_at(r).to_string(),
            Kind::Array(r) => {
                let items: Vec<String> =
                    heap.arr(r).iter().map(|v| v.display(heap)).collect();
                format!("[ {} ]", items.join(", "))
            }
            Kind::Object(r) => {
                let fields: Vec<String> = heap
                    .obj(r)
                    .fields
                    .iter()
                    .map(|(k, v)| format!("{k}: {}", v.display(heap)))
                    .collect();
                format!("{{ {} }}", fields.join(", "))
            }
            Kind::Closure(_) | Kind::Native(_) => "[Function]".into(),
            Kind::Cell(_) => "[Cell]".into(),
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
