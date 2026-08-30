//! Tier-1 baseline template compiler: one short native sequence per
//! bytecode op, registers stay in their `realm.stack` slots (GC/deopt/
//! error machinery unchanged). Fast paths inline for number arithmetic,
//! comparisons and branches; every other op — and every slow case — calls
//! the single-step helper, which shares semantics with the interpreter.
//!
//! Register plan (callee-saved):
//!   x19 realm    x20 base byte offset    x22 closure(ref|MAX)  w23 depth
//!   w24 safepoint countdown   x25 &stack[base]   x26 proto   x27 sentinel

use crate::asm::{Asm, Cond, Label, SP};
use tsc_ir::{FunctionProto, Instr, Op};
use tsr_memory::{Value, JIT_ERR_SENTINEL};

/// Helper entry points, provided by tsr-realm (which owns Realm/run_frame).
/// All are `extern "C"` with the JitRet {val, stack_ptr} return convention.
#[derive(Clone, Copy)]
pub struct Helpers {
    /// fn(realm) -> JitRet{0, stack_ptr}
    pub stack_ptr: usize,
    /// fn(realm, proto, pc, base_bytes, closure) -> JitRet{ctrl|sentinel, stack_ptr}
    /// Executes one instruction with full interpreter semantics; writes any
    /// result register itself. ctrl=1 means branch/skip taken.
    pub step: usize,
    /// fn(realm, proto, pc, base_bytes, a, argc, depth) -> JitRet
    pub call: usize,
    /// fn(realm) -> JitRet{0|sentinel, stack_ptr}
    pub safepoint: usize,
}

#[repr(C)]
pub struct JitRet {
    pub val: u64,
    pub stack: u64,
}

/// Compiled function ABI:
/// extern "C" fn(realm, proto, base_bytes, closure_u32, depth) -> JitRet
/// where JitRet.val = 0 ok / 1 error (RtError in realm.jit_error),
/// JitRet.stack = return Value bits when ok.
pub type CompiledFn =
    extern "C" fn(*mut core::ffi::c_void, *const FunctionProto, u64, u32, u32) -> JitRet;

const SAFEPOINT_INTERVAL: u16 = 1024;
pub const MAX_CODE: usize = 16 * 1024;

const R_REALM: u32 = 19;
const R_BASE: u32 = 20;
const R_CLOSURE: u32 = 22;
const R_DEPTH: u32 = 23;
const R_POLL: u32 = 24;
const R_SLOTS: u32 = 25;
const R_PROTO: u32 = 26;
const R_SENTINEL: u32 = 27;
const R_TAGLIM: u32 = 28; // holds 0xFFF9 (tag threshold; > imm12 so needs a reg)

struct C {
    a: Asm,
    pc_labels: Vec<Label>,
    bail: Label,
    ret: Label,
    helpers: Helpers,
}

impl C {
    fn slot_off(i: u8) -> u32 {
        i as u32 * 8
    }

    fn load_slot(&mut self, x: u32, i: u8) {
        self.a.ldr_imm(x, R_SLOTS, Self::slot_off(i));
    }
    fn store_slot(&mut self, x: u32, i: u8) {
        self.a.str_imm(x, R_SLOTS, Self::slot_off(i));
    }

    /// x10 = tag of x{src}; branch to `slow` when not a number.
    /// (tag >= 0xFFF9 means non-number; 0xFFF9 lives in R_TAGLIM because it
    /// exceeds the 12-bit compare immediate.)
    fn guard_number(&mut self, src: u32, slow: Label) {
        self.a.lsr_imm(10, src, 48);
        self.a.cmp_reg(10, R_TAGLIM);
        self.a.b_cond(Cond::Hs, slow);
    }

    /// Call the single-step helper for instruction `pc`; returns with
    /// ctrl in x0 (sentinel checked -> bail). Stack ptr can't change
    /// (step never pushes frames), but we reload defensively anyway.
    fn call_step(&mut self, pc: usize) {
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
    }
}

/// Compile a proto to Tier-1 native code. Returns None when ineligible.
pub fn compile(proto: &FunctionProto, helpers: Helpers) -> Option<Vec<u32>> {
    if proto.is_async || proto.code.len() > MAX_CODE {
        return None;
    }
    // Calls inside loops lose to the interpreter's inline native-call arm
    // (helper hop per iteration) — leave those functions interpreted.
    // ponytail: revisit with an inline native fast path in the template.
    for (pc, ins) in proto.code.iter().enumerate() {
        if ins.op == Op::Jump && ins.sbx() < 0 {
            let target = (pc as i64 + ins.sbx() as i64 + 1) as usize;
            if proto.code[target..pc].iter().any(|i| i.op == Op::Call) {
                return None;
            }
        }
    }
    let mut c = {
        let mut a = Asm::new();
        let bail = a.new_label();
        let ret = a.new_label();
        let pc_labels: Vec<Label> = (0..proto.code.len() + 2).map(|_| a.new_label()).collect();
        C { a, pc_labels, bail, ret, helpers }
    };

    // prologue
    c.a.stp_pre(29, 30, SP, -16);
    c.a.stp_pre(19, 20, SP, -16);
    c.a.stp_pre(21, 22, SP, -16);
    c.a.stp_pre(23, 24, SP, -16);
    c.a.stp_pre(25, 26, SP, -16);
    c.a.stp_pre(27, 28, SP, -16);
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

    for (pc, ins) in proto.code.iter().enumerate() {
        let l = c.pc_labels[pc];
        c.a.bind(l);
        emit_op(&mut c, pc, *ins);
    }
    // trailing label (jump targets can be code.len())
    let end = c.pc_labels[proto.code.len()];
    c.a.bind(end);
    let end2 = c.pc_labels[proto.code.len() + 1];
    c.a.bind(end2);
    // falling off the end: return undefined (emitter guarantees terminators,
    // this is belt-and-braces)
    c.a.mov_imm64(1, Value::UNDEFINED.bits());
    c.a.b(c.ret);

    // shared return: x1 = value, x0 = 0
    let ret = c.ret;
    c.a.bind(ret);
    c.a.movz(0, 0, 0);
    let out = c.a.new_label();
    c.a.b(out);
    // bail: x0 = 1 (error in realm.jit_error)
    let bail = c.bail;
    c.a.bind(bail);
    c.a.movz(0, 1, 0);
    c.a.bind(out);
    c.a.ldp_post(27, 28, SP, 16);
    c.a.ldp_post(25, 26, SP, 16);
    c.a.ldp_post(23, 24, SP, 16);
    c.a.ldp_post(21, 22, SP, 16);
    c.a.ldp_post(19, 20, SP, 16);
    c.a.ldp_post(29, 30, SP, 16);
    c.a.ret();

    Some(c.a.finish())
}

fn js_cond(op: Op) -> Cond {
    // fcmp-based JS comparison conditions (false on unordered/NaN)
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

fn emit_op(c: &mut C, pc: usize, ins: Instr) {
    match ins.op {
        // ---- pure inline ----
        Op::LoadInt => {
            c.a.mov_imm64(8, Value::number(ins.sbx() as f64).bits());
            c.store_slot(8, ins.a);
        }
        Op::LoadBool => {
            let v = if ins.b != 0 { Value::TRUE } else { Value::FALSE };
            c.a.mov_imm64(8, v.bits());
            c.store_slot(8, ins.a);
        }
        Op::LoadNull => {
            c.a.mov_imm64(8, Value::NULL.bits());
            c.store_slot(8, ins.a);
        }
        Op::LoadUndef => {
            c.a.mov_imm64(8, Value::UNDEFINED.bits());
            c.store_slot(8, ins.a);
        }
        Op::Move => {
            c.load_slot(8, ins.b);
            c.store_slot(8, ins.a);
        }

        // ---- number fast path + step fallback ----
        Op::Add | Op::Sub | Op::Mul | Op::Div => {
            let slow = c.a.new_label();
            let done = c.a.new_label();
            c.load_slot(8, ins.b);
            c.load_slot(9, ins.c);
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
            c.a.fmov_xd(8, 0);
            c.store_slot(8, ins.a);
            c.a.b(done);
            c.a.bind(slow);
            c.call_step(pc);
            c.a.bind(done);
        }
        Op::Neg => {
            let slow = c.a.new_label();
            let done = c.a.new_label();
            c.load_slot(8, ins.b);
            c.guard_number(8, slow);
            c.a.fmov_dx(0, 8);
            c.a.fneg(0, 0);
            c.a.fmov_xd(8, 0);
            c.store_slot(8, ins.a);
            c.a.b(done);
            c.a.bind(slow);
            c.call_step(pc);
            c.a.bind(done);
        }

        Op::Lt | Op::Le | Op::Gt | Op::Ge => {
            let slow = c.a.new_label();
            let done = c.a.new_label();
            c.load_slot(8, ins.b);
            c.load_slot(9, ins.c);
            c.guard_number(8, slow);
            c.guard_number(9, slow);
            c.a.fmov_dx(0, 8);
            c.a.fmov_dx(1, 9);
            c.a.fcmp(0, 1);
            c.a.cset(8, js_cond(ins.op));
            // bool value = FALSE_bits + (0|1)
            c.a.mov_imm64(9, Value::FALSE.bits());
            c.a.add_reg(8, 9, 8);
            c.store_slot(8, ins.a);
            c.a.b(done);
            c.a.bind(slow);
            c.call_step(pc);
            c.a.bind(done);
        }
        Op::Eq | Op::Ne => {
            // strict_eq: numbers via fcmp; identical non-string bits equal;
            // everything else via step
            let slow = c.a.new_label();
            let done = c.a.new_label();
            c.load_slot(8, ins.b);
            c.load_slot(9, ins.c);
            c.guard_number(8, slow);
            c.guard_number(9, slow);
            c.a.fmov_dx(0, 8);
            c.a.fmov_dx(1, 9);
            c.a.fcmp(0, 1);
            c.a.cset(8, js_cond(ins.op));
            c.a.mov_imm64(9, Value::FALSE.bits());
            c.a.add_reg(8, 9, 8);
            c.store_slot(8, ins.a);
            c.a.b(done);
            c.a.bind(slow);
            c.call_step(pc);
            c.a.bind(done);
        }

        Op::LtSkip | Op::LeSkip | Op::GtSkip | Op::GeSkip | Op::EqSkip | Op::NeSkip => {
            // skip == jump to pc+2 (the next instruction is a Jump)
            let slow = c.a.new_label();
            let done = c.a.new_label();
            let skip_to = c.pc_labels[pc + 2];
            c.load_slot(8, ins.b);
            c.load_slot(9, ins.c);
            c.guard_number(8, slow);
            c.guard_number(9, slow);
            c.a.fmov_dx(0, 8);
            c.a.fmov_dx(1, 9);
            c.a.fcmp(0, 1);
            c.a.b_cond(js_cond(ins.op), skip_to);
            c.a.b(done);
            c.a.bind(slow);
            c.call_step(pc);
            // step returns ctrl=1 when the comparison was true (skip)
            c.a.cbnz(0, skip_to);
            c.a.bind(done);
        }

        Op::Jump => {
            let target = (pc as i64 + ins.sbx() as i64 + 1) as usize;
            if ins.sbx() < 0 {
                // back-edge: amortized safepoint
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
            let fall = c.a.new_label();
            let slow = c.a.new_label();
            c.load_slot(8, ins.a);
            c.guard_number(8, slow);
            // number truthiness: falsy iff 0.0/-0.0/NaN
            c.a.fmov_dx(0, 8);
            c.a.fcmp_zero(0);
            match ins.op {
                Op::JumpIfFalse => {
                    c.a.b_cond(Cond::Eq, t); // zero
                    c.a.b_cond(Cond::Vs, t); // NaN
                    c.a.b(fall);
                }
                _ => {
                    c.a.b_cond(Cond::Eq, fall);
                    c.a.b_cond(Cond::Vs, fall);
                    c.a.b(t);
                }
            }
            c.a.bind(slow);
            c.call_step(pc);
            c.a.cbnz(0, t); // ctrl=1: branch taken
            c.a.bind(fall);
        }

        Op::Call => {
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
        }
        Op::Return => {
            c.load_slot(1, ins.a);
            c.a.b(c.ret);
        }
        Op::Halt => {
            c.a.mov_imm64(1, Value::UNDEFINED.bits());
            c.a.b(c.ret);
        }

        // ---- everything else: exact interpreter semantics via step ----
        _ => {
            c.call_step(pc);
        }
    }
}
