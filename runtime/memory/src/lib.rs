//! tsr-memory
//!
//! Per-realm arena heap and value representation. `Ref` handles are indices
//! into their owning realm's heap and must never cross realms — the
//! PortableValue clone boundary (tsr-task) is the only way data moves.
//! M1: bump-style Vec arenas, free-on-realm-death; mark-sweep for the main
//! realm arrives in tsr-gc (P4).

/// Recycled backing buffers held between collections.
///
/// The pool only refills when a minor collection harvests dead nursery
/// slots, so a cap below the number of allocations per cycle sends the
/// remainder to the allocator every cycle and frees the surplus. A 16MB
/// nursery holds roughly 350k small arrays, against the old cap of 131k,
/// which measured a 50% pool hit rate on an array-literal loop.
pub const BUF_POOL_CAP: usize = 262144;

use std::sync::Arc;
use tsc_ir::FunctionProto;

pub mod cells;
pub mod space;
pub use cells::{Closure, Obj, OBJ_INLINE};
use cells::*;
use space::{BornBuf, Old, Young};

/// Heap reference payload: today an index into one of the realm heap's
/// arenas, carried in the low 48 bits of a `Value` so an address fits
/// the same slot later.
pub type Ref = u64;
/// Bits of a `Value` that hold the reference payload.
pub const PAYLOAD_MASK: u64 = (1 << TAG_SHIFT) - 1;

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
const FOREIGN_BASE: u64 = 8;

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
    fn tagged(tag: u64, payload: u64) -> Value {
        debug_assert!(payload & !PAYLOAD_MASK == 0, "payload exceeds 48 bits");
        Value((tag << TAG_SHIFT) | payload)
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
        Value::tagged(TAG_NATIVE, i as u64)
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
    fn payload(self) -> u64 {
        self.0 & PAYLOAD_MASK
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
                // the JIT/TDZ sentinels (4..8) are opaque: no edges, and
                // display/typeof treat them as undefined
                p if p < FOREIGN_BASE => Kind::Undefined,
                _ => Kind::Foreign(p - FOREIGN_BASE),
            },
            TAG_STR => Kind::Str(p),
            TAG_OBJ => Kind::Object(p),
            TAG_ARR => Kind::Array(p),
            TAG_CLOSURE => Kind::Closure(p),
            TAG_CELL => Kind::Cell(p),
            _ => Kind::Native(p as u32),
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
    /// Identity the inline caches and every baked shape guard compare.
    /// Bumped in place when a field representation widens, so all code
    /// that assumed the narrower representation misses its guard.
    id: std::sync::atomic::AtomicU32,
    /// Field names in slot order.
    pub fields: Vec<Arc<str>>,
    /// Per-field representation, 2 bits per slot for the first 32 fields
    /// (later ones are Any). Only ever widens: Int32 -> Number -> Any.
    reprs: std::sync::atomic::AtomicU64,
    transitions: std::sync::RwLock<Vec<(Arc<str>, &'static ShapeData)>>,
}

static SHAPE_IDS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);
static SHAPE_BY_ID: std::sync::OnceLock<
    std::sync::RwLock<std::collections::HashMap<u32, &'static ShapeData>>,
> = std::sync::OnceLock::new();

fn register_shape(id: u32, s: &'static ShapeData) {
    SHAPE_BY_ID
        .get_or_init(Default::default)
        .write()
        .unwrap()
        .insert(id, s);
}

/// The shape currently carrying `id` (compile-time lookups only).
pub fn shape_by_id(id: u32) -> Option<&'static ShapeData> {
    SHAPE_BY_ID.get_or_init(Default::default).read().unwrap().get(&id).copied()
}

/// What a field's values are known to be. Compiled code reading a field
/// of a guarded shape may rely on this without a guard of its own: a
/// store that would violate it widens the shape, which changes the id
/// every such guard compares.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[repr(u8)]
pub enum Repr {
    /// A number with an exact i32 value (and not -0).
    Int32 = 0,
    Number = 1,
    Any = 2,
}

impl Repr {
    pub fn of(v: Value) -> Repr {
        if !v.is_number() {
            return Repr::Any;
        }
        let n = v.as_number();
        if n.fract() == 0.0
            && n >= i32::MIN as f64
            && n <= i32::MAX as f64
            && v.bits() != (-0.0f64).to_bits()
        {
            Repr::Int32
        } else {
            Repr::Number
        }
    }
    fn from_bits(b: u64) -> Repr {
        match b & 3 {
            0 => Repr::Int32,
            1 => Repr::Number,
            _ => Repr::Any,
        }
    }
}

/// Shapes are immutable, process-global, and bounded by the program's
/// distinct field sequences — leaked (`&'static`) so the hot paths
/// (allocation, sweep-clear, transitions, TICs) never touch an atomic
/// refcount.
pub fn empty_shape() -> &'static ShapeData {
    static EMPTY: std::sync::OnceLock<&'static ShapeData> = std::sync::OnceLock::new();
    EMPTY.get_or_init(|| {
        let s: &'static ShapeData = Box::leak(Box::new(ShapeData {
            id: std::sync::atomic::AtomicU32::new(0),
            fields: Vec::new(),
            reprs: std::sync::atomic::AtomicU64::new(0),
            transitions: std::sync::RwLock::new(Vec::new()),
        }));
        register_shape(0, s);
        s
    })
}

impl ShapeData {
    #[inline(always)]
    pub fn id(&self) -> u32 {
        self.id.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn repr(&self, slot: usize) -> Repr {
        if slot >= 32 {
            return Repr::Any;
        }
        Repr::from_bits(self.reprs.load(std::sync::atomic::Ordering::Relaxed) >> (2 * slot))
    }

    /// Widen `slot` to cover `r`. A change takes a fresh id so every
    /// cache and compiled guard keyed on the old one misses.
    pub fn widen(&'static self, slot: usize, r: Repr) {
        if slot >= 32 || r == Repr::Int32 {
            return;
        }
        use std::sync::atomic::Ordering::Relaxed;
        loop {
            let cur = self.reprs.load(Relaxed);
            if Repr::from_bits(cur >> (2 * slot)) >= r {
                return;
            }
            let next = (cur & !(3u64 << (2 * slot))) | ((r as u64) << (2 * slot));
            if self.reprs.compare_exchange(cur, next, Relaxed, Relaxed).is_ok() {
                let id = SHAPE_IDS.fetch_add(1, Relaxed);
                self.id.store(id, Relaxed);
                register_shape(id, self);
                return;
            }
        }
    }

    /// An object moving from `prev` to this shape keeps its values, so
    /// this shape must cover everything `prev` allowed for shared slots.
    pub fn absorb(&'static self, prev: &ShapeData) {
        let pr = prev.reprs.load(std::sync::atomic::Ordering::Relaxed);
        if pr == 0 {
            return;
        }
        for slot in 0..prev.fields.len().min(32) {
            let r = Repr::from_bits(pr >> (2 * slot));
            if r != Repr::Int32 {
                self.widen(slot, r);
            }
        }
    }

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
        let id = SHAPE_IDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // the new slot starts narrowest; its first value widens it
        let next: &'static ShapeData = Box::leak(Box::new(ShapeData {
            id: std::sync::atomic::AtomicU32::new(id),
            fields,
            reprs: std::sync::atomic::AtomicU64::new(
                self.reprs.load(std::sync::atomic::Ordering::Relaxed),
            ),
            transitions: std::sync::RwLock::new(Vec::new()),
        }));
        register_shape(id, next);
        tr.push((name, next));
        next
    }
}

/// Growable bitmap (1 bit per arena slot).
#[derive(Default)]
pub struct Bitmap(Vec<u64>);

impl Bitmap {
    /// Raw words, for the layout probes (the JIT reads old/dirty bits
    /// inline to decide whether a store needs the barrier helper).
    pub fn words(&self) -> &Vec<u64> {
        &self.0
    }
    pub fn words_mut(&mut self) -> &mut Vec<u64> {
        &mut self.0
    }
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

/// Reusable GC scratch: mark vectors for the arena kinds (strings,
/// foreigns), the trace worklist, and the minor's scan queue. Lives on
/// the heap so a collection never allocates proportionally to heap size.
#[derive(Default)]
pub struct GcScratch {
    pub strs: Vec<bool>,
    pub foreigns: Vec<bool>,
    pub ystrs: Bitmap,
    pub yforeigns: Bitmap,
    pub work: Vec<Value>,
    /// Forwarding table for nursery strings (u32::MAX = unforwarded).
    pub fwd_strs: Vec<u32>,
    /// Cells whose edges still need forwarding (addresses; foreigns as
    /// `Value::foreign` bits in `scan_foreign`).
    pub scan: Vec<Ref>,
    pub scan_foreign: Vec<Ref>,
    /// `promoted_bytes_since_major` at the end of the last minor, so the
    /// pretenure policy can see what this minor alone copied.
    pub evac_base: usize,
}

/// Per-arena generational state: age bits, barrier dirty bits, and the log
/// of slots allocated since the last collection (the nursery).
#[derive(Default)]
pub struct GenState {
    pub old: Bitmap,
    pub dirty: Bitmap,
    /// u32 on purpose: the JIT pushes 4-byte entries.
    pub young: Vec<u32>,
}

impl GenState {
    #[inline(always)]
    fn on_alloc(&mut self, r: Ref) {
        // invariant: every slot on a free list has old=0 and dirty=0 —
        // the major sweep rebuilds `old` from marks (unmarked ⇒ 0) and
        // clears all dirty bits; minor-freed slots were never old; and
        // free_foreign clears both explicitly
        debug_assert!(!self.old.get(r) && !self.dirty.get(r));
        self.young.push(r as u32);
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
    /// The node lives in `Heap::ropes`; read it through `Heap::str_at`.
    Rope(u32, u32),
    /// Short string carrying its bytes in the slot itself. Most runtime
    /// strings — rendered numbers, keys, small concatenations — fit, and
    /// this is what keeps them off the allocator entirely.
    Inline(u8, [u8; STR_INLINE]),
}

/// Widest inline string: the largest array that keeps `HStr` at 32
/// bytes. 31 tips the slot to 40 (`slot_stays_32_bytes` guards this).
pub const STR_INLINE: usize = 30;

impl HStr {
    /// Inline `s` if it fits, else `None`.
    #[inline(always)]
    pub fn inline(s: &str) -> Option<HStr> {
        if s.len() > STR_INLINE {
            return None;
        }
        let mut b = [0u8; STR_INLINE];
        b[..s.len()].copy_from_slice(s.as_bytes());
        Some(HStr::Inline(s.len() as u8, b))
    }
}

/// One node of a lazy concatenation tree, living in `Heap::ropes`.
/// Creating one is a `Vec::push` — no allocator call — which is what
/// makes deferring cheap enough to use for short concatenations too.
///
/// `flat` caches the flattened bytes on first read, so reading a string
/// never needs `&mut Heap`.
#[derive(Debug)]
pub struct RopeNodeData {
    left: RopeSide,
    right: RopeSide,
    len: u32,
    flat: std::sync::OnceLock<Arc<str>>,
}

/// One side of a rope node. Short pieces (numbers, punctuation, small
/// literals) live in the node itself, so `s + i` allocates nothing.
#[derive(Debug)]
enum RopeSide {
    Shared(Arc<str>),
    Node(u32),
    Inline(u8, [u8; ROPE_INLINE]),
}

pub const ROPE_INLINE: usize = 15;

impl RopeSide {
    fn from_str(s: &str) -> RopeSide {
        if s.len() <= ROPE_INLINE {
            let mut b = [0u8; ROPE_INLINE];
            b[..s.len()].copy_from_slice(s.as_bytes());
            RopeSide::Inline(s.len() as u8, b)
        } else {
            RopeSide::Shared(Arc::from(s))
        }
    }
    fn dup(&self) -> RopeSide {
        match self {
            RopeSide::Shared(a) => RopeSide::Shared(a.clone()),
            RopeSide::Node(i) => RopeSide::Node(*i),
            RopeSide::Inline(n, b) => RopeSide::Inline(*n, *b),
        }
    }
}

impl HStr {
    /// Flat bytes. Ropes live in the heap's arena and must be read with
    /// `Heap::str_at`; this returns "" for them.
    #[inline(always)]
    pub fn as_str(&self) -> &str {
        match self {
            HStr::Shared(a) => a,
            HStr::Buf(b) => b,
            HStr::Inline(n, b) => unsafe {
                // only ever built from a `&str`
                std::str::from_utf8_unchecked(b.get_unchecked(..*n as usize))
            },
            HStr::Rope(..) => {
                debug_assert!(false, "rope must be read via Heap::str_at");
                ""
            }
        }
    }

    /// Byte length without forcing a rope to flatten (the collector and
    /// size accounting must not materialize lazy strings).
    #[inline(always)]
    pub fn byte_len(&self) -> usize {
        match self {
            HStr::Shared(a) => a.len(),
            HStr::Buf(b) => b.len(),
            HStr::Rope(_, n) => *n as usize,
            HStr::Inline(n, _) => *n as usize,
        }
    }


}

/// Young-generation tag for string refs: bit 31 of the payload indexes
/// `nursery.strs`; minor GC evacuates survivors into `strs`. Objects and
/// arrays are young by address (`Heap::young`).
pub const YOUNG_BIT: u64 = 1 << 31;

#[inline(always)]
pub fn is_young(r: Ref) -> bool {
    r & YOUNG_BIT != 0
}

/// Young strings: a push-allocated arena; minor GC evacuates survivors
/// into `strs` and truncates. (Objects and arrays live in `Heap::young`.)
#[derive(Default)]
pub struct Nursery {
    pub strs: Vec<HStr>,
    /// Approximate string bytes allocated this window.
    pub bytes: usize,
}

pub struct Heap {
    pub strs: Vec<HStr>,
    /// Bump arena of rope nodes. Creating a lazy concatenation is a push
    /// here, not an allocator call. Reclaimed by `compact_ropes()` at
    /// every collection, which copies the still-reachable trees into a
    /// fresh arena and rewrites the indices.
    pub ropes: Vec<RopeNodeData>,
    /// String slots currently holding a rope — the roots compaction walks
    /// from. Young entries are re-registered by `promote_str` when they
    /// survive evacuation, since evacuation changes their ref.
    pub rope_slots: Vec<Ref>,
    /// Live node count at the last compaction. Compaction costs O(live),
    /// so running it at every GC would re-copy a growing accumulator each
    /// cycle — O(n²) again. Doubling makes it amortized O(1) per node.
    rope_live: usize,
    pub foreigns: Vec<Foreign>,
    // free lists rebuilt by the sweep (tsr-gc); alloc reuses them.
    pub free_strs: Vec<u32>,
    pub free_foreigns: Vec<u32>,
    pub allocs_since_gc: usize,
    pub gc_threshold: usize,
    // generational state of the arena kinds (sticky in-place mark-sweep)
    pub gen_strs: GenState,
    pub gen_foreigns: GenState,
    /// Bump nursery for objects, arrays and element chunks.
    pub young: Young,
    /// Old space: promoted cells, closures, cells, pretenured allocations.
    pub old: Old,
    /// Old-space cells allocated since the last minor (the young log of
    /// the old space): scanned as roots by the next minor, which ages the
    /// reachable ones and frees the rest.
    pub born_old: Vec<Value>,
    /// Old-space element chunks allocated since the last minor (reached
    /// through their owner; listed so a dead owner's chunk is freed too).
    pub born_elems: Vec<Ref>,
    /// Boxed cells the JIT took off a free list since the last minor.
    pub born_buf: BornBuf,
    /// Last proto passed to `hold_proto` (see the free function).
    held_last: usize,
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
    /// Pretenure mode (a word so the JIT can probe and test it): when the
    /// last minor showed survivors are the rule, allocate straight into
    /// the old arenas and let the young log mark them in place instead of
    /// copying every one of them out of the nursery. Flipped only at the
    /// end of a minor, when the nursery is empty, so no young ref exists
    /// while allocation goes old. TSC_PRETENURE=1 pins it on.
    pub pretenure: usize,
    pub pretenure_fixed: bool,
    /// Minors left that may age the born log without tracing. Set to 1
    /// by a traced minor that measured >= 90% survival in pretenure mode.
    pub pretenure_skip: u8,
    /// Detached string buffers harvested from dead nursery slots.
    pub pool_str_bufs: Vec<String>,
}

impl Default for Heap {
    fn default() -> Self {
        let nursery_limit = std::env::var("TSC_NURSERY_BYTES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(16 << 20); // 16MB: churn live-sets die in-nursery (measured)
        Heap {
            strs: Vec::new(),
            ropes: Vec::new(),
            rope_slots: Vec::new(),
            rope_live: 0,
            foreigns: Vec::new(),
            free_strs: Vec::new(),
            free_foreigns: Vec::new(),
            allocs_since_gc: 0,
            gc_threshold: 1 << 18, // 256k allocations between collections
            gen_strs: GenState::default(),
            gen_foreigns: GenState::default(),
            young: Young::new(nursery_limit),
            old: Old::new(),
            born_old: Vec::new(),
            born_elems: Vec::new(),
            // 64K entries: a full buffer triggers a minor, so this bounds how
            // long the JIT recycles free cells before the log is swept
            born_buf: BornBuf::new(64 << 10),
            held_last: 0,
            remembered: Vec::new(),
            promoted_since_major: 0,
            promoted_bytes_since_major: 0,
            remembered_peak: 0,
            gc_scratch: GcScratch::default(),
            nursery: Nursery::default(),
            nursery_limit,
            nursery_on: std::env::var_os("TSC_NO_NURSERY").is_none(),
            pretenure: std::env::var_os("TSC_PRETENURE").is_some() as usize,
            pretenure_fixed: std::env::var_os("TSC_PRETENURE").is_some(),
            pretenure_skip: 0,
            pool_str_bufs: Vec::new(),
        }
    }
}

macro_rules! alloc {
    ($fn_name:ident, $get:ident, $get_mut:ident, $field:ident, $free:ident, $gen:ident, $t:ty) => {
        pub fn $fn_name(&mut self, v: $t) -> Ref {
            self.allocs_since_gc += 1;
            if let Some(r) = self.$free.pop().map(|r| r as Ref) {
                self.$field[r as usize] = v;
                self.$gen.on_alloc(r);
                return r;
            }
            self.$field.push(v);
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
            if let Some(r) = self.$free.pop().map(|r| r as Ref) {
                self.$field[r as usize] = v;
                self.$gen.on_alloc(r);
                return r;
            }
            self.$field.push(v);
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

    /// A scratch buffer from the reuse pool with room for `n` bytes.
    /// Pops until one is big enough rather than growing a small buffer —
    /// `reserve` on a too-small pooled buffer is a realloc, which is the
    /// allocator call the pool exists to avoid.
    #[inline]
    fn take_str_buf(&mut self, n: usize) -> String {
        while let Some(b) = self.pool_str_bufs.pop() {
            if b.capacity() >= n {
                return b;
            }
        }
        String::with_capacity(n.max(32))
    }

    /// Runtime-built string: reuses a freed slot's buffer when possible.
    pub fn alloc_str_copy(&mut self, s: &str) -> Ref {
        if let Some(h) = HStr::inline(s) {
            return self.alloc_str_slot_pub(h);
        }
        if self.young_on() {
            let mut b = self.pool_str_bufs.pop().unwrap_or_default();
            b.push_str(s);
            return self.young_str(HStr::Buf(b));
        }
        self.allocs_since_gc += 1;
        if let Some(r) = self.free_strs.pop().map(|r| r as Ref) {
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
        match self.str_raw(r) {
            HStr::Shared(a) => a,
            HStr::Buf(b) => b,
            HStr::Inline(..) => self.str_raw(r).as_str(),
            HStr::Rope(n, _) => self.rope_str(*n),
        }
    }

    /// Arc for cross-realm/portable use (copies Buf strings; ropes
    /// flatten first).
    pub fn str_arc(&self, r: Ref) -> Arc<str> {
        match self.str_raw(r) {
            HStr::Shared(a) => a.clone(),
            HStr::Rope(n, _) => self.rope_arc(*n),
            other => Arc::from(other.as_str()),
        }
    }

    /// Lazy concatenation: push a rope node instead of copying both
    /// sides. Short results are still copied — a flat small string beats
    /// a tree node, and this keeps `a + b` for tiny pieces cheap.
    pub fn alloc_concat(&mut self, a: Ref, b: Ref) -> Ref {
        let flat_limit = rope_limit();
        let (la, lb) = (self.str_raw(a).byte_len(), self.str_raw(b).byte_len());
        if la + lb <= flat_limit {
            if la + lb <= STR_INLINE {
                let mut buf = [0u8; STR_INLINE];
                buf[..la].copy_from_slice(self.str_at(a).as_bytes());
                buf[la..la + lb].copy_from_slice(self.str_at(b).as_bytes());
                return self.alloc_str_slot_pub(HStr::Inline((la + lb) as u8, buf));
            }
            let mut s = self.take_str_buf(la + lb);
            s.push_str(self.str_at(a));
            s.push_str(self.str_at(b));
            return self.alloc_str_owned(s);
        }
        let l = self.side_of(a);
        let r = self.side_of(b);
        let n = self.push_rope(l, r, (la + lb) as u32);
        self.alloc_rope_slot(n, (la + lb) as u32)
    }

    /// `s + <integer>` straight into an inline slot.
    ///
    /// The general path renders the number into a scratch `String` and
    /// then copies that into the slot; a template literal like
    /// `` `s${i % 10}` `` does that five million times in the alloc
    /// benchmark for a two-character result. Returns `None` when the
    /// result would not fit inline, leaving the caller on the general
    /// path.
    /// `alloc_concat_int` into the old generation: for the realm's
    /// small-int concat cache, whose entries must not move.
    pub fn concat_int_old(&mut self, a: Ref, v: i64) -> Option<Ref> {
        let la = self.str_raw(a).byte_len();
        let mut d = [0u8; 20];
        let start = fmt_i64(&mut d, v);
        let dl = d.len() - start;
        if la + dl > STR_INLINE {
            return None;
        }
        let mut buf = [0u8; STR_INLINE];
        buf[..la].copy_from_slice(self.str_at(a).as_bytes());
        buf[la..la + dl].copy_from_slice(&d[start..]);
        Some(self.promote_str(HStr::Inline((la + dl) as u8, buf)))
    }

    pub fn alloc_concat_int(&mut self, a: Ref, v: i64) -> Option<Ref> {
        let la = self.str_raw(a).byte_len();
        let mut d = [0u8; 20];
        let start = fmt_i64(&mut d, v);
        let dl = d.len() - start;
        if la + dl > STR_INLINE {
            return None;
        }
        let mut buf = [0u8; STR_INLINE];
        buf[..la].copy_from_slice(self.str_at(a).as_bytes());
        buf[la..la + dl].copy_from_slice(&d[start..]);
        Some(self.alloc_str_slot_pub(HStr::Inline((la + dl) as u8, buf)))
    }

    /// Concatenate a heap string with a plain `&str` (the common
    /// `s + literal` / `s + number` shape) without materializing an
    /// intermediate heap slot for the right-hand side.
    pub fn alloc_concat_str(&mut self, a: Ref, b: &str) -> Ref {
        let flat_limit = rope_limit();
        let la = self.str_raw(a).byte_len();
        if la + b.len() <= flat_limit {
            if la + b.len() <= STR_INLINE {
                let mut buf = [0u8; STR_INLINE];
                buf[..la].copy_from_slice(self.str_at(a).as_bytes());
                buf[la..la + b.len()].copy_from_slice(b.as_bytes());
                return self.alloc_str_slot_pub(HStr::Inline((la + b.len()) as u8, buf));
            }
            let mut s = self.take_str_buf(la + b.len());
            s.push_str(self.str_at(a));
            s.push_str(b);
            return self.alloc_str_owned(s);
        }
        let l = self.side_of(a);
        let n = self.push_rope(l, RopeSide::from_str(b), (la + b.len()) as u32);
        self.alloc_rope_slot(n, (la + b.len()) as u32)
    }

    /// One side of a new rope node, taken from an existing string slot.
    /// A rope operand is referenced by index (no copy); a flat one is
    /// inlined when short, shared otherwise.
    fn side_of(&mut self, r: Ref) -> RopeSide {
        match self.str_raw(r) {
            HStr::Rope(n, _) => RopeSide::Node(*n),
            HStr::Shared(a) if a.len() > ROPE_INLINE => RopeSide::Shared(a.clone()),
            other => RopeSide::from_str(other.as_str()),
        }
    }

    fn push_rope(&mut self, left: RopeSide, right: RopeSide, len: u32) -> u32 {
        self.ropes.push(RopeNodeData { left, right, len, flat: std::sync::OnceLock::new() });
        (self.ropes.len() - 1) as u32
    }

    fn alloc_rope_slot(&mut self, node: u32, len: u32) -> Ref {
        let r = self.alloc_str_slot_pub(HStr::Rope(node, len));
        self.rope_slots.push(r);
        r
    }

    /// Flat bytes of a rope, cached in the node on first read.
    pub fn rope_str(&self, idx: u32) -> &str {
        let n = &self.ropes[idx as usize];
        n.flat.get_or_init(|| self.flatten(idx))
    }

    pub fn rope_arc(&self, idx: u32) -> Arc<str> {
        let n = &self.ropes[idx as usize];
        n.flat.get_or_init(|| self.flatten(idx)).clone()
    }

    /// Walk the tree left-to-right appending leaves. Iterative: an
    /// accumulator loop builds a left-leaning chain tens of thousands of
    /// nodes deep, which would overflow the Rust stack if recursive.
    fn flatten(&self, idx: u32) -> Arc<str> {
        let mut out = String::with_capacity(self.ropes[idx as usize].len as usize);
        // stack of pending sides, in reverse visit order
        let mut stack: Vec<&RopeSide> = Vec::new();
        let root = &self.ropes[idx as usize];
        stack.push(&root.right);
        stack.push(&root.left);
        while let Some(side) = stack.pop() {
            match side {
                RopeSide::Shared(a) => out.push_str(a),
                RopeSide::Inline(n, b) => {
                    out.push_str(std::str::from_utf8(&b[..*n as usize]).unwrap_or(""))
                }
                RopeSide::Node(i) => {
                    let n = &self.ropes[*i as usize];
                    // an already-flattened subtree short-circuits the walk
                    if let Some(f) = n.flat.get() {
                        out.push_str(f);
                    } else {
                        stack.push(&n.right);
                        stack.push(&n.left);
                    }
                }
            }
        }
        Arc::from(out)
    }

    /// Reclaim the rope arena: copy the trees still reachable from string
    /// slots into a fresh arena and rewrite every index. Run at the start
    /// of a collection, before evacuation moves any string ref.
    ///
    /// Deliberately *not* a flatten: flattening the live trees at every
    /// GC re-copies a growing accumulator each cycle, which puts the O(n²)
    /// back that the ropes exist to remove.
    pub fn compact_ropes(&mut self) {
        if self.ropes.is_empty() {
            self.rope_slots.clear();
            self.rope_live = 0;
            return;
        }
        if self.ropes.len() < self.rope_live * 2 + 4096 {
            // not enough garbage yet to pay for the copy — but young refs
            // must still go: evacuation is about to move them, and
            // `promote_str` re-registers the survivors under the new ref
            self.rope_slots.retain(|r| !is_young(*r));
            return;
        }
        let old = std::mem::take(&mut self.ropes);
        let slots = std::mem::take(&mut self.rope_slots);
        let mut remap = vec![u32::MAX; old.len()];
        let mut new: Vec<RopeNodeData> = Vec::new();
        let mut kept = Vec::with_capacity(slots.len());
        let mut work: Vec<(u32, bool)> = Vec::new();
        for r in slots {
            let idx = match self.str_raw(r) {
                HStr::Rope(i, _) => *i,
                _ => continue, // slot swept or overwritten since
            };
            if remap[idx as usize] == u32::MAX {
                // iterative post-order: children copied before parents so
                // their new indices are known when the parent is built
                work.push((idx, false));
                while let Some((i, done)) = work.pop() {
                    if remap[i as usize] != u32::MAX {
                        continue;
                    }
                    let n = &old[i as usize];
                    if !done {
                        work.push((i, true));
                        for side in [&n.left, &n.right] {
                            if let RopeSide::Node(c) = side {
                                if remap[*c as usize] == u32::MAX {
                                    work.push((*c, false));
                                }
                            }
                        }
                        continue;
                    }
                    let fix = |side: &RopeSide| match side {
                        RopeSide::Node(c) => RopeSide::Node(remap[*c as usize]),
                        other => other.dup(),
                    };
                    let flat = std::sync::OnceLock::new();
                    if let Some(f) = n.flat.get() {
                        let _ = flat.set(f.clone());
                    }
                    new.push(RopeNodeData {
                        left: fix(&n.left),
                        right: fix(&n.right),
                        len: n.len,
                        flat,
                    });
                    remap[i as usize] = (new.len() - 1) as u32;
                }
            }
            let ni = remap[idx as usize];
            if let HStr::Rope(i, _) = self.str_at_mut(r) {
                *i = ni;
            }
            // young slots get re-registered by `promote_str` under their
            // post-evacuation ref; keeping the pre-move ref would dangle
            if !is_young(r) {
                kept.push(r);
            }
        }
        // a slot can be registered twice (freed, then re-used by another
        // rope before the next GC) — collapse so the list can't grow
        kept.sort_unstable();
        kept.dedup();
        self.rope_live = new.len();
        self.ropes = new;
        self.rope_slots = kept;
    }

    /// Store an already-built String (reuses a freed slot's allocation
    /// when the sweep left one).
    pub fn alloc_str_owned(&mut self, s: String) -> Ref {
        if self.young_on() {
            return self.young_str(HStr::Buf(s));
        }
        self.alloc_str_slot_pub(HStr::Buf(s))
    }

    fn alloc_str_slot_pub(&mut self, h: HStr) -> Ref {
        if self.young_on() {
            return self.young_str(h);
        }
        self.allocs_since_gc += 1;
        if let Some(r) = self.free_strs.pop().map(|r| r as Ref) {
            self.strs[r as usize] = h;
            self.gen_strs.on_alloc(r);
            return r;
        }
        self.strs.push(h);
        let r = (self.strs.len() - 1) as Ref;
        self.gen_strs.on_alloc(r);
        r
    }
    alloc!(alloc_foreign, foreign, foreign_mut, foreigns, free_foreigns, gen_foreigns, Foreign);

    /// The only sanctioned way to free a foreign slot outside the sweep:
    /// keeps generation bookkeeping consistent for slot reuse.
    pub fn free_foreign(&mut self, r: Ref) {
        self.foreigns[r as usize] = Foreign::Free;
        self.gen_foreigns.old.clear(r);
        self.gen_foreigns.dirty.clear(r);
        self.free_foreigns.push(r as u32);
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

    /// Allocate into the nursery? Off entirely (TSC_NO_NURSERY) or
    /// temporarily while pretenuring.
    #[inline(always)]
    pub fn young_on(&self) -> bool {
        self.nursery_on && self.pretenure == 0
    }

    pub fn needs_gc(&self) -> bool {
        self.young.top >= self.young.limit
            || self.allocs_since_gc >= self.gc_threshold
            || self.nursery.bytes >= self.nursery_limit
            // old-space allocation (pretenure, closures, cells) collects on a
            // quarter of the nursery budget: those cells are swept in place,
            // so a shorter cycle costs little and bounds the garbage held
            || self.old.bytes_since_minor() >= self.nursery_limit / 4
            || self.born_buf.is_full()
    }

    pub fn promote_str(&mut self, s: HStr) -> Ref {
        self.promoted_bytes_since_major += 24 + s.byte_len();
        let is_rope = matches!(s, HStr::Rope(..));
        let r = match self.free_strs.pop().map(|r| r as Ref) {
            Some(r) => {
                let old = std::mem::replace(&mut self.strs[r as usize], s);
                if let HStr::Buf(mut b) = old {
                    if b.capacity() > 0 && self.pool_str_bufs.len() < BUF_POOL_CAP {
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
        if is_rope {
            self.rope_slots.push(r);
        }
        r
    }
}


// ---- bump-heap cells: objects, arrays, element chunks, closures, cells ----
impl Heap {
    #[inline]
    fn young_str(&mut self, s: HStr) -> Ref {
        let i = self.nursery.strs.len();
        assert!(i < YOUNG_BIT as usize, "nursery overflow");
        self.nursery.bytes += 24 + s.byte_len();
        self.nursery.strs.push(s);
        i as Ref | YOUNG_BIT
    }

    /// Is this obj/arr/elems address in the nursery?
    #[inline(always)]
    pub fn young(&self, a: Ref) -> bool {
        self.young.contains(a)
    }

    /// Old-space allocation for the kinds that never move (closures,
    /// cells) and for pretenured / large cells. A bump-allocated cell is
    /// in the born range the minor walks; a free-list cell is logged in
    /// `born_old` / `born_elems` (`log` builds the entry).
    #[inline(always)]
    fn alloc_old_cell(&mut self, kind: u64, words: usize, len: usize) -> Ref {
        self.allocs_since_gc += 1;
        let a = self.old.alloc(words);
        set_meta(a, meta(kind, words, len));
        if !self.old.in_born_range(a) {
            if kind == K_ELEMS {
                self.born_elems.push(a);
            } else {
                self.born_old.push(Value::tagged(cell_tag(kind), a));
            }
        }
        a
    }

    /// Element chunk with room for `cap` values, in the generation of
    /// its owner (`young_owner`). Big chunks go old outright: copying
    /// them out of the nursery would dominate a minor.
    fn alloc_elems(&mut self, cap: usize, young_owner: bool) -> Ref {
        let cap = cap.max(1);
        let words = 1 + cap;
        let e = if young_owner && cap <= ELEMS_YOUNG_MAX {
            let a = self.young.alloc(words);
            set_meta(a, meta(K_ELEMS, words, cap));
            a
        } else {
            let a = self.alloc_old_cell(K_ELEMS, words, cap);
            a
        };
        // slack past the live length is never read, but zero it anyway so
        // a debug walk never trips on stale bits
        elems_slice_mut(e, cap).fill(Value::UNDEFINED);
        e
    }

    // ---- objects ----

    #[inline(always)]
    pub fn obj(&self, r: Ref) -> &Obj {
        debug_assert_eq!(kind_of(meta_at(r)), K_OBJ);
        unsafe { &*(r as *const Obj) }
    }
    #[inline(always)]
    pub fn obj_mut(&mut self, r: Ref) -> &mut Obj {
        debug_assert_eq!(kind_of(meta_at(r)), K_OBJ);
        unsafe { &mut *(r as *mut Obj) }
    }

    /// Allocate an empty object.
    pub fn alloc_obj_empty(&mut self) -> Ref {
        let a = if self.young_on() {
            let a = self.young.alloc(OBJ_WORDS);
            set_meta(a, meta(K_OBJ, OBJ_WORDS, 0));
            a
        } else {
            let a = self.alloc_old_cell(K_OBJ, OBJ_WORDS, 0);
            a
        };
        let o = unsafe { &mut *(a as *mut Obj) };
        o.shape = empty_shape();
        o.inline = [Value::UNDEFINED; OBJ_INLINE];
        o.spill = 0;
        a
    }

    /// Host-built object: old space, so Rust code may hold the ref across
    /// safepoints (module namespaces, native handles, rehydrated values).
    /// The v1 arena put every Rust-built object there too.
    pub fn alloc_obj_host(&mut self) -> Ref {
        let a = self.alloc_old_cell(K_OBJ, OBJ_WORDS, 0);
        let o = unsafe { &mut *(a as *mut Obj) };
        o.shape = empty_shape();
        o.inline = [Value::UNDEFINED; OBJ_INLINE];
        o.spill = 0;
        a
    }
    /// Host-built array (see `alloc_obj_host`).
    pub fn alloc_arr_host(&mut self, values: &[Value]) -> Ref {
        let a = self.alloc_old_cell(K_ARR, ARR_WORDS, 0);
        let e = self.alloc_elems(values.len().max(4), false);
        set_word(a, 1, e);
        elems_slice_mut(e, values.len()).copy_from_slice(values);
        set_meta(a, with_len(meta_at(a), values.len()));
        a
    }

    /// Fused object literal: final shape + all values in one allocation.
    pub fn alloc_obj_lit(&mut self, shape: &'static ShapeData, values: &[Value]) -> Ref {
        debug_assert_eq!(shape.fields.len(), values.len());
        let a = self.alloc_obj_empty();
        for (i, &v) in values.iter().enumerate() {
            shape.widen(i, Repr::of(v));
        }
        let n_inline = values.len().min(OBJ_INLINE);
        let spill = if values.len() > OBJ_INLINE {
            let young = self.young(a);
            let e = self.alloc_elems(values.len() - OBJ_INLINE, young);
            elems_slice_mut(e, values.len() - OBJ_INLINE).copy_from_slice(&values[OBJ_INLINE..]);
            e
        } else {
            0
        };
        let o = unsafe { &mut *(a as *mut Obj) };
        o.shape = shape;
        o.inline[..n_inline].copy_from_slice(&values[..n_inline]);
        o.spill = spill;
        o.meta = with_len(o.meta, values.len());
        a
    }

    /// Append a value slot (the shape already has the field).
    pub fn obj_push_val(&mut self, r: Ref, v: Value) {
        let i = self.obj(r).vlen();
        self.obj(r).shape.widen(i, Repr::of(v));
        if i < OBJ_INLINE {
            let o = self.obj_mut(r);
            o.inline[i] = v;
            o.meta = with_len(o.meta, i + 1);
            return;
        }
        let k = i - OBJ_INLINE;
        if k >= self.obj(r).spill_cap() {
            let young = self.young(r);
            let e = self.alloc_elems((k + 1).max(4).next_power_of_two(), young);
            let old = self.obj(r).spill;
            if old != 0 {
                elems_slice_mut(e, k).copy_from_slice(elems_slice(old, k));
            }
            self.obj_mut(r).spill = e;
        }
        let o = self.obj_mut(r);
        elems_slice_mut(o.spill, k + 1)[k] = v;
        o.meta = with_len(o.meta, i + 1);
    }

    /// Named store: existing slot, or a shape transition + new slot.
    pub fn obj_set(&mut self, r: Ref, name: Arc<str>, v: Value) {
        match self.obj(r).shape.slot_of(&name) {
            Some(i) => self.obj_mut(r).store(i, v),
            None => {
                let prev = self.obj(r).shape;
                let next = prev.with_field(name);
                next.absorb(prev);
                self.obj_mut(r).shape = next;
                self.obj_push_val(r, v);
            }
        }
    }

    // ---- arrays ----

    #[inline(always)]
    pub fn arr(&self, r: Ref) -> &[Value] {
        debug_assert_eq!(kind_of(meta_at(r)), K_ARR);
        elems_slice(word(r, 1), len_of(meta_at(r)))
    }
    #[inline(always)]
    pub fn arr_mut(&mut self, r: Ref) -> &mut [Value] {
        debug_assert_eq!(kind_of(meta_at(r)), K_ARR);
        elems_slice_mut(word(r, 1), len_of(meta_at(r)))
    }
    #[inline(always)]
    pub fn arr_len(&self, r: Ref) -> usize {
        len_of(meta_at(r))
    }

    /// Empty array with room for `cap`.
    pub fn alloc_arr_empty(&mut self, cap: usize) -> Ref {
        let young = self.young_on();
        let a = if young {
            let a = self.young.alloc(ARR_WORDS);
            set_meta(a, meta(K_ARR, ARR_WORDS, 0));
            a
        } else {
            let a = self.alloc_old_cell(K_ARR, ARR_WORDS, 0);
            a
        };
        let e = self.alloc_elems(cap.max(4), young);
        set_word(a, 1, e);
        a
    }

    /// Fused array literal: contents in one allocation.
    pub fn alloc_arr_lit(&mut self, values: &[Value]) -> Ref {
        let a = self.alloc_arr_empty(values.len());
        elems_slice_mut(word(a, 1), values.len()).copy_from_slice(values);
        set_meta(a, with_len(meta_at(a), values.len()));
        a
    }

    pub fn arr_push(&mut self, r: Ref, v: Value) {
        let n = len_of(meta_at(r));
        let e = word(r, 1);
        let e = if n >= elems_cap(e) {
            let young = self.young(r);
            let ne = self.alloc_elems(n * 2, young);
            elems_slice_mut(ne, n).copy_from_slice(elems_slice(e, n));
            set_word(r, 1, ne);
            ne
        } else {
            e
        };
        elems_slice_mut(e, n + 1)[n] = v;
        set_meta(r, with_len(meta_at(r), n + 1));
    }

    /// Truncate or extend (with undefined) to `len`.
    pub fn arr_set_len(&mut self, r: Ref, len: usize) {
        while len_of(meta_at(r)) < len {
            self.arr_push(r, Value::UNDEFINED);
        }
        set_meta(r, with_len(meta_at(r), len));
    }

    // ---- closures and cells: old space, never move ----

    #[inline(always)]
    pub fn closure(&self, r: Ref) -> &Closure {
        debug_assert_eq!(kind_of(meta_at(r)), K_CLOSURE);
        unsafe { &*(r as *const Closure) }
    }
    #[inline(always)]
    pub fn closure_mut(&mut self, r: Ref) -> &mut Closure {
        unsafe { &mut *(r as *mut Closure) }
    }

    pub fn alloc_closure(&mut self, proto: &Arc<FunctionProto>, upvals: &[Value]) -> Ref {
        self.hold_proto(proto);
        let a = self.alloc_old_cell(K_CLOSURE, 2 + upvals.len(), upvals.len());
        set_word(a, 1, Arc::as_ptr(proto) as u64);
        self.closure_mut(a).upvals_mut().copy_from_slice(upvals);
        a
    }

    /// `hold_proto` with a one-entry cache: closures made in a loop
    /// share one proto.
    #[inline]
    pub fn hold_proto(&mut self, proto: &Arc<FunctionProto>) {
        let p = Arc::as_ptr(proto) as usize;
        if p == self.held_last {
            return;
        }
        self.held_last = p;
        hold_proto(proto);
    }

    #[inline(always)]
    pub fn cell(&self, r: Ref) -> &Value {
        debug_assert_eq!(kind_of(meta_at(r)), K_CELL);
        unsafe { &*((r as *const Value).add(1)) }
    }
    #[inline(always)]
    pub fn cell_mut(&mut self, r: Ref) -> &mut Value {
        debug_assert_eq!(kind_of(meta_at(r)), K_CELL);
        unsafe { &mut *((r as *mut Value).add(1)) }
    }
    pub fn alloc_cell(&mut self, v: Value) -> Ref {
        let a = self.alloc_old_cell(K_CELL, CELL_WORDS, 0);
        set_word(a, 1, v.0);
        a
    }

    // ---- write barriers: record aged containers that gain new edges ----
    // Container-only, dedup via the dirty bit. Young containers are traced
    // wholesale; born-old ones are on the born log already.

    #[inline(always)]
    fn barrier(&mut self, a: Ref, log: Value) {
        let m = meta_at(a);
        if m & (M_AGED | M_DIRTY) == M_AGED {
            set_meta(a, m | M_DIRTY);
            self.remembered.push(log);
        }
    }
    #[inline(always)]
    pub fn barrier_obj(&mut self, r: Ref) {
        if !self.young(r) {
            self.barrier(r, Value::object(r));
        }
    }
    #[inline(always)]
    pub fn barrier_arr(&mut self, r: Ref) {
        if !self.young(r) {
            self.barrier(r, Value::array(r));
        }
    }
    #[inline(always)]
    pub fn barrier_cell(&mut self, r: Ref) {
        self.barrier(r, Value::cell(r));
    }
    #[inline(always)]
    pub fn barrier_foreign(&mut self, r: Ref) {
        if self.gen_foreigns.old.get(r) && !self.gen_foreigns.dirty.get(r) {
            self.gen_foreigns.dirty.set(r);
            self.remembered.push(Value::foreign(r));
        }
    }

    /// Cells in use: old-space bytes plus the nursery, for stats.
    pub fn cell_bytes(&self) -> usize {
        self.old.allocated_bytes() + self.young.used()
    }
}

/// Element chunks up to this many values are nursery-allocated with
/// their owner; larger ones go straight to old space.
pub const ELEMS_YOUNG_MAX: usize = 512;

/// Value tag for a cell kind.
#[inline(always)]
fn cell_tag(kind: u64) -> u64 {
    match kind {
        K_OBJ => TAG_OBJ,
        K_ARR => TAG_ARR,
        K_CLOSURE => TAG_CLOSURE,
        K_CELL => TAG_CELL,
        _ => unreachable!("no Value tag for cell kind {kind}"),
    }
}

/// Keep a proto alive for the rest of the process. Closures store a raw
/// proto pointer; the module tree normally owns the target, but a
/// dynamically imported program drops its tree, and compiled code is
/// shared by every realm. Process-global so every heap is covered.
pub fn hold_proto(proto: &Arc<FunctionProto>) {
    type Held = std::sync::Mutex<(std::collections::HashSet<usize>, Vec<Arc<FunctionProto>>)>;
    static HELD: std::sync::OnceLock<Held> = std::sync::OnceLock::new();
    let p = Arc::as_ptr(proto) as usize;
    let mut h = HELD.get_or_init(Default::default).lock().unwrap();
    if h.0.insert(p) {
        h.1.push(proto.clone());
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
/// Decimal digits of `v` into `buf`, returning the start index. Written
/// backwards from the end, so the text is `buf[start..]`.
fn fmt_i64(buf: &mut [u8; 20], mut v: i64) -> usize {
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
    i
}

fn push_i64(out: &mut String, v: i64) {
    let mut buf = [0u8; 20];
    let i = fmt_i64(&mut buf, v);
    out.push_str(unsafe { std::str::from_utf8_unchecked(&buf[i..]) });
}

pub fn push_number(out: &mut String, n: f64) {
    use std::fmt::Write;
    if n.is_nan() {
        out.push_str("NaN");
    } else if n.is_infinite() {
        out.push_str(if n > 0.0 { "Infinity" } else { "-Infinity" });
    } else if n == n.trunc() && n.abs() < 1e18 {
        // 1e18 keeps this inside i64; `n as i64` saturates past 2^63 and
        // printed a clamped value for integers above it
        push_i64(out, n as i64);
    } else {
        let a = n.abs();
        if a >= 1e21 || (a != 0.0 && a < 1e-6) {
            // JS switches to exponential outside [1e-6, 1e21) and spells
            // the exponent's sign; Rust's Display never switches, so
            // 1e21 printed as twenty-two digits.
            let e = format!("{n:e}");
            match e.split_once('e') {
                Some((mantissa, exp)) => {
                    out.push_str(mantissa);
                    out.push('e');
                    if !exp.starts_with('-') {
                        out.push('+');
                    }
                    out.push_str(exp);
                }
                None => out.push_str(&e),
            }
        } else {
            let _ = write!(out, "{n}");
        }
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

#[cfg(test)]
mod str_slots {
    use super::*;

    /// The inline width is picked to keep the slot at 32 bytes; a wider
    /// array would grow every string slot without covering more strings.
    #[test]
    fn slot_stays_32_bytes() {
        assert_eq!(std::mem::size_of::<HStr>(), 32);
    }

    #[test]
    fn inline_roundtrips_and_hands_off_when_too_long() {
        let mut h = Heap::default();
        h.nursery_on = false;
        for s in ["", "a", "item-1234-x", &"x".repeat(STR_INLINE)] {
            let r = h.alloc_str_copy(s);
            assert!(matches!(h.str_raw(r), HStr::Inline(..)), "{s:?} should inline");
            assert_eq!(h.str_at(r), s);
            assert_eq!(h.str_raw(r).byte_len(), s.len());
        }
        let long = "y".repeat(STR_INLINE + 1);
        let r = h.alloc_str_copy(&long);
        assert!(!matches!(h.str_raw(r), HStr::Inline(..)));
        assert_eq!(h.str_at(r), long);
    }

    /// A concatenation that fits inline must not touch the allocator, and
    /// one that does not must still read back correctly through the rope.
    #[test]
    fn concat_inlines_then_ropes() {
        let mut h = Heap::default();
        h.nursery_on = false;
        let a = h.alloc_str_copy("item-");
        let r = h.alloc_concat_str(a, "42");
        assert!(matches!(h.str_raw(r), HStr::Inline(..)));
        assert_eq!(h.str_at(r), "item-42");

        let big = h.alloc_str_copy(&"z".repeat(200));
        let cat = h.alloc_concat(big, big);
        assert_eq!(h.str_at(cat).len(), 400);
        assert_eq!(h.str_at(cat), "z".repeat(400));
    }

    /// Compaction must preserve the trees still reachable from slots and
    /// drop the rest; indices are rewritten in place.
    #[test]
    fn compaction_keeps_live_ropes() {
        let mut h = Heap::default();
        h.nursery_on = false;
        let base = h.alloc_str_copy(&"ab".repeat(100));
        let live = h.alloc_concat(base, base);
        for _ in 0..8192 {
            let dead = h.alloc_concat(base, base);
            h.strs[dead as usize] = HStr::Buf(String::new()); // simulate a sweep
        }
        let before = h.ropes.len();
        h.compact_ropes();
        assert!(h.ropes.len() < before, "arena should shrink");
        assert_eq!(h.str_at(live), "ab".repeat(200));
    }
}
