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
    // thin per-op helpers (no instruction re-decode, no Arc clones):
    /// fn(realm, proto, pc, obj_bits, const_idx) -> JitRet{val, stack}
    pub get_field: usize,
    /// fn(realm, proto, pc, obj_bits, const_idx, v_bits) -> JitRet
    pub set_field: usize,
    /// fn(realm, proto, pc, obj_bits, idx_bits) -> JitRet{val, stack}
    pub get_index: usize,
    /// fn(realm, proto, pc, target_bits, idx_bits, v_bits) -> JitRet
    pub set_index: usize,
    /// fn(realm, proto, pc, v_bits) -> JitRet{val, stack}
    pub len: usize,
    /// fn(realm, proto, pc, arr_bits, v_bits) -> JitRet
    pub push: usize,
    /// fn(realm, proto, pc, const_idx) -> JitRet{val, stack}
    pub get_global: usize,
    // closure-cell thin helpers (Tier-2 only; args by value, no GC):
    /// fn(realm, closure_u32, idx) -> JitRet{val, stack}
    pub get_upval: usize,
    /// fn(realm, closure_u32, idx, v_bits) -> JitRet
    pub set_upval: usize,
    /// fn(realm, proto, pc, cell_bits) -> JitRet{val, stack}
    pub load_cell: usize,
    /// fn(realm, proto, pc, cell_bits, v_bits) -> JitRet
    pub store_cell: usize,
    /// fn(realm, init_bits) -> JitRet{cell_value_bits, stack}
    pub new_cell: usize,
    /// fn(realm) -> JitRet{obj_value_bits, stack}
    pub new_object: usize,
    /// fn(realm, cap) -> JitRet{arr_value_bits, stack}
    pub new_array: usize,
    /// fn(realm, proto, pc, base_bytes, first, cidx) -> JitRet{obj_bits, stack}
    /// Fused object literal: values read from slots [first, first+n).
    pub new_object_lit: usize,
    /// fn(realm, base_bytes, first, n) -> JitRet{arr_bits, stack}
    pub new_array_lit: usize,
    /// fn(realm, proto, pc, base_bytes, closure_u32) -> JitRet{closure_bits, stack}
    /// Builds the closure for Op::Closure at pc (upval sources read from slots).
    pub new_closure: usize,
    /// fn(realm, b_bits, c_bits) -> JitRet{str_bits, stack}
    pub concat: usize,
    /// fn(realm, proto, pc, b_bits, c_bits) -> JitRet{val, stack}
    /// Slow-path `+`: string concatenation or a type error.
    pub add_slow: usize,
    /// fn(realm, proto, bx) -> JitRet{val_bits, stack} — string constants.
    pub load_const: usize,
    /// fn(realm, base_bytes, first, n, shape_ptr) -> JitRet{obj_bits, stack}
    /// Fused object literal with the final shape baked in at compile time.
    pub new_object_lit2: usize,
    /// fn(realm, awaited_bits, dst, resume_pc) -> JitRet{val | sentinels, stack}
    /// Settled: the value. Rejected: error sentinel. Pending: the await
    /// sentinel, with suspend info stashed in realm.jit_await.
    pub await_: usize,
    /// fn(realm, proto, ret_val, ret_stack, new_base_bytes, closure, depth)
    /// -> JitRet{result_bits | err sentinel, stack}. Finishes a direct call
    /// that bailed (deopt) or errored.
    pub call_resume: usize,
}

/// Probed heap layout offsets (tsr-realm::layout). None disables the
/// inline read paths — helpers still handle everything.
#[derive(Clone, Copy)]
pub struct HeapOffsets {
    /// Offset of realm.stack's data pointer inside Realm.
    pub realm_stack_ptr: u32,
    pub realm_objs_ptr: u32,
    pub realm_arrs_ptr: u32,
    /// Nursery (young) arena data pointers — templates select the base on
    /// payload bit 31 and mask the index.
    pub nursery_objs_ptr: u32,
    pub nursery_arrs_ptr: u32,
    /// [old, young] base-pair tables in Realm: one shifted load selects.
    pub obj_bases_off: u32,
    pub arr_bases_off: u32,
    /// Inline bump-allocation: nursery.objs len/cap + nursery.bytes word
    /// offsets in Realm, the raw words of an empty Vec<Value>, and Obj
    /// field offsets.
    pub nursery_objs_len: u32,
    pub nursery_objs_cap: u32,
    pub nursery_bytes_off: u32,
    /// nursery.arrs len/cap and the array-buffer pool Vec words (inline
    /// array literals pop a pooled buffer and bump a nursery slot).
    pub nursery_arrs_len: u32,
    pub nursery_arrs_cap: u32,
    pub pool_arr_ptr: u32,
    pub pool_arr_len: u32,
    pub pool_arr_cap: u32,
    pub pretenure_off: u32,
    pub objs_len: u32,
    pub objs_cap: u32,
    pub arrs_len: u32,
    pub arrs_cap: u32,
    pub free_objs_len: u32,
    pub free_arrs_len: u32,
    pub objs_young_ptr: u32,
    pub objs_young_len: u32,
    pub objs_young_cap: u32,
    pub arrs_young_ptr: u32,
    pub arrs_young_len: u32,
    pub arrs_young_cap: u32,
    pub allocs_since_gc_off: u32,
    pub arrs_old_ptr: u32,
    pub arrs_old_len: u32,
    pub arrs_dirty_ptr: u32,
    pub arrs_dirty_len: u32,
    pub objs_old_ptr: u32,
    pub objs_old_len: u32,
    pub objs_dirty_ptr: u32,
    pub objs_dirty_len: u32,
    pub const_cache_ptr: u32,
    pub free_closures_len: u32,
    pub closures_young_ptr: u32,
    pub closures_young_len: u32,
    pub closures_young_cap: u32,
    pub empty_vec_words: [u64; 3],
    pub obj_vlen: u32,
    pub obj_overflow: u32,
    /// Inline bump path enabled (nursery on for this process).
    pub bump_alloc: bool,
    pub obj_size: u32,
    pub obj_shape_arc: u32,
    pub shape_id_delta: u32,
    /// Offset of the inline value slots inside Obj.
    pub obj_inline: u32,
    /// Number of inline slots (IC slots >= this take the helper).
    pub obj_inline_n: u32,
    pub realm_closures_ptr: u32,
    pub realm_cells_ptr: u32,
    pub closure_size: u32,
    pub closure_upvals_ptr: u32,
    pub closure_proto_off: u32,
    pub realm_stack_len: u32,
    pub arr_size: u32,
    pub vec_ptr: u32,
    pub vec_len: u32,
    pub vec_cap: u32,
}

#[repr(C)]
pub struct JitRet {
    pub val: u64,
    pub stack: u64,
}

/// Compiled function ABI:
/// extern "C" fn(realm, proto, base_bytes, closure_u32, depth, start_pc)
/// -> JitRet where JitRet.val = 0 ok / 1 error / 2 deopt(resume_pc in
/// stack), JitRet.stack = return Value bits when ok. `start_pc` != 0
/// enters at that loop header (OSR; Tier-1 only — slots are the frame).
pub type CompiledFn = extern "C" fn(
    *mut core::ffi::c_void,
    *const FunctionProto,
    u64,
    u32,
    u32,
    u64,
) -> JitRet;

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
    offsets: Option<HeapOffsets>,
    ics_base: u64,
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


    /// x{base} = arena data pointer selected by payload bit 31 of
    /// w{refr} (old vs nursery); leaves the masked index in w{refr}.
    /// One shifted load from the realm's [old, young] base-pair table
    /// (refreshed by Heap::refresh_bases on every pointer move).
    fn arena_base(&mut self, base: u32, refr: u32, table_off: u32) {
        self.a.lsr_imm(13, refr, 31);
        if table_off < 4096 {
            self.a.add_imm(11, R_REALM, table_off);
        } else {
            self.a.mov_imm64(11, table_off as u64);
            self.a.add_reg(11, R_REALM, 11);
        }
        self.a.ldr_reg_lsl3(base, 11, 13);
        self.a.ubfx32(refr, refr, 0, 31);
    }
    /// x{dst} = x{base} + w{idx} * size (size folded as shift when pow2).
    fn index_addr(&mut self, dst: u32, base: u32, idx: u32, size: u32) {
        if size.is_power_of_two() {
            self.a.add_reg_lsl(dst, base, idx, size.trailing_zeros());
        } else {
            self.a.mov_imm64(11, size as u64);
            self.a.mul(11, idx, 11);
            self.a.add_reg(dst, base, 11);
        }
    }

    /// blr a thin helper whose args are already staged; sentinel check +
    /// stack-ptr refresh.
    fn thin(&mut self, addr: usize) {
        self.a.mov_imm64(8, addr as u64);
        self.a.blr(8);
        self.a.cmp_reg(0, R_SENTINEL);
        self.a.b_cond(Cond::Eq, self.bail);
        self.a.add_reg(R_SLOTS, 1, R_BASE);
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
/// `for_osr`: compiled as an OSR target — calls-in-loops allowed (the
/// alternative there is staying interpreted, not a faster tier).
pub fn compile(
    proto: &FunctionProto,
    helpers: Helpers,
    for_osr: bool,
    offsets: Option<HeapOffsets>,
    ics_base: u64,
) -> Option<Vec<u32>> {
    let pbody = proto.body();
    if proto.is_async || pbody.code.len() > MAX_CODE {
        return None;
    }
    // Calls inside loops lose to the interpreter's inline native-call arm
    // (measured on fnv). Exception: OSR entry into loops that also touch
    // the heap (objects/arrays/closures) — those shapes win in native
    // even paying the call-helper hop.
    // allocation/mutation ops — method-lookup GetFields (Math.imul) must
    // NOT qualify, or tight native-call loops (fnv) regress
    let heap_op = |op: Op| {
        matches!(
            op,
            Op::SetField
                | Op::SetIndex
                | Op::ArrayPush
                | Op::NewObject
                | Op::NewArray
                | Op::NewObjectLit
                | Op::NewArrayLit
                | Op::Closure
                | Op::Concat
        )
    };
    // function-level: OSR may compile call-containing loops when the
    // function allocates/mutates anywhere (object/closure churn shapes);
    // pure arithmetic-and-call functions (fnv shape) stay interpreted
    let fn_mutates = pbody.code.iter().any(|i| heap_op(i.op));
    for (pc, ins) in pbody.code.iter().enumerate() {
        if ins.op == Op::Jump && ins.sbx() < 0 {
            let target = (pc as i64 + ins.sbx() as i64 + 1) as usize;
            let body = &pbody.code[target..pc];
            if body.iter().any(|i| i.op == Op::Call) {
                let allowed = for_osr && fn_mutates;
                if !allowed {
                    return None;
                }
            }
        }
    }
    let mut c = {
        let mut a = Asm::new();
        let bail = a.new_label();
        let ret = a.new_label();
        let pc_labels: Vec<Label> = (0..pbody.code.len() + 2).map(|_| a.new_label()).collect();
        C { a, pc_labels, bail, ret, helpers, offsets, ics_base }
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
    // stash start_pc (x5) across the stack_ptr helper call
    c.a.mov(15, 5);
    c.a.mov_imm64(8, helpers.stack_ptr as u64);
    c.a.blr(8);
    c.a.add_reg(R_SLOTS, 1, R_BASE);

    // OSR dispatch: start_pc != 0 jumps to the matching loop header
    // (register state is the stack slots — always current in Tier-1)
    let normal = c.a.new_label();
    c.a.cbz(15, normal);
    let mut headers: Vec<usize> = pbody
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
        c.a.cmp_reg(15, 8);
        let l = c.pc_labels[h];
        c.a.b_cond(Cond::Eq, l);
    }
    c.a.bind(normal);

    for (pc, ins) in pbody.code.iter().enumerate() {
        let l = c.pc_labels[pc];
        c.a.bind(l);
        emit_op(&mut c, pc, *ins);
    }
    // trailing label (jump targets can be code.len())
    let end = c.pc_labels[pbody.code.len()];
    c.a.bind(end);
    let end2 = c.pc_labels[pbody.code.len() + 1];
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

        Op::Mod => {
            // guarded integer fast path (sdiv/msub, sign-of-dividend, ±0
            // via dividend sign); everything else -> step (fmod/errors)
            let slow = c.a.new_label();
            let done = c.a.new_label();
            c.load_slot(8, ins.b);
            c.load_slot(9, ins.c);
            c.guard_number(8, slow);
            c.guard_number(9, slow);
            c.a.fmov_dx(0, 8);
            c.a.fmov_dx(1, 9);
            c.a.fcvtzs(10, 0);
            c.a.scvtf(2, 10);
            c.a.fcmp(2, 0);
            c.a.b_cond(Cond::Ne, slow);
            c.a.fcvtzs(11, 1);
            c.a.scvtf(3, 11);
            c.a.fcmp(3, 1);
            c.a.b_cond(Cond::Ne, slow);
            c.a.cbz(11, slow);
            c.a.sdiv(12, 10, 11);
            c.a.msub(13, 12, 11, 10);
            let nonzero = c.a.new_label();
            c.a.cbnz(13, nonzero);
            c.a.mov_imm64(14, 0x8000_0000_0000_0000);
            c.a.and_reg(8, 8, 14); // ±0 bits from dividend sign
            c.store_slot(8, ins.a);
            c.a.b(done);
            c.a.bind(nonzero);
            c.a.scvtf(0, 13);
            c.a.fmov_xd(8, 0);
            c.store_slot(8, ins.a);
            c.a.b(done);
            c.a.bind(slow);
            c.call_step(pc);
            c.a.bind(done);
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

        Op::GetField => {
            let slow = c.a.new_label();
            let done = c.a.new_label();
            if let Some(o) = c.offsets {
                // inline IC'd property load: tag check, shape-id compare
                // against the per-pc cache, direct slot load. Reads only —
                // no allocation, so arena pointers are stable throughout.
                c.load_slot(8, ins.b);
                c.a.lsr_imm(10, 8, 48);
                c.a.movz(11, 0xFFFB, 0); // TAG_OBJ
                c.a.cmp_reg(10, 11);
                c.a.b_cond(Cond::Ne, slow);
                c.a.orr_reg32(9, 31, 8); // w9 = payload ref
                c.arena_base(10, 9, o.obj_bases_off);
                c.index_addr(10, 10, 9, o.obj_size);
                c.a.ldr_imm(11, 10, o.obj_shape_arc);
                c.a.ldr_w_imm(12, 11, o.shape_id_delta);
                c.a.mov_imm64(13, c.ics_base + (pc as u64) * 8);
                c.a.ldr_imm(14, 13, 0);
                c.a.lsr_imm(15, 14, 32);
                c.a.cmp_reg(15, 12);
                c.a.b_cond(Cond::Ne, slow); // empty IC or shape miss
                c.a.sub_imm32(16, 14, 1); // slot (+1 encoding)
                c.a.cmp_imm(16, o.obj_inline_n);
                c.a.b_cond(Cond::Hs, slow); // overflow slot -> helper
                c.index_addr(17, 10, 16, 8);
                c.a.ldr_imm(8, 17, o.obj_inline);
                c.store_slot(8, ins.a);
                c.a.b(done);
            }
            c.a.bind(slow);
            c.a.mov(0, R_REALM);
            c.a.mov(1, R_PROTO);
            c.a.mov_imm64(2, pc as u64);
            c.load_slot(3, ins.b);
            c.a.mov_imm64(4, ins.c as u64);
            c.thin(c.helpers.get_field);
            c.a.mov(8, 0);
            c.store_slot(8, ins.a);
            c.a.bind(done);
        }
        Op::SetField => {
            c.a.mov(0, R_REALM);
            c.a.mov(1, R_PROTO);
            c.a.mov_imm64(2, pc as u64);
            c.load_slot(3, ins.a);
            c.a.mov_imm64(4, ins.b as u64);
            c.load_slot(5, ins.c);
            c.thin(c.helpers.set_field);
        }
        Op::GetIndex => {
            let slow = c.a.new_label();
            let done = c.a.new_label();
            if let Some(o) = c.offsets {
                c.load_slot(8, ins.b);
                c.a.lsr_imm(10, 8, 48);
                c.a.movz(11, 0xFFFC, 0); // TAG_ARR
                c.a.cmp_reg(10, 11);
                c.a.b_cond(Cond::Ne, slow);
                c.load_slot(9, ins.c);
                c.guard_number(9, slow);
                c.a.orr_reg32(12, 31, 8); // ref
                c.arena_base(10, 12, o.arr_bases_off);
                c.index_addr(10, 10, 12, o.arr_size);
                // exact-integer index: fcvtzs/scvtf round trip
                c.a.fmov_dx(0, 9);
                c.a.fcvtzs(13, 0);
                c.a.scvtf(1, 13);
                c.a.fcmp(1, 0);
                c.a.b_cond(Cond::Ne, slow); // fractional / NaN / huge
                c.a.ldr_imm(14, 10, o.vec_len);
                c.a.cmp_reg(13, 14);
                c.a.b_cond(Cond::Hs, slow); // OOB or negative (unsigned)
                c.a.ldr_imm(15, 10, o.vec_ptr);
                c.a.ldr_reg_lsl3(8, 15, 13);
                c.store_slot(8, ins.a);
                c.a.b(done);
            }
            c.a.bind(slow);
            c.a.mov(0, R_REALM);
            c.a.mov(1, R_PROTO);
            c.a.mov_imm64(2, pc as u64);
            c.load_slot(3, ins.b);
            c.load_slot(4, ins.c);
            c.thin(c.helpers.get_index);
            c.a.mov(8, 0);
            c.store_slot(8, ins.a);
            c.a.bind(done);
        }
        Op::SetIndex => {
            c.a.mov(0, R_REALM);
            c.a.mov(1, R_PROTO);
            c.a.mov_imm64(2, pc as u64);
            c.load_slot(3, ins.a);
            c.load_slot(4, ins.b);
            c.load_slot(5, ins.c);
            c.thin(c.helpers.set_index);
        }
        Op::Len => {
            let slow = c.a.new_label();
            let done = c.a.new_label();
            if let Some(o) = c.offsets {
                c.load_slot(8, ins.b);
                c.a.lsr_imm(10, 8, 48);
                c.a.movz(11, 0xFFFC, 0);
                c.a.cmp_reg(10, 11);
                c.a.b_cond(Cond::Ne, slow); // strings etc -> helper
                c.a.orr_reg32(12, 31, 8);
                c.arena_base(10, 12, o.arr_bases_off);
                c.index_addr(10, 10, 12, o.arr_size);
                c.a.ldr_imm(14, 10, o.vec_len);
                c.a.scvtf(0, 14);
                c.a.fmov_xd(8, 0);
                c.store_slot(8, ins.a);
                c.a.b(done);
            }
            c.a.bind(slow);
            c.a.mov(0, R_REALM);
            c.a.mov(1, R_PROTO);
            c.a.mov_imm64(2, pc as u64);
            c.load_slot(3, ins.b);
            c.thin(c.helpers.len);
            c.a.mov(8, 0);
            c.store_slot(8, ins.a);
            c.a.bind(done);
        }
        Op::ArrayPush => {
            c.a.mov(0, R_REALM);
            c.a.mov(1, R_PROTO);
            c.a.mov_imm64(2, pc as u64);
            c.load_slot(3, ins.a);
            c.load_slot(4, ins.b);
            c.thin(c.helpers.push);
        }
        Op::GetGlobal => {
            c.a.mov(0, R_REALM);
            c.a.mov(1, R_PROTO);
            c.a.mov_imm64(2, pc as u64);
            c.a.mov_imm64(3, ins.bx() as u64);
            c.thin(c.helpers.get_global);
            c.a.mov(8, 0);
            c.store_slot(8, ins.a);
        }

        // ---- everything else: exact interpreter semantics via step ----
        _ => {
            c.call_step(pc);
        }
    }
}
