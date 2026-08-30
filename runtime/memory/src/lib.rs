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
/// TAG_SPECIAL payloads: 0 null, 1 undefined, 2 false, 3 true,
/// 4 = JIT error sentinel (never a live value), 5..8 reserved,
/// >= FOREIGN_BASE = foreign arena ref + FOREIGN_BASE.
const FOREIGN_BASE: u32 = 8;

/// Returned by JIT helpers to signal "error stored in realm.jit_error".
pub const JIT_ERR_SENTINEL: u64 = (TAG_SPECIAL << TAG_SHIFT) | 4;

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
    /// Index into the realm's foreign arena (promises, coroutines, handles).
    Foreign(Ref),
}

impl Value {
    pub const NULL: Value = Value((TAG_SPECIAL << TAG_SHIFT) | 0);
    pub const UNDEFINED: Value = Value((TAG_SPECIAL << TAG_SHIFT) | 1);
    pub const FALSE: Value = Value((TAG_SPECIAL << TAG_SHIFT) | 2);
    pub const TRUE: Value = Value((TAG_SPECIAL << TAG_SHIFT) | 3);

    #[inline(always)]
    pub const fn bits(self) -> u64 {
        self.0
    }
    #[inline(always)]
    pub const fn from_bits(b: u64) -> Value {
        Value(b)
    }

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
    pub fn foreign(r: Ref) -> Value {
        Value::tagged(TAG_SPECIAL, r + FOREIGN_BASE)
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
    pub fn as_foreign(self) -> Option<Ref> {
        (self.tag() == TAG_SPECIAL && self.payload() >= FOREIGN_BASE)
            .then(|| self.payload() - FOREIGN_BASE)
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
                3 => Kind::Bool(true),
                _ => {
                    debug_assert!(p >= FOREIGN_BASE);
                    Kind::Foreign(p - FOREIGN_BASE)
                }
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

/// Error payload for rejected promises (tsr-memory cannot see RtError).
#[derive(Clone, Debug)]
pub struct PromiseError {
    pub msg: String,
    pub cancelled: bool,
    /// Source byte offset where the error originated, when known.
    pub span: Option<u32>,
}

#[derive(Debug)]
pub enum PromiseState {
    Pending,
    Fulfilled(Value),
    Rejected(PromiseError),
}

#[derive(Debug)]
pub struct Promise {
    pub state: PromiseState,
    /// Coroutine refs (foreign arena) resumed when this settles.
    pub reactions: Vec<Ref>,
}

/// A suspended async-function frame: registers + resume point.
#[derive(Debug)]
pub struct Coroutine {
    pub closure: Option<Ref>,
    pub proto: Arc<FunctionProto>,
    pub regs: Vec<Value>,
    pub resume_pc: usize,
    /// Register receiving the awaited value on resume.
    pub dst: u8,
    /// The promise (foreign ref) this coroutine settles.
    pub promise: Ref,
}

pub enum Foreign {
    Promise(Promise),
    Coroutine(Coroutine),
    /// Opaque shared handle (channels, actor refs) — kind name for display
    /// and downcasting at the owning crate.
    Handle(&'static str, Arc<dyn std::any::Any + Send + Sync>),
    /// Cleared by the sweep; slot on the free list.
    Free,
}

#[derive(Debug)]
pub struct Closure {
    pub proto: Arc<FunctionProto>,
    /// Captured environment per proto.upvals: `Value::cell(r)` for
    /// mutable captures, the captured value itself for immutable ones.
    pub upvals: Vec<Value>,
}

/// Hidden class: objects with the same field history share one shape.
/// Process-global (ids are stable across realms, so the per-proto inline
/// caches on Arc-shared code work from every worker thread). Transitions
/// are rare (one per distinct literal shape) and mutex-guarded; reads are
/// lock-free through the object's own Arc.
#[derive(Debug)]
pub struct ShapeData {
    pub id: u32,
    /// Field names in slot order.
    pub fields: Vec<Arc<str>>,
    transitions: std::sync::Mutex<Vec<(Arc<str>, Arc<ShapeData>)>>,
}

static SHAPE_IDS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);

pub fn empty_shape() -> Arc<ShapeData> {
    static EMPTY: std::sync::OnceLock<Arc<ShapeData>> = std::sync::OnceLock::new();
    EMPTY
        .get_or_init(|| {
            Arc::new(ShapeData {
                id: 0,
                fields: Vec::new(),
                transitions: std::sync::Mutex::new(Vec::new()),
            })
        })
        .clone()
}

impl ShapeData {
    pub fn slot_of(&self, name: &str) -> Option<usize> {
        self.fields.iter().position(|f| &**f == name)
    }

    /// Shape after adding `name` (cached transition).
    pub fn with_field(self: &Arc<Self>, name: Arc<str>) -> Arc<ShapeData> {
        let mut tr = self.transitions.lock().unwrap();
        if let Some((_, next)) = tr.iter().find(|(k, _)| **k == *name) {
            return next.clone();
        }
        let mut fields = self.fields.clone();
        fields.push(name.clone());
        let next = Arc::new(ShapeData {
            id: SHAPE_IDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            fields,
            transitions: std::sync::Mutex::new(Vec::new()),
        });
        tr.push((name, next.clone()));
        next
    }
}

/// Object = shared shape + dense value slots.
#[derive(Debug)]
pub struct Obj {
    pub shape: Arc<ShapeData>,
    pub values: Vec<Value>,
}

impl Default for Obj {
    fn default() -> Self {
        Obj { shape: empty_shape(), values: Vec::new() }
    }
}

impl Obj {
    pub fn get(&self, name: &str) -> Option<Value> {
        self.shape.slot_of(name).map(|i| self.values[i])
    }
    pub fn set(&mut self, name: Arc<str>, v: Value) {
        match self.shape.slot_of(&name) {
            Some(i) => self.values[i] = v,
            None => {
                self.shape = self.shape.with_field(name);
                self.values.push(v);
            }
        }
    }
    /// (name, value) pairs in slot order.
    pub fn entries(&self) -> impl Iterator<Item = (&Arc<str>, &Value)> {
        self.shape.fields.iter().zip(self.values.iter())
    }
}

/// Growable bitmap (1 bit per arena slot).
#[derive(Default)]
pub struct Bitmap(Vec<u64>);

impl Bitmap {
    #[inline(always)]
    pub fn get(&self, i: Ref) -> bool {
        let w = (i >> 6) as usize;
        self.0.get(w).is_some_and(|&b| b & (1 << (i & 63)) != 0)
    }
    #[inline(always)]
    pub fn set(&mut self, i: Ref) {
        let w = (i >> 6) as usize;
        if w >= self.0.len() {
            self.0.resize(w + 1, 0);
        }
        self.0[w] |= 1 << (i & 63);
    }
    #[inline(always)]
    pub fn clear(&mut self, i: Ref) {
        let w = (i >> 6) as usize;
        if let Some(b) = self.0.get_mut(w) {
            *b &= !(1 << (i & 63));
        }
    }
    pub fn clear_all(&mut self) {
        self.0.iter_mut().for_each(|b| *b = 0);
    }
}

/// Per-arena generational state: age bits, barrier dirty bits, and the log
/// of slots allocated since the last collection (the nursery).
#[derive(Default)]
pub struct GenState {
    pub old: Bitmap,
    pub dirty: Bitmap,
    pub young: Vec<Ref>,
}

impl GenState {
    #[inline(always)]
    fn on_alloc(&mut self, r: Ref) {
        self.old.clear(r);
        self.dirty.clear(r);
        self.young.push(r);
    }
}

pub struct Heap {
    pub strs: Vec<Arc<str>>,
    pub objs: Vec<Obj>,
    pub arrs: Vec<Vec<Value>>,
    pub closures: Vec<Closure>,
    pub cells: Vec<Value>,
    pub foreigns: Vec<Foreign>,
    // free lists rebuilt by the sweep (tsr-gc); alloc reuses them.
    // ponytail: non-moving collector — no compaction, refs stay stable
    pub free_strs: Vec<Ref>,
    pub free_objs: Vec<Ref>,
    pub free_arrs: Vec<Ref>,
    pub free_closures: Vec<Ref>,
    pub free_cells: Vec<Ref>,
    pub free_foreigns: Vec<Ref>,
    pub allocs_since_gc: usize,
    pub gc_threshold: usize,
    // generational state (sticky in-place mark-sweep)
    pub gen_strs: GenState,
    pub gen_objs: GenState,
    pub gen_arrs: GenState,
    pub gen_closures: GenState,
    pub gen_cells: GenState,
    pub gen_foreigns: GenState,
    /// Old containers mutated since the last collection (write barrier log).
    pub remembered: Vec<Value>,
    pub promoted_since_major: usize,
    /// Approximate bytes promoted to old since the last major — the major
    /// trigger (slot counts under-estimate big arrays).
    pub promoted_bytes_since_major: usize,
    pub remembered_peak: usize,
}

impl Default for Heap {
    fn default() -> Self {
        Heap {
            strs: Vec::new(),
            objs: Vec::new(),
            arrs: Vec::new(),
            closures: Vec::new(),
            cells: Vec::new(),
            foreigns: Vec::new(),
            free_strs: Vec::new(),
            free_objs: Vec::new(),
            free_arrs: Vec::new(),
            free_closures: Vec::new(),
            free_cells: Vec::new(),
            free_foreigns: Vec::new(),
            allocs_since_gc: 0,
            gc_threshold: 1 << 18, // 256k allocations between collections
            gen_strs: GenState::default(),
            gen_objs: GenState::default(),
            gen_arrs: GenState::default(),
            gen_closures: GenState::default(),
            gen_cells: GenState::default(),
            gen_foreigns: GenState::default(),
            remembered: Vec::new(),
            promoted_since_major: 0,
            promoted_bytes_since_major: 0,
            remembered_peak: 0,
        }
    }
}

macro_rules! alloc {
    ($fn_name:ident, $get:ident, $get_mut:ident, $field:ident, $free:ident, $gen:ident, $t:ty) => {
        pub fn $fn_name(&mut self, v: $t) -> Ref {
            self.allocs_since_gc += 1;
            if let Some(r) = self.$free.pop() {
                self.$field[r as usize] = v;
                self.$gen.on_alloc(r);
                return r;
            }
            self.$field.push(v);
            let r = (self.$field.len() - 1) as Ref;
            self.$gen.on_alloc(r);
            r
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
    alloc!(alloc_str, str_at, str_at_mut, strs, free_strs, gen_strs, Arc<str>);
    alloc!(alloc_obj, obj, obj_mut, objs, free_objs, gen_objs, Obj);
    alloc!(alloc_arr, arr, arr_mut, arrs, free_arrs, gen_arrs, Vec<Value>);
    alloc!(alloc_closure, closure, closure_mut, closures, free_closures, gen_closures, Closure);
    alloc!(alloc_cell, cell, cell_mut, cells, free_cells, gen_cells, Value);
    alloc!(alloc_foreign, foreign, foreign_mut, foreigns, free_foreigns, gen_foreigns, Foreign);

    /// The only sanctioned way to free a foreign slot outside the sweep:
    /// keeps generation bookkeeping consistent for slot reuse.
    pub fn free_foreign(&mut self, r: Ref) {
        self.foreigns[r as usize] = Foreign::Free;
        self.gen_foreigns.old.clear(r);
        self.gen_foreigns.dirty.clear(r);
        self.free_foreigns.push(r);
    }

    // ---- write barriers: record old containers that gain new edges ----
    // Container-only, dedup via dirty bit. Zero cost when the container is
    // young (the common case in churn) — one bitmap test.

    #[inline(always)]
    pub fn barrier_obj(&mut self, r: Ref) {
        if self.gen_objs.old.get(r) && !self.gen_objs.dirty.get(r) {
            self.gen_objs.dirty.set(r);
            self.remembered.push(Value::object(r));
        }
    }
    #[inline(always)]
    pub fn barrier_arr(&mut self, r: Ref) {
        if self.gen_arrs.old.get(r) && !self.gen_arrs.dirty.get(r) {
            self.gen_arrs.dirty.set(r);
            self.remembered.push(Value::array(r));
        }
    }
    #[inline(always)]
    pub fn barrier_cell(&mut self, r: Ref) {
        if self.gen_cells.old.get(r) && !self.gen_cells.dirty.get(r) {
            self.gen_cells.dirty.set(r);
            self.remembered.push(Value::cell(r));
        }
    }
    #[inline(always)]
    pub fn barrier_foreign(&mut self, r: Ref) {
        if self.gen_foreigns.old.get(r) && !self.gen_foreigns.dirty.get(r) {
            self.gen_foreigns.dirty.set(r);
            self.remembered.push(Value::foreign(r));
        }
    }

    pub fn alloc_promise(&mut self) -> Ref {
        self.alloc_foreign(Foreign::Promise(Promise {
            state: PromiseState::Pending,
            reactions: Vec::new(),
        }))
    }

    pub fn promise(&self, r: Ref) -> &Promise {
        match self.foreign(r) {
            Foreign::Promise(p) => p,
            _ => panic!("foreign {r} is not a promise"),
        }
    }

    pub fn promise_mut(&mut self, r: Ref) -> &mut Promise {
        match self.foreign_mut(r) {
            Foreign::Promise(p) => p,
            _ => panic!("foreign {r} is not a promise"),
        }
    }

    pub fn needs_gc(&self) -> bool {
        self.allocs_since_gc >= self.gc_threshold
    }

    /// Allocate an empty array, reusing a freed slot's buffer when
    /// possible (churn workloads would otherwise malloc per array).
    pub fn alloc_arr_empty(&mut self, cap: usize) -> Ref {
        self.allocs_since_gc += 1;
        if let Some(r) = self.free_arrs.pop() {
            let v = &mut self.arrs[r as usize];
            v.clear();
            if v.capacity() > 1024 {
                v.shrink_to(64); // don't hoard giant buffers in the free list
            }
            self.gen_arrs.on_alloc(r);
            return r;
        }
        self.arrs.push(Vec::with_capacity(cap));
        let r = (self.arrs.len() - 1) as Ref;
        self.gen_arrs.on_alloc(r);
        r
    }

    /// Allocate an empty object, reusing a freed slot's values buffer.
    pub fn alloc_obj_empty(&mut self) -> Ref {
        self.allocs_since_gc += 1;
        if let Some(r) = self.free_objs.pop() {
            let o = &mut self.objs[r as usize];
            o.shape = empty_shape();
            o.values.clear();
            self.gen_objs.on_alloc(r);
            return r;
        }
        self.objs.push(Obj::default());
        let r = (self.objs.len() - 1) as Ref;
        self.gen_objs.on_alloc(r);
        r
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
            Kind::Foreign(_) => "object",
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
                    .entries()
                    .map(|(k, v)| format!("{k}: {}", v.display(heap)))
                    .collect();
                format!("{{ {} }}", fields.join(", "))
            }
            Kind::Closure(_) | Kind::Native(_) => "[Function]".into(),
            Kind::Cell(_) => "[Cell]".into(),
            Kind::Foreign(r) => match heap.foreign(r) {
                Foreign::Promise(_) => "[Promise]".into(),
                Foreign::Coroutine(_) => "[Coroutine]".into(),
                Foreign::Handle(kind, _) => format!("[{kind}]"),
                Foreign::Free => "[Freed]".into(),
            },
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
