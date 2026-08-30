//! tsc-ir
//!
//! Bytecode contract between compiler and runtime: opcodes, `FunctionProto`,
//! `Chunk`, disassembler. The only crate both sides depend on.

use std::fmt;
use std::sync::atomic::{AtomicPtr, AtomicU32, AtomicU8};
use std::sync::Arc;

/// Fixed-width instruction. ABC form uses a/b/c; ABx form packs b/c as u16
/// via [`Instr::bx`]; sBx is bx biased by 32768 (jumps).
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Instr {
    pub op: Op,
    pub a: u8,
    pub b: u8,
    pub c: u8,
}

pub const SBX_BIAS: i32 = 32768;

impl Instr {
    pub fn abc(op: Op, a: u8, b: u8, c: u8) -> Self {
        Instr { op, a, b, c }
    }
    pub fn abx(op: Op, a: u8, bx: u16) -> Self {
        Instr { op, a, b: (bx >> 8) as u8, c: bx as u8 }
    }
    pub fn asbx(op: Op, a: u8, sbx: i32) -> Self {
        Self::abx(op, a, (sbx + SBX_BIAS) as u16)
    }
    pub fn bx(self) -> u16 {
        ((self.b as u16) << 8) | self.c as u16
    }
    pub fn sbx(self) -> i32 {
        self.bx() as i32 - SBX_BIAS
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Op {
    // load/move
    LoadConst, // A = const[Bx]
    LoadInt,   // A = sBx as f64
    LoadBool,  // A = B != 0
    LoadNull,
    LoadUndef,
    Move, // A = B
    // arithmetic (A = B op C)
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
    Neg, // A = -B
    // bitwise (A = B op C, ToInt32 semantics)
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
    UShr,
    BitNot, // A = ~B
    // compare (A = B op C)
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Not, // A = !B
    // fused compare+branch: if (B op C) skip the next instruction (which
    // the emitter guarantees is a Jump). One dispatch instead of two on
    // the hot (true) path of loop conditions.
    EqSkip,
    NeSkip,
    LtSkip,
    LeSkip,
    GtSkip,
    GeSkip,
    // control
    Jump,        // pc += sBx
    JumpIfFalse, // if !truthy(A) pc += sBx
    JumpIfTrue,
    // calls
    Call,       // A = call A(args A+1 .. A+B)
    Return,     // return A
    Halt,
    // closures / cells
    Closure,  // A = closure(proto[Bx]), captures per proto.upvals
    NewCell,  // A = new cell(undefined)
    LoadCell, // A = *cell(B)
    StoreCell, // *cell(A) = B
    GetUpval, // A = *upval[B]
    SetUpval, // *upval[A] = B
    // heap
    NewObject, // A = {}
    NewArray,  // A = [] with capacity hint B
    /// A = object with const[C] (Const::Keys) fields, values from
    /// registers B..B+n (contiguous). Fused literal: one allocation, one
    /// shape lookup (per-pc cache), no per-field transitions.
    NewObjectLit,
    /// A = array of registers B..B+C (contiguous).
    NewArrayLit,
    GetField,  // A = B[const[C] as name]
    SetField,  // A[const[B] as name] = C
    GetIndex,  // A = B[C]
    SetIndex,  // A[B] = C
    Len,       // A = B.length
    ArrayPush, // A.push(B)
    // globals
    GetGlobal, // A = globals[const[Bx] as name]
    // misc
    Concat, // A = str(B) + str(C)
    TypeOf, // A = typeof B
    /// A = await A. Non-promise/settled: falls through inline. Pending:
    /// suspends the (async) frame. Only emitted inside is_async protos.
    Await,
}

/// Static type facts (flat lattice: anything joins to Top).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TypeHint {
    Num,
    Bool,
    Str,
    Other,
    Top,
}

pub const TIER_COLD: u8 = 0;
pub const TIER_COMPILING: u8 = 2;
pub const TIER_BASELINE: u8 = 3;
pub const TIER_OPT: u8 = 4;
pub const TIER_REJECTED: u8 = 5;

/// Per-function JIT state. Lives on the Arc-shared proto: counters and the
/// published code pointer are cross-thread.
#[derive(Debug, Default)]
pub struct JitState {
    pub counter: AtomicU32,
    pub tier: AtomicU8,
    pub code: AtomicPtr<u8>,
    /// Tier-1 code entered via OSR (loop-header dispatch); may coexist
    /// with a Tier-2 `code` pointer used for normal calls.
    pub osr_code: AtomicPtr<u8>,
    /// Interpreter back-edges observed (OSR trigger).
    pub backedges: AtomicU32,
    /// Tier-2 deoptimization count (demote to Tier-1 at 10).
    pub deopts: AtomicU32,
    /// Arg-tag bitmasks observed during profiling: 1=number seen,
    /// 2=non-number seen. Fixed 8 slots (args beyond 8 unprofiled).
    pub arg_seen: [AtomicU8; 8],
    /// Per-pc inline caches for GetField/SetField:
    /// (shape_id << 32) | (slot + 1); 0 = empty. Lazily allocated.
    pub ics: std::sync::OnceLock<Box<[std::sync::atomic::AtomicU64]>>,
    /// Per-pc shape-transition caches for add-field SetField sites:
    /// leaked pointer to a realm-defined entry (type-erased — this crate
    /// can't see ShapeData); null = empty. First writer wins, entries are
    /// immutable and process-lived.
    pub tics: std::sync::OnceLock<Box<[std::sync::atomic::AtomicPtr<u8>]>>,
}

impl JitState {
    #[inline(always)]
    pub fn ic_load(&self, code_len: usize, pc: usize) -> u64 {
        let ics = self.ics.get_or_init(|| {
            (0..code_len)
                .map(|_| std::sync::atomic::AtomicU64::new(0))
                .collect()
        });
        ics[pc].load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Force-init and return the IC table base (for baking into JIT code).
    pub fn ics_base(&self, code_len: usize) -> *const std::sync::atomic::AtomicU64 {
        let ics = self.ics.get_or_init(|| {
            (0..code_len)
                .map(|_| std::sync::atomic::AtomicU64::new(0))
                .collect()
        });
        ics.as_ptr()
    }

    #[inline(always)]
    pub fn ic_store(&self, code_len: usize, pc: usize, shape_id: u32, slot: usize) {
        let ics = self.ics.get_or_init(|| {
            (0..code_len)
                .map(|_| std::sync::atomic::AtomicU64::new(0))
                .collect()
        });
        ics[pc].store(
            ((shape_id as u64) << 32) | (slot as u64 + 1),
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    /// Force-init and return the TIC table base (for baking into JIT code).
    pub fn tics_base(&self, code_len: usize) -> *const std::sync::atomic::AtomicPtr<u8> {
        let tics = self.tics.get_or_init(|| {
            (0..code_len)
                .map(|_| std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()))
                .collect()
        });
        tics.as_ptr()
    }

    #[inline(always)]
    pub fn tic_load(&self, code_len: usize, pc: usize) -> *mut u8 {
        let tics = self.tics.get_or_init(|| {
            (0..code_len)
                .map(|_| std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()))
                .collect()
        });
        tics[pc].load(std::sync::atomic::Ordering::Acquire)
    }

    /// Publish a transition entry; only the first store wins (returns the
    /// losing pointer to the caller for cleanup on a race).
    pub fn tic_store(&self, code_len: usize, pc: usize, entry: *mut u8) -> bool {
        let tics = self.tics.get_or_init(|| {
            (0..code_len)
                .map(|_| std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()))
                .collect()
        });
        tics[pc]
            .compare_exchange(
                std::ptr::null_mut(),
                entry,
                std::sync::atomic::Ordering::Release,
                std::sync::atomic::Ordering::Relaxed,
            )
            .is_ok()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Const {
    Number(f64),
    Str(Arc<str>),
    /// Field-name list for a NewObjectLit site.
    Keys(Arc<[Arc<str>]>),
}

/// Where a closure's upvalue comes from at `Closure` execution time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpvalSrc {
    /// A cell living in the enclosing frame's register `reg`.
    ParentLocal(u8),
    /// Immutable capture: copy the VALUE from the enclosing frame's
    /// register (the binding is never reassigned — no cell needed).
    ParentLocalValue(u8),
    /// The enclosing closure's upvalue `idx` (copied verbatim).
    ParentUpval(u8),
}

/// The compiled body of a function — everything execution needs beyond
/// the header. Split out (M6a) so it can be filled lazily on first call
/// (M6c); until then it is always populated at construction.
#[derive(Debug, Default)]
pub struct ProtoBody {
    pub n_regs: u8,
    pub code: Vec<Instr>,
    pub consts: Vec<Const>,
    /// Child function protos referenced by `Closure` Bx.
    pub protos: Vec<Arc<FunctionProto>>,
    /// Source byte offset per instruction, for error spans.
    pub spans: Vec<u32>,
}

/// Observability: lazily-created vs actually-filled proto counts
/// (`TSC_COMPILE_PHASES=1` prints them at exit).
pub static LAZY_TOTAL: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
pub static LAZY_FILLED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Everything a deferred body compile needs (M6c). The filler lives in
/// tsc-parser (this crate cannot depend on it), injected as a fn pointer;
/// `payload` is the parser's own state, downcast on fill.
pub struct LazySource {
    pub payload: Arc<dyn std::any::Any + Send + Sync>,
    /// Compile the body now. Errors are (message, source span start).
    pub fill: fn(&LazySource) -> Result<ProtoBody, (String, u32)>,
}

impl std::fmt::Debug for LazySource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LazySource")
    }
}

#[derive(Debug)]
pub struct FunctionProto {
    pub name: Arc<str>,
    pub arity: u8,
    /// Async functions run as coroutines; calls return a promise.
    pub is_async: bool,
    pub upvals: Vec<UpvalSrc>,
    /// TS parameter annotations (Top when unannotated).
    pub arg_types: Vec<TypeHint>,
    pub jit: JitState,
    body: std::sync::OnceLock<ProtoBody>,
    lazy: Option<LazySource>,
    /// Set when a lazy fill failed; raised at the next call boundary.
    fill_err: std::sync::OnceLock<(String, u32)>,
}

impl FunctionProto {
    pub fn new(
        name: Arc<str>,
        arity: u8,
        is_async: bool,
        upvals: Vec<UpvalSrc>,
        arg_types: Vec<TypeHint>,
        body: ProtoBody,
    ) -> Self {
        let cell = std::sync::OnceLock::new();
        let _ = cell.set(body);
        FunctionProto {
            name,
            arity,
            is_async,
            upvals,
            arg_types,
            jit: JitState::default(),
            body: cell,
            lazy: None,
            fill_err: std::sync::OnceLock::new(),
        }
    }

    /// A proto whose body compiles on first use (M6c).
    pub fn new_lazy(
        name: Arc<str>,
        arity: u8,
        is_async: bool,
        upvals: Vec<UpvalSrc>,
        arg_types: Vec<TypeHint>,
        lazy: LazySource,
    ) -> Self {
        LAZY_TOTAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        FunctionProto {
            name,
            arity,
            is_async,
            upvals,
            arg_types,
            jit: JitState::default(),
            body: std::sync::OnceLock::new(),
            lazy: Some(lazy),
            fill_err: std::sync::OnceLock::new(),
        }
    }

    /// The function's compiled body. One atomic load when filled; a lazy
    /// proto compiles here on first use. Resolve once per frame and reuse
    /// the reference on hot paths. A failed fill yields an empty Halt body
    /// — call boundaries check [`fill_error`] and raise properly.
    #[inline(always)]
    pub fn body(&self) -> &ProtoBody {
        if let Some(b) = self.body.get() {
            return b;
        }
        self.fill_slow()
    }

    #[cold]
    fn fill_slow(&self) -> &ProtoBody {
        self.body.get_or_init(|| {
            LAZY_FILLED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let lz = self.lazy.as_ref().expect("proto has neither body nor lazy source");
            match (lz.fill)(lz) {
                Ok(b) => b,
                Err(e) => {
                    let _ = self.fill_err.set(e);
                    ProtoBody {
                        n_regs: 1,
                        code: vec![Instr::abc(Op::Halt, 0, 0, 0)],
                        consts: Vec::new(),
                        protos: Vec::new(),
                        spans: vec![0],
                    }
                }
            }
        })
    }

    /// Deferred compile error from a failed lazy fill, if any.
    #[inline(always)]
    pub fn fill_error(&self) -> Option<&(String, u32)> {
        self.fill_err.get()
    }
}

impl Default for FunctionProto {
    fn default() -> Self {
        FunctionProto::new(
            Arc::from(""),
            0,
            false,
            Vec::new(),
            Vec::new(),
            ProtoBody::default(),
        )
    }
}

/// A compiled program: the top-level function.
pub struct Chunk {
    pub main: Arc<FunctionProto>,
    pub source_name: String,
}

impl fmt::Debug for Instr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.op {
            Op::LoadConst | Op::Closure | Op::GetGlobal => {
                write!(f, "{:?} r{} {}", self.op, self.a, self.bx())
            }
            Op::LoadInt | Op::Jump | Op::JumpIfFalse | Op::JumpIfTrue => {
                write!(f, "{:?} r{} {:+}", self.op, self.a, self.sbx())
            }
            _ => write!(f, "{:?} r{} r{} r{}", self.op, self.a, self.b, self.c),
        }
    }
}

pub fn disassemble(proto: &FunctionProto, out: &mut String) {
    use fmt::Write;
    let b = proto.body();
    let _ = writeln!(
        out,
        "function {} (arity {}, regs {}, upvals {})",
        proto.name,
        proto.arity,
        b.n_regs,
        proto.upvals.len()
    );
    for (i, c) in b.consts.iter().enumerate() {
        let _ = writeln!(out, "  const {i}: {c:?}");
    }
    for (i, ins) in b.code.iter().enumerate() {
        let _ = writeln!(out, "  {i:4}: {ins:?}");
    }
    for p in &b.protos {
        disassemble(p, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instr_roundtrip() {
        let i = Instr::abx(Op::LoadConst, 3, 65535);
        assert_eq!(i.bx(), 65535);
        let j = Instr::asbx(Op::Jump, 0, -5);
        assert_eq!(j.sbx(), -5);
        let k = Instr::asbx(Op::Jump, 0, 300);
        assert_eq!(k.sbx(), 300);
        assert_eq!(std::mem::size_of::<Instr>(), 4);
    }
}
