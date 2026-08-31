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

/// Concatenations at or below this many bytes are copied flat; larger
/// ones build a rope node. Read once — an env lookup per concat costs
/// more than the copy it decides about.
#[inline(always)]
#[allow(non_snake_case)]
fn rope_limit() -> usize {
    static L: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *L.get_or_init(|| {
        std::env::var("TSC_ROPE_LIMIT").ok().and_then(|v| v.parse().ok()).unwrap_or(64)
    })
}

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
/// Returned by h_await to signal "pending — suspend info in realm.jit_await".
pub const JIT_AWAIT_SENTINEL: u64 = (TAG_SPECIAL << TAG_SHIFT) | 5;
/// Module-namespace field placeholder before the exporter initializes it
/// (TDZ): reads through GetField report a clean error instead of
/// undefined. Never a live user value.
pub const TDZ_SENTINEL: u64 = (TAG_SPECIAL << TAG_SHIFT) | 6;

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
    /// Originating file for `span` (module graphs).
    pub source: Option<Arc<str>>,
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
// repr(C): keep `id` at offset 0 — the JIT's shape-guard load hits the
// pointer's first cache line instead of wherever field reordering puts it
#[repr(C)]
#[derive(Debug)]
pub struct ShapeData {
    pub id: u32,
    /// Field names in slot order.
    pub fields: Vec<Arc<str>>,
    transitions: std::sync::RwLock<Vec<(Arc<str>, &'static ShapeData)>>,
}

static SHAPE_IDS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);

/// Shapes are immutable, process-global, and bounded by the program's
/// distinct field sequences — leaked (`&'static`) so the hot paths
/// (allocation, sweep-clear, transitions, TICs) never touch an atomic
/// refcount.
pub fn empty_shape() -> &'static ShapeData {
    static EMPTY: std::sync::OnceLock<&'static ShapeData> = std::sync::OnceLock::new();
    EMPTY.get_or_init(|| {
        Box::leak(Box::new(ShapeData {
            id: 0,
            fields: Vec::new(),
            transitions: std::sync::RwLock::new(Vec::new()),
        }))
    })
}

impl ShapeData {
    pub fn slot_of(&self, name: &str) -> Option<usize> {
        self.fields.iter().position(|f| &**f == name)
    }

    /// Shape after adding `name` — read-mostly: cached transitions hit a
    /// shared read lock (object-literal creation is this path, hot).
    pub fn with_field(&'static self, name: Arc<str>) -> &'static ShapeData {
        {
            let tr = self.transitions.read().unwrap();
            if let Some((_, next)) = tr.iter().find(|(k, _)| **k == *name) {
                return next;
            }
        }
        let mut tr = self.transitions.write().unwrap();
        // re-check under the write lock (racing creator)
        if let Some((_, next)) = tr.iter().find(|(k, _)| **k == *name) {
            return next;
        }
        let mut fields = self.fields.clone();
        fields.push(name.clone());
        let next: &'static ShapeData = Box::leak(Box::new(ShapeData {
            id: SHAPE_IDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            fields,
            transitions: std::sync::RwLock::new(Vec::new()),
        }));
        tr.push((name, next));
        next
    }
}

/// Inline value slots per object; fields beyond this spill to `overflow`.
/// 3 keeps Obj at exactly 64 bytes (pow2 arena stride: the JIT indexes
/// with one shifted add instead of a mul). Bump only with re-measurement.
pub const OBJ_INLINE: usize = 3;

/// Object = shared shape + value slots. The first OBJ_INLINE values live
/// inline in the arena slot (no second allocation, no pointer chase on
/// reads); rare bigger objects spill to `overflow`. repr(C) so the JIT
/// reads fields at compile-time offsets.
#[repr(C)]
#[derive(Debug)]
pub struct Obj {
    pub shape: &'static ShapeData,
    /// Number of populated value slots (== shape.fields.len()).
    pub vlen: u32,
    pub inline: [Value; OBJ_INLINE],
    pub overflow: Vec<Value>,
}

impl Default for Obj {
    fn default() -> Self {
        Obj {
            shape: empty_shape(),
            vlen: 0,
            inline: [Value::UNDEFINED; OBJ_INLINE],
            overflow: Vec::new(),
        }
    }
}

impl Obj {
    #[inline(always)]
    pub fn vlen(&self) -> usize {
        self.vlen as usize
    }
    #[inline(always)]
    pub fn val(&self, i: usize) -> Value {
        if i < OBJ_INLINE {
            self.inline[i]
        } else {
            self.overflow[i - OBJ_INLINE]
        }
    }
    #[inline(always)]
    pub fn set_val(&mut self, i: usize, v: Value) {
        if i < OBJ_INLINE {
            self.inline[i] = v;
        } else {
            self.overflow[i - OBJ_INLINE] = v;
        }
    }
    #[inline(always)]
    pub fn push_val(&mut self, v: Value) {
        let i = self.vlen as usize;
        if i < OBJ_INLINE {
            self.inline[i] = v;
        } else {
            self.overflow.push(v);
        }
        self.vlen += 1;
    }
    #[inline(always)]
    pub fn extend_vals(&mut self, vals: &[Value]) {
        let n_inline = vals.len().min(OBJ_INLINE);
        self.inline[..n_inline].copy_from_slice(&vals[..n_inline]);
        if vals.len() > OBJ_INLINE {
            self.overflow.extend_from_slice(&vals[OBJ_INLINE..]);
        }
        self.vlen += vals.len() as u32;
    }
    /// Reset value storage (keeps overflow capacity for reuse).
    #[inline(always)]
    pub fn clear_vals(&mut self) {
        self.vlen = 0;
        self.overflow.clear();
    }
    pub fn get(&self, name: &str) -> Option<Value> {
        self.shape.slot_of(name).map(|i| self.val(i))
    }
    pub fn set(&mut self, name: Arc<str>, v: Value) {
        match self.shape.slot_of(&name) {
            Some(i) => self.set_val(i, v),
            None => {
                self.shape = self.shape.with_field(name);
                self.push_val(v);
            }
        }
    }
    /// (name, value) pairs in slot order.
    pub fn entries(&self) -> impl Iterator<Item = (&Arc<str>, Value)> {
        self.shape.fields.iter().zip((0..self.vlen()).map(|i| self.val(i)))
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

/// Reusable GC scratch: mark vectors, minor-mark bitmaps, and the trace
/// worklist. Lives on the heap so a collection never allocates
/// proportionally to arena size (a major used to malloc six full-length
/// Vec<bool>s per run).
#[derive(Default)]
pub struct GcScratch {
    pub strs: Vec<bool>,
    pub objs: Vec<bool>,
    pub arrs: Vec<bool>,
    pub closures: Vec<bool>,
    pub cells: Vec<bool>,
    pub foreigns: Vec<bool>,
    pub ystrs: Bitmap,
    pub yobjs: Bitmap,
    pub yarrs: Bitmap,
    pub yclosures: Bitmap,
    pub ycells: Bitmap,
    pub yforeigns: Bitmap,
    pub work: Vec<Value>,
    // evacuation state (copying nursery): forwarding tables per moving
    // kind (u32::MAX = unforwarded) and the Cheney scan queue, packed as
    // (kind << 32) | old_ref
    pub fwd_objs: Vec<u32>,
    pub fwd_arrs: Vec<u32>,
    pub fwd_strs: Vec<u32>,
    pub scan: Vec<u64>,
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
        // invariant: every slot on a free list has old=0 and dirty=0 —
        // the major sweep rebuilds `old` from marks (unmarked ⇒ 0) and
        // clears all dirty bits; minor-freed slots were never old; and
        // free_foreign clears both explicitly
        debug_assert!(!self.old.get(r) && !self.dirty.get(r));
        self.young.push(r);
    }
}

/// Heap string: shared constants stay refcounted; runtime-built strings
/// own a reusable buffer (freed slots keep capacity — steady-state
/// string churn stops calling malloc).
#[derive(Debug)]
pub enum HStr {
    Shared(Arc<str>),
    Buf(String),
    /// Lazily concatenated string: a cons tree that is flattened the
    /// first time the bytes are actually read. `out = out + piece` in a
    /// loop is then O(total bytes) instead of O(n^2) copying, which is
    /// what every production JS engine does (V8 ConsString, JSC ropes).
    ///
    /// The tree holds `Arc`s, not heap refs, so the collector needs to
    /// know nothing about it and `as_str` stays `&self`.
    Rope(Arc<RopeNode>, usize),
}

/// One node of a lazy concatenation tree. `flat` caches the flattened
/// bytes on first read (interior mutability: reading a string must not
/// require `&mut Heap`).
#[derive(Debug)]
pub struct RopeNode {
    kind: RopeKind,
    flat: std::sync::OnceLock<Arc<str>>,
}

#[derive(Debug)]
enum RopeKind {
    Leaf(Arc<str>),
    Cat(Arc<RopeNode>, Arc<RopeNode>),
}

impl RopeNode {
    pub fn leaf(s: Arc<str>) -> Arc<RopeNode> {
        Arc::new(RopeNode { kind: RopeKind::Leaf(s), flat: std::sync::OnceLock::new() })
    }

    pub fn cat(l: Arc<RopeNode>, r: Arc<RopeNode>) -> Arc<RopeNode> {
        Arc::new(RopeNode { kind: RopeKind::Cat(l, r), flat: std::sync::OnceLock::new() })
    }

    /// Flatten once; later reads hit the cache.
    pub fn as_str(&self) -> &str {
        if let RopeKind::Leaf(s) = &self.kind {
            return s; // leaves need no flattening
        }
        self.flat.get_or_init(|| {
            let mut out = String::with_capacity(self.byte_len());
            // iterative walk: deep left-leaning trees must not blow the
            // Rust stack (a 100k-iteration append loop is 100k deep)
            let mut stack: Vec<&RopeNode> = vec![self];
            while let Some(n) = stack.pop() {
                match &n.kind {
                    RopeKind::Leaf(s) => out.push_str(s),
                    RopeKind::Cat(l, r) => {
                        if let Some(f) = n.flat.get() {
                            out.push_str(f); // already-flattened subtree
                        } else {
                            stack.push(r);
                            stack.push(l);
                        }
                    }
                }
            }
            Arc::from(out.as_str())
        })
    }

    pub fn byte_len(&self) -> usize {
        if let Some(f) = self.flat.get() {
            return f.len();
        }
        match &self.kind {
            RopeKind::Leaf(s) => s.len(),
            RopeKind::Cat(l, r) => l.byte_len() + r.byte_len(),
        }
    }
}

impl HStr {
    #[inline(always)]
    pub fn as_str(&self) -> &str {
        match self {
            HStr::Shared(a) => a,
            HStr::Buf(b) => b,
            HStr::Rope(n, _) => n.as_str(),
        }
    }

    /// Byte length without forcing a rope to flatten (the collector and
    /// size accounting must not materialize lazy strings).
    #[inline(always)]
    pub fn byte_len(&self) -> usize {
        match self {
            HStr::Shared(a) => a.len(),
            HStr::Buf(b) => b.len(),
            HStr::Rope(_, n) => *n,
        }
    }

    /// The rope node for this string, cloning cheaply where possible.
    /// A `Buf` must be materialized once; after that concatenation is
    /// O(1) per operation.
    pub fn to_rope(&self) -> Arc<RopeNode> {
        match self {
            HStr::Shared(a) => RopeNode::leaf(a.clone()),
            HStr::Buf(b) => RopeNode::leaf(Arc::from(b.as_str())),
            HStr::Rope(n, _) => n.clone(),
        }
    }
}

/// Young-generation tag: bit 31 of an obj/arr/str Value payload. Young
/// refs index the nursery arenas; minor GC evacuates survivors into the
/// old arenas and rewrites every reachable edge. Closures/cells/foreigns
/// never carry it (non-moving kinds).
pub const YOUNG_BIT: u32 = 1 << 31;

#[inline(always)]
pub fn is_young(r: Ref) -> bool {
    r & YOUNG_BIT != 0
}

/// Bump-allocated nursery for obj/arr/str. Allocation is a push; minor GC
/// evacuates the live survivors and truncates. Backing buffers of dead
/// slots are harvested into the reuse pools before truncation.
#[derive(Default)]
pub struct Nursery {
    pub objs: Vec<Obj>,
    pub arrs: Vec<Vec<Value>>,
    pub strs: Vec<HStr>,
    /// Approximate bytes allocated in the nursery this window (arrays and
    /// strings count their contents — slot counts under-estimate growth).
    pub bytes: usize,
}

pub struct Heap {
    pub strs: Vec<HStr>,
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
    /// Reusable collection scratch (taken/returned by tsr-gc).
    pub gc_scratch: GcScratch,
    /// Copying young generation (N1: present but unused — no allocator
    /// sets YOUNG_BIT yet).
    pub nursery: Nursery,
    /// Nursery byte budget before a minor evacuation (TSC_NURSERY_BYTES).
    pub nursery_limit: usize,
    /// Young allocation enabled (TSC_NURSERY=1; N3 flips the default once
    /// the JIT templates understand YOUNG_BIT).
    pub nursery_on: bool,
    /// [old data ptr, nursery data ptr] — contiguous so the JIT selects
    /// an arena base with one shifted load on the young bit. Refreshed by
    /// `refresh_bases()` at every point a data pointer can move.
    pub obj_bases: [usize; 2],
    pub arr_bases: [usize; 2],
    /// Detached backing buffers harvested from dead nursery slots.
    pub pool_arr_bufs: Vec<Vec<Value>>,
    pub pool_str_bufs: Vec<String>,
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
            gc_scratch: GcScratch::default(),
            nursery: Nursery::default(),
            nursery_limit: std::env::var("TSC_NURSERY_BYTES")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(16 << 20), // 16MB: churn live-sets die in-nursery (measured)
            nursery_on: std::env::var_os("TSC_NO_NURSERY").is_none(),
            obj_bases: [0; 2],
            arr_bases: [0; 2],
            pool_arr_bufs: Vec::new(),
            pool_str_bufs: Vec::new(),
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
            self.refresh_bases();
            let r = (self.$field.len() - 1) as Ref;
            self.$gen.on_alloc(r);
            r
        }
        #[inline(always)]
        pub fn $get(&self, r: Ref) -> &$t {
            &self.$field[r as usize]
        }
        #[inline(always)]
        pub fn $get_mut(&mut self, r: Ref) -> &mut $t {
            &mut self.$field[r as usize]
        }
    };
}

/// Like `alloc!` but for the moving (nursery) kinds: getters dispatch on
/// YOUNG_BIT. The alloc fn stays old-space — young allocation is a
/// separate path (`alloc_*_young`, N2).
macro_rules! alloc_moving {
    ($fn_name:ident, $get:ident, $get_mut:ident, $field:ident, $free:ident, $gen:ident, $t:ty) => {
        pub fn $fn_name(&mut self, v: $t) -> Ref {
            self.allocs_since_gc += 1;
            if let Some(r) = self.$free.pop() {
                self.$field[r as usize] = v;
                self.$gen.on_alloc(r);
                return r;
            }
            self.$field.push(v);
            self.refresh_bases();
            let r = (self.$field.len() - 1) as Ref;
            self.$gen.on_alloc(r);
            r
        }
        #[inline(always)]
        pub fn $get(&self, r: Ref) -> &$t {
            if r & YOUNG_BIT != 0 {
                &self.nursery.$field[(r & !YOUNG_BIT) as usize]
            } else {
                &self.$field[r as usize]
            }
        }
        #[inline(always)]
        pub fn $get_mut(&mut self, r: Ref) -> &mut $t {
            if r & YOUNG_BIT != 0 {
                &mut self.nursery.$field[(r & !YOUNG_BIT) as usize]
            } else {
                &mut self.$field[r as usize]
            }
        }
    };
}

impl Heap {
    pub fn new() -> Self {
        Self::default()
    }
    alloc_moving!(alloc_str_slot, str_raw, str_at_mut, strs, free_strs, gen_strs, HStr);

    /// Shared-constant string (refcount bump only).
    pub fn alloc_str(&mut self, s: Arc<str>) -> Ref {
        self.alloc_str_slot(HStr::Shared(s))
    }

    /// Closure allocation reusing a freed slot's upvals buffer.
    /// Allocate a closure, reusing a freed slot. `proto` is borrowed: a
    /// recycled slot usually already holds the same proto (closures of
    /// one function churning), and skipping the swap avoids an Arc
    /// increment + decrement per creation — measurable when a hot loop
    /// builds thousands of closures.
    pub fn alloc_closure_reuse(
        &mut self,
        proto: &Arc<FunctionProto>,
        upvals: &[Value],
    ) -> Ref {
        self.allocs_since_gc += 1;
        if let Some(r) = self.free_closures.pop() {
            let c = &mut self.closures[r as usize];
            if !Arc::ptr_eq(&c.proto, proto) {
                c.proto = proto.clone();
            }
            debug_assert!(c.upvals.is_empty()); // sweep pre-clears
            c.upvals.extend_from_slice(upvals);
            self.gen_closures.on_alloc(r);
            return r;
        }
        self.closures.push(Closure { proto: proto.clone(), upvals: upvals.to_vec() });
        let r = (self.closures.len() - 1) as Ref;
        self.gen_closures.on_alloc(r);
        r
    }

    /// Runtime-built string: reuses a freed slot's buffer when possible.
    pub fn alloc_str_copy(&mut self, s: &str) -> Ref {
        if self.nursery_on {
            let mut b = self.pool_str_bufs.pop().unwrap_or_default();
            b.push_str(s);
            return self.young_str(HStr::Buf(b));
        }
        self.allocs_since_gc += 1;
        if let Some(r) = self.free_strs.pop() {
            // freed slots arrive pre-cleared (sweep clears + caps capacity)
            let slot = &mut self.strs[r as usize];
            match slot {
                HStr::Buf(b) => {
                    debug_assert!(b.is_empty());
                    b.push_str(s);
                }
                _ => *slot = HStr::Buf(String::from(s)),
            }
            self.gen_strs.on_alloc(r);
            return r;
        }
        self.strs.push(HStr::Buf(String::from(s)));
        let r = (self.strs.len() - 1) as Ref;
        self.gen_strs.on_alloc(r);
        r
    }

    #[inline(always)]
    pub fn str_at(&self, r: Ref) -> &str {
        self.str_raw(r).as_str()
    }

    /// Arc for cross-realm/portable use (copies Buf strings; ropes
    /// flatten first).
    pub fn str_arc(&self, r: Ref) -> Arc<str> {
        match self.str_raw(r) {
            HStr::Shared(a) => a.clone(),
            other => Arc::from(other.as_str()),
        }
    }

    /// Lazy concatenation: build a rope node instead of copying both
    /// sides. Short results are still copied — a flat small string beats
    /// a tree node, and this keeps `a + b` for tiny pieces cheap.
    pub fn alloc_concat(&mut self, a: Ref, b: Ref) -> Ref {
        let FLAT_LIMIT = rope_limit();
        let (la, lb) = (self.str_raw(a).byte_len(), self.str_raw(b).byte_len());
        if la + lb <= FLAT_LIMIT {
            let mut s = String::with_capacity(la + lb);
            s.push_str(self.str_at(a));
            s.push_str(self.str_at(b));
            return self.alloc_str_owned(s);
        }
        let node = RopeNode::cat(self.str_raw(a).to_rope(), self.str_raw(b).to_rope());
        self.alloc_str_slot_pub(HStr::Rope(node, la + lb))
    }

    /// Concatenate a heap string with a plain `&str` (the common
    /// `s + literal` / `s + number` shape) without materializing an
    /// intermediate heap slot for the right-hand side.
    pub fn alloc_concat_str(&mut self, a: Ref, b: &str) -> Ref {
        let FLAT_LIMIT = rope_limit();
        let la = self.str_raw(a).byte_len();
        if la + b.len() <= FLAT_LIMIT {
            let mut s = String::with_capacity(la + b.len());
            s.push_str(self.str_at(a));
            s.push_str(b);
            return self.alloc_str_owned(s);
        }
        let node = RopeNode::cat(self.str_raw(a).to_rope(), RopeNode::leaf(Arc::from(b)));
        self.alloc_str_slot_pub(HStr::Rope(node, la + b.len()))
    }

    /// Store an already-built String (reuses a freed slot's allocation
    /// when the sweep left one).
    pub fn alloc_str_owned(&mut self, s: String) -> Ref {
        if self.nursery_on {
            return self.young_str(HStr::Buf(s));
        }
        self.alloc_str_slot_pub(HStr::Buf(s))
    }

    fn alloc_str_slot_pub(&mut self, h: HStr) -> Ref {
        if self.nursery_on {
            return self.young_str(h);
        }
        self.allocs_since_gc += 1;
        if let Some(r) = self.free_strs.pop() {
            self.strs[r as usize] = h;
            self.gen_strs.on_alloc(r);
            return r;
        }
        self.strs.push(h);
        let r = (self.strs.len() - 1) as Ref;
        self.gen_strs.on_alloc(r);
        r
    }
    alloc_moving!(alloc_obj, obj, obj_mut, objs, free_objs, gen_objs, Obj);
    alloc_moving!(alloc_arr, arr, arr_mut, arrs, free_arrs, gen_arrs, Vec<Value>);
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
        if r & YOUNG_BIT != 0 {
            return; // young containers are traced wholesale
        }
        if self.gen_objs.old.get(r) && !self.gen_objs.dirty.get(r) {
            self.gen_objs.dirty.set(r);
            self.remembered.push(Value::object(r));
        }
    }
    #[inline(always)]
    pub fn barrier_arr(&mut self, r: Ref) {
        if r & YOUNG_BIT != 0 {
            return; // young containers are traced wholesale
        }
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

    /// Re-derive the JIT's arena base-pair tables. MUST be called after
    /// any operation that can move an objs/arrs data pointer (Vec growth,
    /// nursery take/restore) — compiled code reads these words directly.
    #[inline(always)]
    pub fn refresh_bases(&mut self) {
        self.obj_bases = [
            self.objs.as_ptr() as usize,
            self.nursery.objs.as_ptr() as usize,
        ];
        self.arr_bases = [
            self.arrs.as_ptr() as usize,
            self.nursery.arrs.as_ptr() as usize,
        ];
    }

    pub fn needs_gc(&self) -> bool {
        self.allocs_since_gc >= self.gc_threshold
            || self.nursery.bytes >= self.nursery_limit
    }

    /// Allocate an empty array, reusing a freed slot's buffer when
    /// possible (churn workloads would otherwise malloc per array).
    pub fn alloc_arr_empty(&mut self, cap: usize) -> Ref {
        if self.nursery_on {
            let b = self.young_arr_buf(cap);
            return self.young_arr(b);
        }
        self.allocs_since_gc += 1;
        if let Some(r) = self.free_arrs.pop() {
            // freed slots arrive pre-cleared (sweep clears + caps capacity)
            debug_assert!(self.arrs[r as usize].is_empty());
            self.gen_arrs.on_alloc(r);
            return r;
        }
        self.arrs.push(Vec::with_capacity(cap));
        self.refresh_bases();
        let r = (self.arrs.len() - 1) as Ref;
        self.gen_arrs.on_alloc(r);
        r
    }

    /// Fused object literal: final shape + all values in one allocation.
    pub fn alloc_obj_lit(&mut self, shape: &'static ShapeData, values: &[Value]) -> Ref {
        if self.nursery_on {
            let mut o = Obj { shape, ..Obj::default() };
            o.extend_vals(values);
            return self.young_obj(o);
        }
        self.allocs_since_gc += 1;
        if let Some(r) = self.free_objs.pop() {
            let o = &mut self.objs[r as usize];
            debug_assert!(o.vlen == 0);
            o.shape = shape;
            o.extend_vals(values);
            self.gen_objs.on_alloc(r);
            return r;
        }
        let mut o = Obj { shape, ..Obj::default() };
        o.extend_vals(values);
        self.objs.push(o);
        self.refresh_bases();
        let r = (self.objs.len() - 1) as Ref;
        self.gen_objs.on_alloc(r);
        r
    }

    /// Fused array literal: contents in one allocation.
    pub fn alloc_arr_lit(&mut self, values: &[Value]) -> Ref {
        if self.nursery_on {
            let mut b = self.young_arr_buf(values.len());
            b.extend_from_slice(values);
            return self.young_arr(b);
        }
        self.allocs_since_gc += 1;
        if let Some(r) = self.free_arrs.pop() {
            let v = &mut self.arrs[r as usize];
            debug_assert!(v.is_empty());
            v.extend_from_slice(values);
            self.gen_arrs.on_alloc(r);
            return r;
        }
        self.arrs.push(values.to_vec());
        self.refresh_bases();
        let r = (self.arrs.len() - 1) as Ref;
        self.gen_arrs.on_alloc(r);
        r
    }

    /// Allocate an empty object, reusing a freed slot's values buffer.
    pub fn alloc_obj_empty(&mut self) -> Ref {
        if self.nursery_on {
            return self.young_obj(Obj::default());
        }
        self.allocs_since_gc += 1;
        if let Some(r) = self.free_objs.pop() {
            // freed slots arrive pre-cleared: sweep resets shape + values
            debug_assert!(self.objs[r as usize].vlen == 0);
            self.gen_objs.on_alloc(r);
            return r;
        }
        self.objs.push(Obj::default());
        self.refresh_bases();
        let r = (self.objs.len() - 1) as Ref;
        self.gen_objs.on_alloc(r);
        r
    }

    // ---- nursery (young) allocation: bump push, no free-list, no young
    // log; a minor GC evacuates survivors and truncates ----

    #[inline]
    fn young_obj(&mut self, o: Obj) -> Ref {
        let i = self.nursery.objs.len();
        assert!(i < YOUNG_BIT as usize, "nursery overflow");
        self.nursery.bytes += 72 + o.overflow.capacity() * 8;
        self.nursery.objs.push(o);
        self.refresh_bases();
        i as Ref | YOUNG_BIT
    }

    #[inline]
    fn young_arr(&mut self, v: Vec<Value>) -> Ref {
        let i = self.nursery.arrs.len();
        assert!(i < YOUNG_BIT as usize, "nursery overflow");
        // ponytail: alloc-time capacity only; later growth undercounts,
        // allocs_since_gc backstops the trigger
        self.nursery.bytes += 32 + v.capacity() * 8;
        self.nursery.arrs.push(v);
        self.refresh_bases();
        i as Ref | YOUNG_BIT
    }

    #[inline]
    fn young_str(&mut self, s: HStr) -> Ref {
        let i = self.nursery.strs.len();
        assert!(i < YOUNG_BIT as usize, "nursery overflow");
        self.nursery.bytes += 24 + s.byte_len();
        self.nursery.strs.push(s);
        i as Ref | YOUNG_BIT
    }

    /// Recycled Vec for a young array (pool from evacuation harvest).
    fn young_arr_buf(&mut self, cap: usize) -> Vec<Value> {
        match self.pool_arr_bufs.pop() {
            Some(b) if b.capacity() >= cap => b,
            Some(b) => {
                self.pool_arr_bufs.push(b);
                Vec::with_capacity(cap)
            }
            None => Vec::with_capacity(cap),
        }
    }

    // ---- promotion: evacuation target in the old arenas. Sets the old
    // bit and skips the young log — the existing alloc path would clear
    // the old bit and barriers would stop firing (dangling refs) ----

    pub fn promote_obj(&mut self, o: Obj) -> Ref {
        self.promoted_bytes_since_major += 72 + o.overflow.len() * 8;
        let r = match self.free_objs.pop() {
            Some(r) => {
                let old = std::mem::replace(&mut self.objs[r as usize], o);
                // the freed slot kept its overflow buffer — recycle it
                if old.overflow.capacity() > 0 && self.pool_arr_bufs.len() < 131072 {
                    let mut b = old.overflow;
                    b.clear();
                    self.pool_arr_bufs.push(b);
                }
                r
            }
            None => {
                self.objs.push(o);
                self.refresh_bases();
        self.refresh_bases();
                (self.objs.len() - 1) as Ref
            }
        };
        self.gen_objs.old.set(r);
        self.promoted_since_major += 1;
        r
    }

    pub fn promote_arr(&mut self, v: Vec<Value>) -> Ref {
        self.promoted_bytes_since_major += 32 + v.len() * 8;
        let r = match self.free_arrs.pop() {
            Some(r) => {
                let old = std::mem::replace(&mut self.arrs[r as usize], v);
                if old.capacity() > 0 && self.pool_arr_bufs.len() < 131072 {
                    let mut b = old;
                    b.clear();
                    self.pool_arr_bufs.push(b);
                }
                r
            }
            None => {
                self.arrs.push(v);
                self.refresh_bases();
                (self.arrs.len() - 1) as Ref
            }
        };
        self.gen_arrs.old.set(r);
        self.promoted_since_major += 1;
        r
    }

    pub fn promote_str(&mut self, s: HStr) -> Ref {
        self.promoted_bytes_since_major += 24 + s.byte_len();
        let r = match self.free_strs.pop() {
            Some(r) => {
                let old = std::mem::replace(&mut self.strs[r as usize], s);
                if let HStr::Buf(mut b) = old {
                    if b.capacity() > 0 && self.pool_str_bufs.len() < 131072 {
                        b.clear();
                        self.pool_str_bufs.push(b);
                    }
                }
                r
            }
            None => {
                self.strs.push(s);
                (self.strs.len() - 1) as Ref
            }
        };
        self.gen_strs.old.set(r);
        self.promoted_since_major += 1;
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

/// Append JS-style number formatting without intermediate allocations.
/// Integer-to-decimal without core::fmt (measured hot in string concat —
/// the fmt machinery was ~10% of the alloc benchmark).
fn push_i64(out: &mut String, mut v: i64) {
    let mut buf = [0u8; 20];
    let neg = v < 0;
    if !neg {
        v = -v; // negative space covers i64::MIN
    }
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = (b'0' as i64 - v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    if neg {
        i -= 1;
        buf[i] = b'-';
    }
    out.push_str(unsafe { std::str::from_utf8_unchecked(&buf[i..]) });
}

pub fn push_number(out: &mut String, n: f64) {
    use std::fmt::Write;
    if n.is_nan() {
        out.push_str("NaN");
    } else if n.is_infinite() {
        out.push_str(if n > 0.0 { "Infinity" } else { "-Infinity" });
    } else if n == n.trunc() && n.abs() < 1e21 {
        push_i64(out, n as i64);
    } else {
        let _ = write!(out, "{n}");
    }
}

/// Append a value's display form; returns false for aggregate values the
/// caller must route through the slow path.
pub fn display_into(out: &mut String, v: Value, heap: &Heap) -> bool {
    if v.is_number() {
        push_number(out, v.as_number());
        return true;
    }
    match v.kind() {
        Kind::Str(r) => out.push_str(heap.str_at(r)),
        Kind::Bool(b) => out.push_str(if b { "true" } else { "false" }),
        Kind::Null => out.push_str("null"),
        Kind::Undefined => out.push_str("undefined"),
        _ => return false,
    }
    true
}

/// JS-style number formatting: integers print without a fraction.
pub fn fmt_number(n: f64) -> String {
    if n.is_nan() {
        "NaN".into()
    } else if n.is_infinite() {
        if n > 0.0 { "Infinity".into() } else { "-Infinity".into() }
    } else if n == n.trunc() && n.abs() < 1e21 {
        let mut s = String::with_capacity(20);
        push_i64(&mut s, n as i64);
        s
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

/// JS array index semantics: valid only for non-negative integral
/// numbers (arr[-1] and arr[1.5] are property lookups -> undefined).
#[inline(always)]
pub fn array_index(n: f64) -> Option<usize> {
    if n >= 0.0 && n.fract() == 0.0 && n < 4294967296.0 {
        Some(n as usize)
    } else {
        None
    }
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
