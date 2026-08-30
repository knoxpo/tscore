//! Tier-2 typed compiler for number-pure functions (qualified by
//! tsc-types). The NaN-box makes this cheap: a number's Value bits ARE its
//! f64 bits, so d-registers hold raw Value bits and arithmetic runs
//! directly on them — no boxing, no per-op guards (type facts prove
//! operands numeric), no slot traffic in loop bodies.
//!
//! Register plan: vreg i < 8 lives in d(8+i) (callee-saved, safe across
//! helper/libm calls); vreg >= 8 stays in its stack slot. GP plan matches
//! Tier-1 (x19 realm, x20 base, x22 closure, w23 depth, w24 poll,
//! x25 slots, x26 proto, x27 sentinel, x28 tag limit).
//!
//! Deopt sites: entry arg guards (resume pc 0, nothing computed) and
//! Call-result guards (resume pc+1, all state materialized in slots) —
//! `JitRet{val:2, stack:resume_pc}`; the caller resumes in the
//! interpreter. d-regs never hold heap refs (facts allow only
//! Num/Bool/Undefined), so GC at safepoints needs no spills.

use crate::asm::{Asm, Cond, Label, SP};
use crate::tier1::Helpers;
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
const SAFEPOINT_INTERVAL: u16 = 1024;
/// vregs 0..LOW live in d(8+i).
const LOW: u8 = 8;

struct C {
    a: Asm,
    pc_labels: Vec<Label>,
    bail: Label,
    ret: Label,
    helpers: Helpers,
    n_low: u8, // min(n_regs, LOW)
}

impl C {
    fn slot(i: u8) -> u32 {
        i as u32 * 8
    }

    /// Get operand vreg into an FP register (its home dreg, or `scratch`).
    fn fetch(&mut self, v: u8, scratch: u32) -> u32 {
        if v < LOW {
            (8 + v) as u32
        } else {
            self.a.ldr_d_imm(scratch, R_SLOTS, Self::slot(v));
            scratch
        }
    }

    /// Store result from FP register `src` into vreg v's home.
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

    /// mov_imm64 + fmov/str into vreg.
    fn put_bits(&mut self, v: u8, bits: u64) {
        self.a.mov_imm64(8, bits);
        if v < LOW {
            self.a.fmov_dx((8 + v) as u32, 8);
        } else {
            self.a.str_imm(8, R_SLOTS, Self::slot(v));
        }
    }

    fn deopt(&mut self, resume_pc: usize, out: Label) {
        self.a.movz(0, 2, 0);
        self.a.mov_imm64(1, resume_pc as u64);
        self.a.b(out);
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

/// Compile a tier2-qualified proto. `jumpif_num[pc]` from tsc-types.
pub fn compile(
    proto: &FunctionProto,
    helpers: Helpers,
    jumpif_num: &[bool],
    fmod_addr: usize,
    pow_addr: usize,
) -> Vec<u32> {
    let n_low = proto.n_regs.min(LOW);
    let mut c = {
        let mut a = Asm::new();
        let bail = a.new_label();
        let ret = a.new_label();
        let pc_labels: Vec<Label> = (0..proto.code.len() + 2).map(|_| a.new_label()).collect();
        C { a, pc_labels, bail, ret, helpers, n_low }
    };
    let out = c.a.new_label(); // shared epilogue entry (x0/x1 already set)

    // prologue (same frame as tier1)
    c.a.stp_pre(29, 30, SP, -16);
    c.a.stp_pre(19, 20, SP, -16);
    c.a.stp_pre(21, 22, SP, -16);
    c.a.stp_pre(23, 24, SP, -16);
    c.a.stp_pre(25, 26, SP, -16);
    c.a.stp_pre(27, 28, SP, -16);
    // save d8-d15 (we own them for vregs)
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
    c.a.mov_imm64(8, helpers.stack_ptr as u64);
    c.a.blr(8);
    c.a.add_reg(R_SLOTS, 1, R_BASE);

    // entry guards: every arg must be a number (deopt to pc 0 — nothing
    // has been computed, the interpreter re-runs from scratch)
    let deopt0 = c.a.new_label();
    for i in 0..proto.arity {
        c.a.ldr_imm(8, R_SLOTS, C::slot(i));
        c.a.lsr_imm(9, 8, 48);
        c.a.cmp_reg(9, R_TAGLIM);
        c.a.b_cond(Cond::Hs, deopt0);
        if i < LOW {
            c.a.fmov_dx((8 + i) as u32, 8);
        }
    }

    for (pc, ins) in proto.code.iter().enumerate() {
        let l = c.pc_labels[pc];
        c.a.bind(l);
        emit_op(&mut c, proto, pc, *ins, jumpif_num, fmod_addr, pow_addr, out);
    }
    let e1 = c.pc_labels[proto.code.len()];
    c.a.bind(e1);
    let e2 = c.pc_labels[proto.code.len() + 1];
    c.a.bind(e2);
    c.a.mov_imm64(1, Value::UNDEFINED.bits());
    c.a.b(c.ret);

    c.a.bind(deopt0);
    c.a.movz(0, 2, 0);
    c.a.movz(1, 0, 0);
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

    c.a.finish()
}

#[allow(clippy::too_many_arguments)]
fn emit_op(
    c: &mut C,
    proto: &FunctionProto,
    pc: usize,
    ins: Instr,
    jumpif_num: &[bool],
    fmod_addr: usize,
    pow_addr: usize,
    out: Label,
) {
    match ins.op {
        Op::LoadInt => c.put_bits(ins.a, Value::number(ins.sbx() as f64).bits()),
        Op::LoadBool => c.put_bits(
            ins.a,
            if ins.b != 0 { Value::TRUE.bits() } else { Value::FALSE.bits() },
        ),
        Op::LoadUndef => c.put_bits(ins.a, Value::UNDEFINED.bits()),
        Op::LoadConst => {
            // analysis guarantees this is a Number constant
            let bits = match proto.consts[ins.bx() as usize] {
                Const::Number(n) => Value::number(n).bits(),
                _ => unreachable!("non-number const passed analysis"),
            };
            c.put_bits(ins.a, bits);
        }
        Op::Move => {
            let src = c.fetch(ins.b, 0);
            c.put(ins.a, src);
        }
        Op::Add | Op::Sub | Op::Mul | Op::Div => {
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
        }
        Op::Mod => {
            // integer fast path: both operands round-trip through i64 and
            // divisor != 0 -> sdiv/msub (exact; sign of dividend, matching
            // fmod). Zero remainder inherits the dividend's sign (-0 rule).
            let db = c.fetch(ins.b, 0);
            let dc = c.fetch(ins.c, 1);
            let slow = c.a.new_label();
            let done = c.a.new_label();
            c.a.fcvtzs(10, db);
            c.a.scvtf(2, 10);
            c.a.fcmp(2, db);
            c.a.b_cond(Cond::Ne, slow); // not an integer (or NaN)
            c.a.fcvtzs(11, dc);
            c.a.scvtf(3, 11);
            c.a.fcmp(3, dc);
            c.a.b_cond(Cond::Ne, slow);
            c.a.cbz(11, slow); // x % 0 = NaN: let fmod handle it
            c.a.sdiv(12, 10, 11);
            c.a.msub(13, 12, 11, 10); // rem = b - (b/c)*c
            let nonzero = c.a.new_label();
            c.a.cbnz(13, nonzero);
            // remainder 0: result is ±0 with the dividend's sign
            c.a.fmov_xd(14, db);
            c.a.mov_imm64(15, 0x8000_0000_0000_0000);
            c.a.and_reg(14, 14, 15);
            let dst = if ins.a < LOW { (8 + ins.a) as u32 } else { 2 };
            c.a.fmov_dx(dst, 14);
            if ins.a >= LOW {
                c.a.str_d_imm(dst, R_SLOTS, C::slot(ins.a));
            }
            c.a.b(done);
            c.a.bind(nonzero);
            let dst = if ins.a < LOW { (8 + ins.a) as u32 } else { 2 };
            c.a.scvtf(dst, 13);
            if ins.a >= LOW {
                c.a.str_d_imm(dst, R_SLOTS, C::slot(ins.a));
            }
            c.a.b(done);
            c.a.bind(slow);
            // libm fmod: d0/d1 -> d0; d8-d15 and x19+ callee-saved
            if db != 0 {
                c.a.fmov_dd(0, db);
            }
            if dc != 1 {
                c.a.fmov_dd(1, dc);
            }
            c.a.mov_imm64(8, fmod_addr as u64);
            c.a.blr(8);
            c.put(ins.a, 0);
            c.a.bind(done);
        }
        Op::Pow => {
            let db = c.fetch(ins.b, 0);
            let dc = c.fetch(ins.c, 1);
            if db != 0 {
                c.a.fmov_dd(0, db);
            }
            if dc != 1 {
                c.a.fmov_dd(1, dc);
            }
            c.a.mov_imm64(8, pow_addr as u64);
            c.a.blr(8);
            c.put(ins.a, 0);
        }
        Op::Neg => {
            let db = c.fetch(ins.b, 0);
            let dst = if ins.a < LOW { (8 + ins.a) as u32 } else { 0 };
            c.a.fneg(dst, db);
            if ins.a >= LOW {
                c.a.str_d_imm(dst, R_SLOTS, C::slot(ins.a));
            }
        }
        Op::BitAnd | Op::BitOr | Op::BitXor | Op::Shl | Op::Shr | Op::UShr | Op::BitNot => {
            let db = c.fetch(ins.b, 0);
            c.a.fcvtzs(10, db);
            if ins.op != Op::BitNot {
                let dc = c.fetch(ins.c, 1);
                c.a.fcvtzs(11, dc);
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
        }
        Op::Eq | Op::Ne | Op::Lt | Op::Le | Op::Gt | Op::Ge => {
            let db = c.fetch(ins.b, 0);
            let dc = c.fetch(ins.c, 1);
            c.a.fcmp(db, dc);
            c.a.cset(8, js_cond(ins.op));
            c.a.mov_imm64(9, Value::FALSE.bits());
            c.a.add_reg(8, 9, 8);
            if ins.a < LOW {
                c.a.fmov_dx((8 + ins.a) as u32, 8);
            } else {
                c.a.str_imm(8, R_SLOTS, C::slot(ins.a));
            }
        }
        Op::EqSkip | Op::NeSkip | Op::LtSkip | Op::LeSkip | Op::GtSkip | Op::GeSkip => {
            let db = c.fetch(ins.b, 0);
            let dc = c.fetch(ins.c, 1);
            c.a.fcmp(db, dc);
            let skip_to = c.pc_labels[pc + 2];
            c.a.b_cond(js_cond(ins.op), skip_to);
        }
        Op::Jump => {
            let target = (pc as i64 + ins.sbx() as i64 + 1) as usize;
            if ins.sbx() < 0 {
                let no_poll = c.a.new_label();
                c.a.sub_imm32(R_POLL, R_POLL, 1);
                c.a.cbnz32(R_POLL, no_poll);
                c.a.mov(0, R_REALM);
                c.a.mov_imm64(8, c.helpers.safepoint as u64);
                c.a.blr(8);
                c.a.cmp_reg(0, R_SENTINEL);
                c.a.b_cond(Cond::Eq, c.bail);
                c.a.add_reg(R_SLOTS, 1, R_BASE);
                c.a.movz(R_POLL, SAFEPOINT_INTERVAL, 0);
                c.a.bind(no_poll);
            }
            let t = c.pc_labels[target];
            c.a.b(t);
        }
        Op::JumpIfFalse | Op::JumpIfTrue => {
            let target = (pc as i64 + ins.sbx() as i64 + 1) as usize;
            let t = c.pc_labels[target];
            if jumpif_num[pc] {
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
            } else {
                // Bool fact: compare bits against TRUE
                c.fetch_x(ins.a, 8);
                c.a.mov_imm64(9, Value::TRUE.bits());
                c.a.cmp_reg(8, 9);
                match ins.op {
                    Op::JumpIfFalse => c.a.b_cond(Cond::Ne, t),
                    _ => c.a.b_cond(Cond::Eq, t),
                }
            }
        }
        Op::Call => {
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
            // guard: result must be a number (else deopt at pc+1 — the
            // slots are fully materialized right now)
            c.fetch_x(ins.a, 8);
            c.a.lsr_imm(9, 8, 48);
            c.a.cmp_reg(9, R_TAGLIM);
            let okl = c.a.new_label();
            c.a.b_cond(Cond::Lo, okl);
            c.deopt(pc + 1, out);
            c.a.bind(okl);
        }
        Op::Return => {
            c.fetch_x(ins.a, 1);
            c.a.b(c.ret);
        }
        _ => unreachable!("op excluded by analysis: {:?}", ins.op),
    }
}
