//! Unified Tier-2: compiles EVERY non-async function. Type facts from
//! tsc-types pick lanes per site, they never reject:
//!   - proven-Num ops   -> unboxed FP on d-registers, no guards
//!   - unproven arith   -> tag-guarded inline fast path, h_step fallback
//!   - object/array read-> tier1-style inline IC template (helper fallback)
//!   - mutations        -> thin helpers (args by value, no slot traffic)
//!   - everything else  -> h_step with full spill/reload around it
//!
//! Register plan: vreg i < 8 lives in d(8+i) holding raw Value bits (a
//! number's Value bits ARE its f64 bits); vreg >= 8 stays in its stack
//! slot. GP plan matches Tier-1 (x19 realm, x20 base, x22 closure,
//! w23 depth, w24 poll, x25 slots, x26 proto, x27 sentinel, x28 taglim).
//!
//! GC safety: only h_call and h_safepoint can collect (thin helpers and
//! h_step allocate but never trigger GC), and the collector roots from
//! stack slots — so d-regs are spilled to slots before exactly those two
//! calls and reloaded after. h_step reads/writes slots directly, so it
//! gets the same spill/reload treatment for coherence.
//!
//! Deopt sites: entry arg guards only (resume pc = entry start_pc, which
//! is 0 on a normal call and the loop header on OSR). Call results are
//! NOT guarded — analysis types them Top, so downstream code treats the
//! raw bits correctly whatever they are.

use crate::asm::{Asm, Cond, Label, SP};
use crate::tier1::{Helpers, HeapOffsets};
use tsc_ir::{Const, FunctionProto, Instr, Op};
use tsr_memory::{Value, JIT_ERR_SENTINEL};

const R_REALM: u32 = 19;
const R_BASE: u32 = 20;
const R_CLOSURE: u32 = 22;
const R_DEPTH: u32 = 23;
const R_POLL: u32 = 24;
const R_SLOTS: u32 = 25;
const R_PROTO: u32 = 26;
const R_SENTINEL: u32 = 27;
const R_TAGLIM: u32 = 28;
const R_STARTPC: u32 = 15; // stashed x5, still live at the entry guards
const SAFEPOINT_INTERVAL: u16 = 1024;
/// vregs 0..LOW live in d(8+i).
const LOW: u8 = 8;

/// JumpIf condition fact (mirrors tsc_types::CondFact; tsr-jit stays
/// decoupled from the analysis crate).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum JCond {
    Num,
    Bool,
    Other,
}

/// Per-site facts from tsc-types.
pub struct Facts<'a> {
    /// per-pc (a,b,c): operand proven Num at instruction entry
    pub num: &'a [[bool; 3]],
    pub jumpif: &'a [JCond],
    /// per-arg: entry guard proves numeric
    pub arg_guard: &'a [bool],
}

struct C {
    a: Asm,
    pc_labels: Vec<Label>,
    bail: Label,
    ret: Label,
    helpers: Helpers,
    offsets: Option<HeapOffsets>,
    ics_base: u64,
    n_low: u8,
}

impl C {
    fn slot(i: u8) -> u32 {
        i as u32 * 8
    }

    /// Operand vreg into an FP register (its home dreg, or `scratch`).
    fn fetch(&mut self, v: u8, scratch: u32) -> u32 {
        if v < LOW {
            (8 + v) as u32
        } else {
            self.a.ldr_d_imm(scratch, R_SLOTS, Self::slot(v));
            scratch
        }
    }

    /// Result from FP register `src` into vreg v's home.
    fn put(&mut self, v: u8, src: u32) {
        if v < LOW {
            let home = (8 + v) as u32;
            if home != src {
                self.a.fmov_dd(home, src);
            }
        } else {
            self.a.str_d_imm(src, R_SLOTS, Self::slot(v));
        }
    }

    /// vreg bits into a GP register.
    fn fetch_x(&mut self, v: u8, x: u32) {
        if v < LOW {
            self.a.fmov_xd(x, (8 + v) as u32);
        } else {
            self.a.ldr_imm(x, R_SLOTS, Self::slot(v));
        }
    }

    /// GP register bits into vreg v's home.
    fn put_x(&mut self, v: u8, x: u32) {
        if v < LOW {
            self.a.fmov_dx((8 + v) as u32, x);
        } else {
            self.a.str_imm(x, R_SLOTS, Self::slot(v));
        }
    }

    fn spill_low(&mut self) {
        for i in 0..self.n_low {
            self.a.str_d_imm((8 + i) as u32, R_SLOTS, Self::slot(i));
        }
    }

    fn reload_low(&mut self) {
        for i in 0..self.n_low {
            self.a.ldr_d_imm((8 + i) as u32, R_SLOTS, Self::slot(i));
        }
    }

    fn put_bits(&mut self, v: u8, bits: u64) {
        self.a.mov_imm64(8, bits);
        self.put_x(v, 8);
    }

    /// x10 = tag of x{src}; branch to `slow` when not a number.
    fn guard_number(&mut self, src: u32, slow: Label) {
        self.a.lsr_imm(10, src, 48);
        self.a.cmp_reg(10, R_TAGLIM);
        self.a.b_cond(Cond::Hs, slow);
    }

    /// x{dst} = x{base} + w{idx} * size.
    fn index_addr(&mut self, dst: u32, base: u32, idx: u32, size: u32) {
        if size.is_power_of_two() {
            self.a.add_reg_lsl(dst, base, idx, size.trailing_zeros());
        } else {
            self.a.mov_imm64(11, size as u64);
            self.a.mul(11, idx, 11);
            self.a.add_reg(dst, base, 11);
        }
    }

    /// blr a thin helper whose args are staged in x0..; sentinel check +
    /// stack-ptr refresh. Thin helpers take args BY VALUE and never GC,
    /// so d-regs stay live across them.
    fn thin(&mut self, addr: usize) {
        self.a.mov_imm64(8, addr as u64);
        self.a.blr(8);
        self.a.cmp_reg(0, R_SENTINEL);
        self.a.b_cond(Cond::Eq, self.bail);
        self.a.add_reg(R_SLOTS, 1, R_BASE);
    }

    /// Full interpreter semantics for instruction `pc` via h_step, with
    /// slot coherence: spill d-regs before (h_step reads slots), reload
    /// after (h_step writes its result to a slot). ctrl stays in x0.
    fn step_full(&mut self, pc: usize) {
        self.spill_low();
        self.a.mov(0, R_REALM);
        self.a.mov(1, R_PROTO);
        self.a.mov_imm64(2, pc as u64);
        self.a.mov(3, R_BASE);
        self.a.mov(4, R_CLOSURE);
        self.a.mov_imm64(8, self.helpers.step as u64);
        self.a.blr(8);
        self.a.cmp_reg(0, R_SENTINEL);
        self.a.b_cond(Cond::Eq, self.bail);
        self.a.add_reg(R_SLOTS, 1, R_BASE);
        self.reload_low();
    }
}

fn js_cond(op: Op) -> Cond {
    match op {
        Op::Lt | Op::LtSkip => Cond::Mi,
        Op::Le | Op::LeSkip => Cond::Ls,
        Op::Gt | Op::GtSkip => Cond::Gt,
        Op::Ge | Op::GeSkip => Cond::Ge,
        Op::Eq | Op::EqSkip => Cond::Eq,
        Op::Ne | Op::NeSkip => Cond::Ne,
        _ => unreachable!(),
    }
}

/// Compile any non-async proto. Returns None only for the calls-in-loop
/// policy (same discriminator as Tier-1: measured, keeps fnv-shaped
/// native-call loops in the interpreter's faster inline call arm).
#[allow(clippy::too_many_arguments)]
pub fn compile(
    proto: &FunctionProto,
    helpers: Helpers,
    facts: &Facts,
    fmod_addr: usize,
    pow_addr: usize,
    for_osr: bool,
    offsets: Option<HeapOffsets>,
    ics_base: u64,
) -> Option<Vec<u32>> {
    if proto.is_async || proto.code.len() > crate::tier1::MAX_CODE {
        return None;
    }
    let heap_op = |op: Op| {
        matches!(
            op,
            Op::SetField
                | Op::SetIndex
                | Op::ArrayPush
                | Op::NewObject
                | Op::NewArray
                | Op::Closure
                | Op::Concat
        )
    };
    let fn_mutates = proto.code.iter().any(|i| heap_op(i.op));
    for (pc, ins) in proto.code.iter().enumerate() {
        if ins.op == Op::Jump && ins.sbx() < 0 {
            let target = (pc as i64 + ins.sbx() as i64 + 1) as usize;
            let body = &proto.code[target..pc];
            if body.iter().any(|i| i.op == Op::Call) && !(for_osr && fn_mutates) {
                return None;
            }
        }
    }

    let n_low = proto.n_regs.min(LOW);
    let mut c = {
        let mut a = Asm::new();
        let bail = a.new_label();
        let ret = a.new_label();
        let pc_labels: Vec<Label> = (0..proto.code.len() + 2).map(|_| a.new_label()).collect();
        C { a, pc_labels, bail, ret, helpers, offsets, ics_base, n_low }
    };
    let out = c.a.new_label();

    // prologue (tier1 frame + d8-d15, which we own for vregs)
    c.a.stp_pre(29, 30, SP, -16);
    c.a.stp_pre(19, 20, SP, -16);
    c.a.stp_pre(21, 22, SP, -16);
    c.a.stp_pre(23, 24, SP, -16);
    c.a.stp_pre(25, 26, SP, -16);
    c.a.stp_pre(27, 28, SP, -16);
    c.a.raw(0x6DBF_27E8); // stp d8, d9, [sp, #-16]!
    c.a.raw(0x6DBF_2FEA); // stp d10, d11, [sp, #-16]!
    c.a.raw(0x6DBF_37EC); // stp d12, d13, [sp, #-16]!
    c.a.raw(0x6DBF_3FEE); // stp d14, d15, [sp, #-16]!
    c.a.mov(R_REALM, 0);
    c.a.mov(R_PROTO, 1);
    c.a.mov(R_BASE, 2);
    c.a.mov(R_CLOSURE, 3);
    c.a.mov(R_DEPTH, 4);
    c.a.mov_imm64(R_SENTINEL, JIT_ERR_SENTINEL);
    c.a.movz(R_TAGLIM, 0xFFF9, 0);
    c.a.movz(R_POLL, SAFEPOINT_INTERVAL, 0);
    c.a.mov(R_STARTPC, 5); // stash start_pc across the stack_ptr call
    c.a.mov_imm64(8, helpers.stack_ptr as u64);
    c.a.blr(8);
    c.a.add_reg(R_SLOTS, 1, R_BASE);

    // load every low vreg home from its slot: args get their values,
    // locals get the frame's UNDEFINED fill — homes always hold valid
    // Value bits, so spill_low at any later point roots correctly
    c.reload_low();

    // entry guards for proven-Num params. Deopt resumes at start_pc: 0 on
    // a normal call, the loop header on OSR — either way the slots are
    // exactly the interpreter frame.
    let deopt_entry = c.a.new_label();
    for (i, &g) in facts.arg_guard.iter().enumerate() {
        if !g {
            continue;
        }
        c.fetch_x(i as u8, 8);
        c.a.lsr_imm(9, 8, 48);
        c.a.cmp_reg(9, R_TAGLIM);
        c.a.b_cond(Cond::Hs, deopt_entry);
    }

    // OSR dispatch: start_pc != 0 jumps to the matching loop header
    // (homes were just reloaded from the current interpreter slots)
    if for_osr {
        let normal = c.a.new_label();
        c.a.cbz(R_STARTPC, normal);
        let mut headers: Vec<usize> = proto
            .code
            .iter()
            .enumerate()
            .filter(|(_, i)| i.op == Op::Jump && i.sbx() < 0)
            .map(|(pc, i)| (pc as i64 + i.sbx() as i64 + 1) as usize)
            .collect();
        headers.sort_unstable();
        headers.dedup();
        for h in headers {
            c.a.mov_imm64(8, h as u64);
            c.a.cmp_reg(R_STARTPC, 8);
            let l = c.pc_labels[h];
            c.a.b_cond(Cond::Eq, l);
        }
        c.a.bind(normal);
    }

    for (pc, ins) in proto.code.iter().enumerate() {
        let l = c.pc_labels[pc];
        c.a.bind(l);
        emit_op(&mut c, proto, pc, *ins, facts, fmod_addr, pow_addr);
    }
    let e1 = c.pc_labels[proto.code.len()];
    c.a.bind(e1);
    let e2 = c.pc_labels[proto.code.len() + 1];
    c.a.bind(e2);
    c.a.mov_imm64(1, Value::UNDEFINED.bits());
    c.a.b(c.ret);

    c.a.bind(deopt_entry);
    c.a.movz(0, 2, 0);
    c.a.mov(1, R_STARTPC);
    c.a.b(out);

    let ret = c.ret;
    c.a.bind(ret);
    c.a.movz(0, 0, 0);
    c.a.b(out);
    let bail = c.bail;
    c.a.bind(bail);
    c.a.movz(0, 1, 0);
    c.a.bind(out);
    c.a.raw(0x6CC1_3FEE); // ldp d14, d15, [sp], #16
    c.a.raw(0x6CC1_37EC); // ldp d12, d13, [sp], #16
    c.a.raw(0x6CC1_2FEA); // ldp d10, d11, [sp], #16
    c.a.raw(0x6CC1_27E8); // ldp d8, d9, [sp], #16
    c.a.ldp_post(27, 28, SP, 16);
    c.a.ldp_post(25, 26, SP, 16);
    c.a.ldp_post(23, 24, SP, 16);
    c.a.ldp_post(21, 22, SP, 16);
    c.a.ldp_post(19, 20, SP, 16);
    c.a.ldp_post(29, 30, SP, 16);
    c.a.ret();

    Some(c.a.finish())
}

/// Integer-fast-path Mod on numeric inputs in d{db}/d{dc}; falls back to
/// libm fmod. Result lands in vreg a's home.
fn emit_mod_num(c: &mut C, a_reg: u8, db: u32, dc: u32, fmod_addr: usize) {
    let slow = c.a.new_label();
    let done = c.a.new_label();
    c.a.fcvtzs(10, db);
    c.a.scvtf(2, 10);
    c.a.fcmp(2, db);
    c.a.b_cond(Cond::Ne, slow);
    c.a.fcvtzs(11, dc);
    c.a.scvtf(3, 11);
    c.a.fcmp(3, dc);
    c.a.b_cond(Cond::Ne, slow);
    c.a.cbz(11, slow); // x % 0 = NaN: fmod handles
    c.a.sdiv(12, 10, 11);
    c.a.msub(13, 12, 11, 10);
    let nonzero = c.a.new_label();
    c.a.cbnz(13, nonzero);
    // remainder 0: ±0 with the dividend's sign
    c.a.fmov_xd(14, db);
    c.a.mov_imm64(12, 0x8000_0000_0000_0000);
    c.a.and_reg(14, 14, 12);
    c.put_x(a_reg, 14);
    c.a.b(done);
    c.a.bind(nonzero);
    let dst = if a_reg < LOW { (8 + a_reg) as u32 } else { 2 };
    c.a.scvtf(dst, 13);
    if a_reg >= LOW {
        c.a.str_d_imm(dst, R_SLOTS, C::slot(a_reg));
    }
    c.a.b(done);
    c.a.bind(slow);
    if db != 0 {
        c.a.fmov_dd(0, db);
    }
    if dc != 1 {
        c.a.fmov_dd(1, dc);
    }
    c.a.mov_imm64(8, fmod_addr as u64);
    c.a.blr(8);
    c.put(a_reg, 0);
    c.a.bind(done);
}

fn emit_op(
    c: &mut C,
    proto: &FunctionProto,
    pc: usize,
    ins: Instr,
    facts: &Facts,
    fmod_addr: usize,
    pow_addr: usize,
) {
    let num_bc = facts.num[pc][1] && facts.num[pc][2];
    let num_b = facts.num[pc][1];
    match ins.op {
        Op::LoadInt => c.put_bits(ins.a, Value::number(ins.sbx() as f64).bits()),
        Op::LoadBool => c.put_bits(
            ins.a,
            if ins.b != 0 { Value::TRUE.bits() } else { Value::FALSE.bits() },
        ),
        Op::LoadNull => c.put_bits(ins.a, Value::NULL.bits()),
        Op::LoadUndef => c.put_bits(ins.a, Value::UNDEFINED.bits()),
        Op::LoadConst => match proto.consts.get(ins.bx() as usize) {
            Some(Const::Number(n)) => c.put_bits(ins.a, Value::number(*n).bits()),
            // string consts allocate — interpreter semantics
            _ => c.step_full(pc),
        },
        Op::Move => {
            let src = c.fetch(ins.b, 0);
            c.put(ins.a, src);
        }

        Op::Add | Op::Sub | Op::Mul | Op::Div => {
            if num_bc {
                let db = c.fetch(ins.b, 0);
                let dc = c.fetch(ins.c, 1);
                let dst = if ins.a < LOW { (8 + ins.a) as u32 } else { 2 };
                match ins.op {
                    Op::Add => c.a.fadd(dst, db, dc),
                    Op::Sub => c.a.fsub(dst, db, dc),
                    Op::Mul => c.a.fmul(dst, db, dc),
                    Op::Div => c.a.fdiv(dst, db, dc),
                    _ => unreachable!(),
                }
                if ins.a >= LOW {
                    c.a.str_d_imm(dst, R_SLOTS, C::slot(ins.a));
                }
            } else {
                // guarded: numbers inline, anything else (concat, errors)
                // through the interpreter step
                let slow = c.a.new_label();
                let done = c.a.new_label();
                c.fetch_x(ins.b, 8);
                c.fetch_x(ins.c, 9);
                c.guard_number(8, slow);
                c.guard_number(9, slow);
                c.a.fmov_dx(0, 8);
                c.a.fmov_dx(1, 9);
                match ins.op {
                    Op::Add => c.a.fadd(0, 0, 1),
                    Op::Sub => c.a.fsub(0, 0, 1),
                    Op::Mul => c.a.fmul(0, 0, 1),
                    Op::Div => c.a.fdiv(0, 0, 1),
                    _ => unreachable!(),
                }
                c.put(ins.a, 0);
                c.a.b(done);
                c.a.bind(slow);
                c.step_full(pc);
                c.a.bind(done);
            }
        }
        Op::Mod => {
            if num_bc {
                let db = c.fetch(ins.b, 0);
                let dc = c.fetch(ins.c, 1);
                emit_mod_num(c, ins.a, db, dc, fmod_addr);
            } else {
                let slow = c.a.new_label();
                let done = c.a.new_label();
                c.fetch_x(ins.b, 8);
                c.fetch_x(ins.c, 9);
                c.guard_number(8, slow);
                c.guard_number(9, slow);
                c.a.fmov_dx(0, 8);
                c.a.fmov_dx(1, 9);
                emit_mod_num(c, ins.a, 0, 1, fmod_addr);
                c.a.b(done);
                c.a.bind(slow);
                c.step_full(pc);
                c.a.bind(done);
            }
        }
        Op::Pow => {
            let slow = c.a.new_label();
            let done = c.a.new_label();
            if num_bc {
                let db = c.fetch(ins.b, 0);
                let dc = c.fetch(ins.c, 1);
                if db != 0 {
                    c.a.fmov_dd(0, db);
                }
                if dc != 1 {
                    c.a.fmov_dd(1, dc);
                }
            } else {
                c.fetch_x(ins.b, 8);
                c.fetch_x(ins.c, 9);
                c.guard_number(8, slow);
                c.guard_number(9, slow);
                c.a.fmov_dx(0, 8);
                c.a.fmov_dx(1, 9);
            }
            c.a.mov_imm64(8, pow_addr as u64);
            c.a.blr(8);
            c.put(ins.a, 0);
            c.a.b(done);
            c.a.bind(slow);
            if !num_bc {
                c.step_full(pc);
            }
            c.a.bind(done);
        }
        Op::Neg => {
            if num_b {
                let db = c.fetch(ins.b, 0);
                let dst = if ins.a < LOW { (8 + ins.a) as u32 } else { 0 };
                c.a.fneg(dst, db);
                if ins.a >= LOW {
                    c.a.str_d_imm(dst, R_SLOTS, C::slot(ins.a));
                }
            } else {
                let slow = c.a.new_label();
                let done = c.a.new_label();
                c.fetch_x(ins.b, 8);
                c.guard_number(8, slow);
                c.a.fmov_dx(0, 8);
                c.a.fneg(0, 0);
                c.put(ins.a, 0);
                c.a.b(done);
                c.a.bind(slow);
                c.step_full(pc);
                c.a.bind(done);
            }
        }
        Op::BitAnd | Op::BitOr | Op::BitXor | Op::Shl | Op::Shr | Op::UShr | Op::BitNot => {
            let unary = ins.op == Op::BitNot;
            let proven = if unary { num_b } else { num_bc };
            let slow = c.a.new_label();
            let done = c.a.new_label();
            if proven {
                let db = c.fetch(ins.b, 0);
                c.a.fcvtzs(10, db);
                if !unary {
                    let dc = c.fetch(ins.c, 1);
                    c.a.fcvtzs(11, dc);
                }
            } else {
                c.fetch_x(ins.b, 8);
                c.guard_number(8, slow);
                if !unary {
                    c.fetch_x(ins.c, 9);
                    c.guard_number(9, slow);
                    c.a.fmov_dx(1, 9);
                    c.a.fcvtzs(11, 1);
                }
                c.a.fmov_dx(0, 8);
                c.a.fcvtzs(10, 0);
            }
            let mut unsigned = false;
            match ins.op {
                Op::BitAnd => c.a.and_reg32(10, 10, 11),
                Op::BitOr => c.a.orr_reg32(10, 10, 11),
                Op::BitXor => c.a.eor_reg32(10, 10, 11),
                Op::Shl => {
                    c.a.movz(12, 31, 0);
                    c.a.and_reg32(11, 11, 12);
                    c.a.lslv32(10, 10, 11);
                }
                Op::Shr => {
                    c.a.movz(12, 31, 0);
                    c.a.and_reg32(11, 11, 12);
                    c.a.asrv32(10, 10, 11);
                }
                Op::UShr => {
                    c.a.movz(12, 31, 0);
                    c.a.and_reg32(11, 11, 12);
                    c.a.lsrv32(10, 10, 11);
                    unsigned = true;
                }
                Op::BitNot => c.a.mvn32(10, 10),
                _ => unreachable!(),
            }
            let dst = if ins.a < LOW { (8 + ins.a) as u32 } else { 2 };
            if unsigned {
                c.a.ucvtf_w(dst, 10);
            } else {
                c.a.scvtf_w(dst, 10);
            }
            if ins.a >= LOW {
                c.a.str_d_imm(dst, R_SLOTS, C::slot(ins.a));
            }
            c.a.b(done);
            c.a.bind(slow);
            if !proven {
                c.step_full(pc);
            }
            c.a.bind(done);
        }

        Op::Eq | Op::Ne | Op::Lt | Op::Le | Op::Gt | Op::Ge => {
            if num_bc {
                let db = c.fetch(ins.b, 0);
                let dc = c.fetch(ins.c, 1);
                c.a.fcmp(db, dc);
                c.a.cset(8, js_cond(ins.op));
                c.a.mov_imm64(9, Value::FALSE.bits());
                c.a.add_reg(8, 9, 8);
                c.put_x(ins.a, 8);
            } else {
                let slow = c.a.new_label();
                let done = c.a.new_label();
                c.fetch_x(ins.b, 8);
                c.fetch_x(ins.c, 9);
                c.guard_number(8, slow);
                c.guard_number(9, slow);
                c.a.fmov_dx(0, 8);
                c.a.fmov_dx(1, 9);
                c.a.fcmp(0, 1);
                c.a.cset(8, js_cond(ins.op));
                c.a.mov_imm64(9, Value::FALSE.bits());
                c.a.add_reg(8, 9, 8);
                c.put_x(ins.a, 8);
                c.a.b(done);
                c.a.bind(slow);
                c.step_full(pc);
                c.a.bind(done);
            }
        }
        Op::EqSkip | Op::NeSkip | Op::LtSkip | Op::LeSkip | Op::GtSkip | Op::GeSkip => {
            let skip_to = c.pc_labels[pc + 2];
            if num_bc {
                let db = c.fetch(ins.b, 0);
                let dc = c.fetch(ins.c, 1);
                c.a.fcmp(db, dc);
                c.a.b_cond(js_cond(ins.op), skip_to);
            } else {
                let slow = c.a.new_label();
                let done = c.a.new_label();
                c.fetch_x(ins.b, 8);
                c.fetch_x(ins.c, 9);
                c.guard_number(8, slow);
                c.guard_number(9, slow);
                c.a.fmov_dx(0, 8);
                c.a.fmov_dx(1, 9);
                c.a.fcmp(0, 1);
                c.a.b_cond(js_cond(ins.op), skip_to);
                c.a.b(done);
                c.a.bind(slow);
                c.step_full(pc);
                c.a.cbnz(0, skip_to); // ctrl=1: comparison true -> skip
                c.a.bind(done);
            }
        }

        Op::Jump => {
            let target = (pc as i64 + ins.sbx() as i64 + 1) as usize;
            if ins.sbx() < 0 {
                let no_poll = c.a.new_label();
                c.a.sub_imm32(R_POLL, R_POLL, 1);
                c.a.cbnz32(R_POLL, no_poll);
                // GC can run here: root the d-reg homes through the slots
                c.spill_low();
                c.a.mov(0, R_REALM);
                c.a.mov_imm64(8, c.helpers.safepoint as u64);
                c.a.blr(8);
                c.a.cmp_reg(0, R_SENTINEL);
                c.a.b_cond(Cond::Eq, c.bail);
                c.a.add_reg(R_SLOTS, 1, R_BASE);
                c.reload_low();
                c.a.movz(R_POLL, SAFEPOINT_INTERVAL, 0);
                c.a.bind(no_poll);
            }
            let t = c.pc_labels[target];
            c.a.b(t);
        }
        Op::JumpIfFalse | Op::JumpIfTrue => {
            let target = (pc as i64 + ins.sbx() as i64 + 1) as usize;
            let t = c.pc_labels[target];
            match facts.jumpif[pc] {
                JCond::Num => {
                    let da = c.fetch(ins.a, 0);
                    c.a.fcmp_zero(da);
                    match ins.op {
                        Op::JumpIfFalse => {
                            c.a.b_cond(Cond::Eq, t);
                            c.a.b_cond(Cond::Vs, t);
                        }
                        _ => {
                            let fall = c.a.new_label();
                            c.a.b_cond(Cond::Eq, fall);
                            c.a.b_cond(Cond::Vs, fall);
                            c.a.b(t);
                            c.a.bind(fall);
                        }
                    }
                }
                JCond::Bool => {
                    c.fetch_x(ins.a, 8);
                    c.a.mov_imm64(9, Value::TRUE.bits());
                    c.a.cmp_reg(8, 9);
                    match ins.op {
                        Op::JumpIfFalse => c.a.b_cond(Cond::Ne, t),
                        _ => c.a.b_cond(Cond::Eq, t),
                    }
                }
                JCond::Other => {
                    // numbers inline; other truthiness via step
                    let fall = c.a.new_label();
                    let slow = c.a.new_label();
                    c.fetch_x(ins.a, 8);
                    c.guard_number(8, slow);
                    c.a.fmov_dx(0, 8);
                    c.a.fcmp_zero(0);
                    match ins.op {
                        Op::JumpIfFalse => {
                            c.a.b_cond(Cond::Eq, t);
                            c.a.b_cond(Cond::Vs, t);
                            c.a.b(fall);
                        }
                        _ => {
                            c.a.b_cond(Cond::Eq, fall);
                            c.a.b_cond(Cond::Vs, fall);
                            c.a.b(t);
                        }
                    }
                    c.a.bind(slow);
                    c.step_full(pc);
                    c.a.cbnz(0, t); // ctrl=1: branch taken
                    c.a.bind(fall);
                }
            }
        }

        Op::Call => {
            // h_call pushes frames and can GC: full spill/reload. Result
            // is typed Top by analysis — no guard, no deopt.
            c.spill_low();
            c.a.mov(0, R_REALM);
            c.a.mov(1, R_PROTO);
            c.a.mov_imm64(2, pc as u64);
            c.a.mov(3, R_BASE);
            c.a.mov_imm64(4, ins.a as u64);
            c.a.mov_imm64(5, ins.b as u64);
            c.a.mov(6, R_DEPTH);
            c.a.mov_imm64(8, c.helpers.call as u64);
            c.a.blr(8);
            c.a.cmp_reg(0, R_SENTINEL);
            c.a.b_cond(Cond::Eq, c.bail);
            c.a.add_reg(R_SLOTS, 1, R_BASE);
            c.reload_low();
        }
        Op::Return => {
            c.fetch_x(ins.a, 1);
            c.a.b(c.ret);
        }
        Op::Halt => {
            c.a.mov_imm64(1, Value::UNDEFINED.bits());
            c.a.b(c.ret);
        }

        Op::GetField => {
            let slow = c.a.new_label();
            let done = c.a.new_label();
            if let Some(o) = c.offsets {
                // inline IC'd property load (reads only, no GC)
                c.fetch_x(ins.b, 8);
                c.a.lsr_imm(10, 8, 48);
                c.a.movz(11, 0xFFFB, 0); // TAG_OBJ
                c.a.cmp_reg(10, 11);
                c.a.b_cond(Cond::Ne, slow);
                c.a.orr_reg32(9, 31, 8); // w9 = payload ref
                c.a.ldr_imm(10, R_REALM, o.realm_objs_ptr);
                c.index_addr(10, 10, 9, o.obj_size);
                c.a.ldr_imm(11, 10, o.obj_shape_arc);
                c.a.ldr_w_imm(12, 11, o.shape_id_delta);
                c.a.mov_imm64(13, c.ics_base + (pc as u64) * 8);
                c.a.ldr_imm(14, 13, 0);
                c.a.lsr_imm(16, 14, 32);
                c.a.cmp_reg(16, 12);
                c.a.b_cond(Cond::Ne, slow);
                c.a.sub_imm32(16, 14, 1);
                c.a.ldr_imm(17, 10, o.obj_vals_ptr);
                c.a.ldr_reg_lsl3(8, 17, 16);
                c.put_x(ins.a, 8);
                c.a.b(done);
            }
            c.a.bind(slow);
            c.a.mov(0, R_REALM);
            c.a.mov(1, R_PROTO);
            c.a.mov_imm64(2, pc as u64);
            c.fetch_x(ins.b, 3);
            c.a.mov_imm64(4, ins.c as u64);
            c.thin(c.helpers.get_field);
            c.put_x(ins.a, 0);
            c.a.bind(done);
        }
        Op::SetField => {
            c.a.mov(0, R_REALM);
            c.a.mov(1, R_PROTO);
            c.a.mov_imm64(2, pc as u64);
            c.fetch_x(ins.a, 3);
            c.a.mov_imm64(4, ins.b as u64);
            c.fetch_x(ins.c, 5);
            c.thin(c.helpers.set_field);
        }
        Op::GetIndex => {
            let slow = c.a.new_label();
            let done = c.a.new_label();
            if let Some(o) = c.offsets {
                c.fetch_x(ins.b, 8);
                c.a.lsr_imm(10, 8, 48);
                c.a.movz(11, 0xFFFC, 0); // TAG_ARR
                c.a.cmp_reg(10, 11);
                c.a.b_cond(Cond::Ne, slow);
                c.fetch_x(ins.c, 9);
                c.guard_number(9, slow);
                c.a.orr_reg32(12, 31, 8);
                c.a.ldr_imm(10, R_REALM, o.realm_arrs_ptr);
                c.index_addr(10, 10, 12, o.arr_size);
                c.a.fmov_dx(0, 9);
                c.a.fcvtzs(13, 0);
                c.a.scvtf(1, 13);
                c.a.fcmp(1, 0);
                c.a.b_cond(Cond::Ne, slow);
                c.a.ldr_imm(14, 10, o.vec_len);
                c.a.cmp_reg(13, 14);
                c.a.b_cond(Cond::Hs, slow);
                c.a.ldr_imm(16, 10, o.vec_ptr);
                c.a.ldr_reg_lsl3(8, 16, 13);
                c.put_x(ins.a, 8);
                c.a.b(done);
            }
            c.a.bind(slow);
            c.a.mov(0, R_REALM);
            c.a.mov(1, R_PROTO);
            c.a.mov_imm64(2, pc as u64);
            c.fetch_x(ins.b, 3);
            c.fetch_x(ins.c, 4);
            c.thin(c.helpers.get_index);
            c.put_x(ins.a, 0);
            c.a.bind(done);
        }
        Op::SetIndex => {
            c.a.mov(0, R_REALM);
            c.a.mov(1, R_PROTO);
            c.a.mov_imm64(2, pc as u64);
            c.fetch_x(ins.a, 3);
            c.fetch_x(ins.b, 4);
            c.fetch_x(ins.c, 5);
            c.thin(c.helpers.set_index);
        }
        Op::Len => {
            let slow = c.a.new_label();
            let done = c.a.new_label();
            if let Some(o) = c.offsets {
                c.fetch_x(ins.b, 8);
                c.a.lsr_imm(10, 8, 48);
                c.a.movz(11, 0xFFFC, 0);
                c.a.cmp_reg(10, 11);
                c.a.b_cond(Cond::Ne, slow);
                c.a.orr_reg32(12, 31, 8);
                c.a.ldr_imm(10, R_REALM, o.realm_arrs_ptr);
                c.index_addr(10, 10, 12, o.arr_size);
                c.a.ldr_imm(14, 10, o.vec_len);
                c.a.scvtf(0, 14);
                c.put(ins.a, 0);
                c.a.b(done);
            }
            c.a.bind(slow);
            c.a.mov(0, R_REALM);
            c.a.mov(1, R_PROTO);
            c.a.mov_imm64(2, pc as u64);
            c.fetch_x(ins.b, 3);
            c.thin(c.helpers.len);
            c.put_x(ins.a, 0);
            c.a.bind(done);
        }
        Op::ArrayPush => {
            c.a.mov(0, R_REALM);
            c.a.mov(1, R_PROTO);
            c.a.mov_imm64(2, pc as u64);
            c.fetch_x(ins.a, 3);
            c.fetch_x(ins.b, 4);
            c.thin(c.helpers.push);
        }
        Op::GetGlobal => {
            c.a.mov(0, R_REALM);
            c.a.mov(1, R_PROTO);
            c.a.mov_imm64(2, pc as u64);
            c.a.mov_imm64(3, ins.bx() as u64);
            c.thin(c.helpers.get_global);
            c.put_x(ins.a, 0);
        }

        // closure-cell ops: thin helpers, args by value — no spill needed
        Op::GetUpval => {
            c.a.mov(0, R_REALM);
            c.a.mov(1, R_CLOSURE);
            c.a.mov_imm64(2, ins.b as u64);
            c.thin(c.helpers.get_upval);
            c.put_x(ins.a, 0);
        }
        Op::SetUpval => {
            c.a.mov(0, R_REALM);
            c.a.mov(1, R_CLOSURE);
            c.a.mov_imm64(2, ins.a as u64);
            c.fetch_x(ins.b, 3);
            c.thin(c.helpers.set_upval);
        }
        Op::LoadCell => {
            c.a.mov(0, R_REALM);
            c.a.mov(1, R_PROTO);
            c.a.mov_imm64(2, pc as u64);
            c.fetch_x(ins.b, 3);
            c.thin(c.helpers.load_cell);
            c.put_x(ins.a, 0);
        }
        Op::StoreCell => {
            c.a.mov(0, R_REALM);
            c.a.mov(1, R_PROTO);
            c.a.mov_imm64(2, pc as u64);
            c.fetch_x(ins.a, 3);
            c.fetch_x(ins.b, 4);
            c.thin(c.helpers.store_cell);
        }
        Op::NewObject => {
            c.a.mov(0, R_REALM);
            c.thin(c.helpers.new_object);
            c.put_x(ins.a, 0);
        }
        Op::NewArray => {
            c.a.mov(0, R_REALM);
            c.a.mov_imm64(1, ins.b as u64);
            c.thin(c.helpers.new_array);
            c.put_x(ins.a, 0);
        }
        Op::NewCell => {
            c.a.mov(0, R_REALM);
            c.fetch_x(ins.a, 1);
            c.thin(c.helpers.new_cell);
            c.put_x(ins.a, 0);
        }

        Op::Await => unreachable!("async proto passed analysis"),
        // closures, cells, upvals, Concat, TypeOf, Not, NewObject,
        // NewArray, string consts: exact interpreter semantics.
        // ponytail: NewObject/NewArray via step costs spill+reload per
        // alloc; inline bump-nursery alloc is the upgrade path.
        _ => c.step_full(pc),
    }
}
