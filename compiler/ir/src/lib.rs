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
}

#[derive(Clone, Debug, PartialEq)]
pub enum Const {
    Number(f64),
    Str(Arc<str>),
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

#[derive(Debug)]
pub struct FunctionProto {
    pub name: Arc<str>,
    pub arity: u8,
    /// Async functions run as coroutines; calls return a promise.
    pub is_async: bool,
    pub n_regs: u8,
    pub code: Vec<Instr>,
    pub consts: Vec<Const>,
    pub upvals: Vec<UpvalSrc>,
    /// Child function protos referenced by `Closure` Bx.
    pub protos: Vec<Arc<FunctionProto>>,
    /// Source byte offset per instruction, for error spans.
    pub spans: Vec<u32>,
    /// TS parameter annotations (Top when unannotated).
    pub arg_types: Vec<TypeHint>,
    pub jit: JitState,
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
    let _ = writeln!(
        out,
        "function {} (arity {}, regs {}, upvals {})",
        proto.name,
        proto.arity,
        proto.n_regs,
        proto.upvals.len()
    );
    for (i, c) in proto.consts.iter().enumerate() {
        let _ = writeln!(out, "  const {i}: {c:?}");
    }
    for (i, ins) in proto.code.iter().enumerate() {
        let _ = writeln!(out, "  {i:4}: {ins:?}");
    }
    for p in &proto.protos {
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
