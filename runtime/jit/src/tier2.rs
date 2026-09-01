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
use tsr_memory::{Value, JIT_AWAIT_SENTINEL, JIT_ERR_SENTINEL};

const R_REALM: u32 = 19;
const R_BASE: u32 = 20;
/// Current closure, unless this function traded it for the array-base
/// cache (`R_ACACHE`).
const R_CLOSURE: u32 = 22;
/// Cached array data pointer, 0 when invalid — same discipline as the
/// field cache in x15. Only live when `int_lane`.
const R_ACACHE: u32 = 22;
const CLOSURE_SLOT: u32 = 8;
const R_POLL: u32 = 24;
const R_PROTO: u32 = 26;
/// Call depth, unless this function traded it for the integer lane.
const R_DEPTH: u32 = 23;
/// Frame slot the call depth moves to when this function wants x23 for
/// the integer lane.
///
/// Freeing a callee-saved register is not free. The reads themselves are
/// — `ldr` costs what the `mov` it replaces did — but the slot needs a
/// push and a pop, and a function full of small calls pays those on every
/// entry and exit. Measured at about 1% on promises when applied
/// unconditionally, so the frame only grows for functions that have an
/// integer lane to spend the register on.
///
/// Its neighbours stay in registers regardless: the proto pointer is read
/// by `step_full` on every generic slow path (1.5% on promises), and the
/// safepoint counter is decremented on every back edge, where a spill
/// would cost instructions per loop iteration.
const DEPTH_SLOT: u32 = 0;
/// The loop-carried integer lane. Holds one `int_spec` vreg as a raw
/// integer for the body of its loop, so an integer dependency chain stops
/// round-tripping through f64 between every operation.
const R_LANE: u32 = 23;
/// Holds one in-flight integer intermediate — an expression like
/// `(s * 31 + i) % M` only ever has one live at a time.
///
/// Shares x22 with the array-base cache, which is what makes it usable:
/// a caller-saved scratch register does not survive the ops that sit
/// between the halves of an expression (a `LoadConst` between the add and
/// the mod was enough to lose it), and then the lane pays to keep the
/// intermediate in sync without ever collecting the saving. A function
/// takes whichever of the two it can use, never both.
const R_ITMP: u32 = 22;
const R_SLOTS: u32 = 25;
const R_SENTINEL: u32 = 27;
const R_TAGLIM: u32 = 28;
const R_STARTPC: u32 = 15; // stashed x5, still live at the entry guards
const R_ICS: u32 = 21; // per-pc IC table base (callee-saved, survives helpers)
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
    /// per-pc (b, c): operand is a known integer constant
    pub const_ops: &'a [[Option<i64>; 2]],
    /// per-pc: warmed GetField/SetField IC contents (shape id, slot) —
    /// baked into the code as immediates, guarded by a shape compare
    pub ic_baked: &'a [Option<(u32, u32)>],
    /// loop-header speculation: (header pc, vregs) — number-guard on the
    /// fall-in edge and at OSR entry; deopt resumes at the header
    pub loop_spec: &'a [(usize, Vec<u8>)],
    /// Loop-carried vregs worth holding unboxed in an integer register.
    /// Non-empty means this function pays for the extra frame slot that
    /// frees x23; empty means its frame is untouched.
    pub int_spec: &'a [(usize, Vec<u8>)],
}

struct C {
    a: Asm,
    pc_labels: Vec<Label>,
    bail: Label,
    await_exit: Label,
    /// mid-body deopt: x0=2, x1=resume pc set by the deopting site
    deopt_exit: Label,
    ret: Label,
    helpers: Helpers,
    offsets: Option<HeapOffsets>,
    ics_base: u64,
    tics_base: u64,
    n_low: u8,
    /// This function traded x23 for a frame slot to get an integer lane.
    int_lane: bool,
    /// vreg held as a raw integer in R_LANE for the current loop.
    lane: Option<u8>,
    /// vreg whose integer value is currently in R_ITMP (block-local).
    itmp: Option<u8>,
    /// x22 is the array-base cache in this function.
    acache_on: bool,
    /// x22 is the integer lane's intermediate in this function.
    lane_on: bool,
    /// vregs the current loop header proved to hold an i32. Converting one
    /// needs no verification; anything outside this set does, because a
    /// bare `fcvtzs` would silently truncate a fraction.
    int_ok: Vec<u8>,
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

    /// Call depth, either still in its register or loaded into `scratch`.
    fn depth(&mut self, scratch: u32) -> u32 {
        if self.int_lane {
            self.a.ldr_imm(scratch, SP, DEPTH_SLOT);
            scratch
        } else {
            R_DEPTH
        }
    }

    /// A GP register holding vreg `v` as an integer.
    ///
    /// Free when the value is already in the lane or is the in-flight
    /// intermediate; otherwise it costs the `fcvtzs` that the boxed
    /// representation requires. Callers must have proven `v` numeric.
    fn int_src(&mut self, v: u8, scratch: u32) -> u32 {
        if self.lane == Some(v) {
            return R_LANE;
        }
        if self.itmp == Some(v) {
            return R_ITMP;
        }
        let d = self.fetch(v, 0);
        self.a.fcvtzs(scratch, d);
        scratch
    }

    /// Record that `v`'s integer value is in `src`, and refresh its boxed
    /// home so every other reader — a later block, a bail, a safepoint
    /// spill — still sees the truth.
    ///
    /// The conversion is off the dependency chain: the next operation
    /// reads the integer register, not the home, so it retires alongside
    /// rather than in front of it. That is what makes syncing every write
    /// affordable here, and it is what keeps every existing exit path
    /// (bail, deopt, safepoint) correct with no extra machinery.
    fn int_dst(&mut self, v: u8, src: u32, pc: usize) {
        // A result that does not fit i32 is one f64 would have carried
        // exactly and the integer form would not. Deopt to this pc: the
        // inputs are all still live in their homes, so the interpreter
        // re-runs the operation from correct state.
        let ok = self.a.new_label();
        self.a.sxtw(12, src);
        self.a.cmp_reg(12, src);
        self.a.b_cond(Cond::Eq, ok);
        self.spill_low();
        self.a.movz(0, 2, 0);
        self.a.mov_imm64(1, pc as u64);
        self.a.b(self.deopt_exit);
        self.a.bind(ok);

        let home = if self.lane == Some(v) { R_LANE } else { R_ITMP };
        if src != home {
            self.a.mov(home, src);
        }
        if self.lane == Some(v) {
            self.itmp = None;
        } else {
            self.itmp = Some(v);
        }
        self.a.scvtf(3, home);
        self.put(v, 3);
    }

    /// Current closure, either still in its register or loaded.
    fn closure(&mut self, scratch: u32) -> u32 {
        if self.int_lane {
            self.a.ldr_imm(scratch, SP, CLOSURE_SLOT);
            scratch
        } else {
            R_CLOSURE
        }
    }

    /// x{dst} = the array's `Vec<Value>` address, tag-checked.
    ///
    /// Deriving this is thirteen instructions — fetch the value, check the
    /// tag, select the arena on the young bit, scale by the slot stride —
    /// and a loop reading `a.length` then `a[i]` derives the same pointer
    /// twice. When `reuse` is set the previous one is still in R_ACACHE
    /// (zeroed by any safepoint or slow path) and it collapses to a test
    /// and a move.
    fn array_base(&mut self, dst: u32, v: u8, off: u32, size: u32, slow: Label, reuse: bool) {
        let derived = self.a.new_label();
        let cached = self.a.new_label();
        if reuse {
            self.a.cbnz(R_ACACHE, cached);
        }
        self.fetch_x(v, 8);
        self.a.lsr_imm(10, 8, 48);
        self.a.movz(11, 0xFFFC, 0); // TAG_ARR
        self.a.cmp_reg(10, 11);
        self.a.b_cond(Cond::Ne, slow);
        self.a.orr_reg32(12, 31, 8);
        self.arena_base(dst, 12, off);
        self.index_addr(dst, dst, 12, size);
        if self.acache_on {
            self.a.mov(R_ACACHE, dst);
        }
        if reuse {
            self.a.b(derived);
            self.a.bind(cached);
            self.a.mov(dst, R_ACACHE);
            self.a.bind(derived);
        }
    }

    /// The field-access CSE cache lives in x15 (validated obj address;
    /// 0 = invalid) and x16 (its shape id). Both are caller-saved, so any
    /// emitted blr must zero x15 — cached accesses guard with one cbz.
    fn zero_cache(&mut self) {
        self.a.movz(15, 0, 0);
        if self.acache_on {
            self.a.movz(R_ACACHE, 0, 0);
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
        self.zero_cache();
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
        let cl = self.closure(4);
        self.a.mov(4, cl);
        self.a.mov_imm64(8, self.helpers.step as u64);
        self.a.blr(8);
        self.a.cmp_reg(0, R_SENTINEL);
        self.a.b_cond(Cond::Eq, self.bail);
        self.a.add_reg(R_SLOTS, 1, R_BASE);
        self.reload_low();
        self.zero_cache();
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
    tics_base: u64,
    lit_shapes: &[(u64, u32)],
    inlines: &[Option<(u64, std::sync::Arc<FunctionProto>)>],
) -> Option<Vec<u32>> {
    let pbody = proto.body();
    if pbody.code.len() > crate::tier1::MAX_CODE {
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
                | Op::NewObjectLit
                | Op::NewArrayLit
                | Op::Closure
                | Op::Concat
        )
    };
    let fn_mutates = pbody.code.iter().any(|i| heap_op(i.op));
    for (pc, ins) in pbody.code.iter().enumerate() {
        if ins.op == Op::Jump && ins.sbx() < 0 {
            let target = (pc as i64 + ins.sbx() as i64 + 1) as usize;
            let body = &pbody.code[target..pc];
            if body.iter().any(|i| i.op == Op::Call) && !(for_osr && fn_mutates) {
                return None;
            }
        }
    }

    let n_low = pbody.n_regs.min(LOW);
    let mut c = {
        let mut a = Asm::new();
        // x22 serves one purpose per function: the array-base cache where
        // there are arrays to index, otherwise the integer lane's
        // intermediate. x23 is the lane value itself.
        let acache_on = pbody.code.iter().any(|i| matches!(i.op, Op::Len | Op::GetIndex));
        let lane_on = !acache_on && !facts.int_spec.is_empty();
        let int_lane = acache_on || lane_on;
        let bail = a.new_label();
        let await_exit = a.new_label();
        let deopt_exit = a.new_label();
        let ret = a.new_label();
        let pc_labels: Vec<Label> = (0..pbody.code.len() + 2).map(|_| a.new_label()).collect();
        C { a, pc_labels, bail, await_exit, deopt_exit, ret, helpers, offsets, ics_base, tics_base, n_low, int_lane, lane: None, itmp: None, acache_on, lane_on, int_ok: Vec::new() }
    };
    let out = c.a.new_label();

    // prologue: tier1 frame + only the d-pairs this fn actually uses as
    // vreg homes (small leaf functions push/pop far less)
    let d_pairs = c.n_low.div_ceil(2) as u32;
    let has_field_ops = pbody
        .code
        .iter()
        .any(|i| matches!(i.op, Op::GetField | Op::SetField));
    c.a.stp_pre(29, 30, SP, -16);
    c.a.stp_pre(19, 20, SP, -16);
    c.a.stp_pre(21, 22, SP, -16);
    c.a.stp_pre(23, 24, SP, -16);
    c.a.stp_pre(25, 26, SP, -16);
    c.a.stp_pre(27, 28, SP, -16);
    for i in 0..d_pairs {
        // stp d(8+2i), d(9+2i), [sp, #-16]!
        let rt = 8 + 2 * i;
        c.a.raw(0x6DBF_0000 | ((rt + 1) << 10) | (31 << 5) | rt);
    }
    // depth to its frame slot when x23 is wanted elsewhere; SP does not
    // move again, so the offset stays valid for the whole body (the pair
    // keeps SP 16-byte aligned)
    if c.int_lane {
        c.a.stp_pre(4, 3, SP, -16);
        if c.acache_on {
            c.a.movz(R_ACACHE, 0, 0);
        }
    } else {
        c.a.mov(R_DEPTH, 4);
        c.a.mov(R_CLOSURE, 3);
    }
    c.a.mov(R_REALM, 0);
    c.a.mov(R_PROTO, 1);
    c.a.mov(R_BASE, 2);
    c.a.mov_imm64(R_SENTINEL, JIT_ERR_SENTINEL);
    c.a.movz(R_TAGLIM, 0xFFF9, 0);
    c.a.movz(R_POLL, SAFEPOINT_INTERVAL, 0);
    if has_field_ops {
        c.a.mov_imm64(R_ICS, ics_base);
    }
    c.a.mov(R_STARTPC, 5); // stash start_pc across the slots init
    if let Some(o) = c.offsets {
        // inline slots init: R_SLOTS = realm.stack.ptr + base_bytes
        c.a.ldr_imm(R_SLOTS, R_REALM, o.realm_stack_ptr);
        c.a.add_reg(R_SLOTS, R_SLOTS, R_BASE);
    } else {
        c.a.mov_imm64(8, helpers.stack_ptr as u64);
        c.a.blr(8);
        c.a.add_reg(R_SLOTS, 1, R_BASE);
    }

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
            c.a.cmp_reg(R_STARTPC, 8);
            let ints = c
                .lane_on
                .then(|| facts.int_spec.iter().find(|(hh, _)| *hh == h))
                .flatten();
            if facts.loop_spec.iter().any(|(hh, _)| *hh == h) || ints.is_some() {
                // speculated header: entering mid-loop must re-establish
                // the guards (slots are current; d-homes just reloaded)
                let vs: &[u8] = facts
                    .loop_spec
                    .iter()
                    .find(|(hh, _)| *hh == h)
                    .map_or(&[], |(_, v)| v.as_slice());
                let next = c.a.new_label();
                let fail = c.a.new_label();
                c.a.b_cond(Cond::Ne, next);
                for &v in vs {
                    c.fetch_x(v, 8);
                    c.a.lsr_imm(10, 8, 48);
                    c.a.cmp_reg(10, R_TAGLIM);
                    c.a.b_cond(Cond::Hs, fail);
                }
                if let Some((_, ivs)) = ints {
                    let laned = lane_pick(pbody, facts, h, ivs);
                    if laned.is_some() {
                        emit_lane_guard(&mut c, ivs, laned, fail);
                    }
                }
                let l = c.pc_labels[h];
                c.a.b(l);
                c.a.bind(fail);
                c.spill_low();
                c.a.movz(0, 2, 0);
                c.a.mov_imm64(1, h as u64);
                c.a.b(c.deopt_exit);
                c.a.bind(next);
            } else {
                let l = c.pc_labels[h];
                c.a.b_cond(Cond::Eq, l);
            }
        }
        c.a.bind(normal);
    }

    // field-access CSE: compile-time single-entry cache of which vreg's
    // validated object address/shape sit in x15/x16. Merge points kill it.
    let mut jump_targets = vec![false; pbody.code.len() + 2];
    for (pc, ins) in pbody.code.iter().enumerate() {
        match ins.op {
            Op::Jump | Op::JumpIfFalse | Op::JumpIfTrue => {
                let t = (pc as i64 + ins.sbx() as i64 + 1) as usize;
                jump_targets[t] = true;
            }
            Op::EqSkip | Op::NeSkip | Op::LtSkip | Op::LeSkip | Op::GtSkip
            | Op::GeSkip => jump_targets[pc + 2] = true,
            _ => {}
        }
    }
    let mut fcache: Option<u8> = None;
    // which vreg's array data pointer is in R_ACACHE (0 there = invalid)
    let mut acache: Option<u8> = None;
    for (pc, ins) in pbody.code.iter().enumerate() {
        if jump_targets[pc] {
            fcache = None;
            acache = None;
        }
        // loop-header speculation: guard the fall-in path (back-edges jump
        // to the label BELOW these guards and are already proven)
        // integer lane: at a loop header, take one int_spec vreg into a
        // raw integer register for the body. The guard runs on the fall-in
        // edge only — back edges branch to the label below it — so an
        // integer chain pays one conversion per loop entry instead of two
        // per iteration.
        if c.lane_on {
            if let Some((_, vs)) = facts.int_spec.iter().find(|(h, _)| *h == pc) {
                let laned = lane_pick(pbody, facts, pc, vs);
                if laned.is_some() {
                    let ok = c.a.new_label();
                    let fail = c.a.new_label();
                    emit_lane_guard(&mut c, vs, laned, fail);
                    c.a.b(ok);
                    c.a.bind(fail);
                    c.spill_low();
                    c.a.movz(0, 2, 0);
                    c.a.mov_imm64(1, pc as u64);
                    c.a.b(c.deopt_exit);
                    c.a.bind(ok);
                    c.lane = laned;
                    c.itmp = None;
                    c.int_ok = vs.clone();
                }
            }
        }
        if let Some((_, vs)) = facts.loop_spec.iter().find(|(h, _)| *h == pc) {
            let fail = c.a.new_label();
            let pass = c.a.new_label();
            for &v in vs {
                c.fetch_x(v, 8);
                c.a.lsr_imm(10, 8, 48);
                c.a.cmp_reg(10, R_TAGLIM);
                c.a.b_cond(Cond::Hs, fail);
            }
            c.a.b(pass);
            c.a.bind(fail);
            c.spill_low();
            c.a.movz(0, 2, 0);
            c.a.mov_imm64(1, pc as u64);
            c.a.b(c.deopt_exit);
            c.a.bind(pass);
        }
        let l = c.pc_labels[pc];
        c.a.bind(l);
        emit_op(&mut c, proto, pc, *ins, facts, fmod_addr, pow_addr, lit_shapes, inlines, &mut fcache, &mut acache);
        // ops with only cold-path blrs (which zero x15) preserve the cache;
        // everything else — calls, allocation, heap ops that clobber
        // x15/x16 outright — kills it at compile time
        let preserves = match ins.op {
            Op::LoadInt | Op::LoadBool | Op::LoadNull | Op::LoadUndef | Op::Move
            | Op::Add | Op::Sub | Op::Mul | Op::Div | Op::Mod | Op::Pow | Op::Neg
            | Op::BitAnd | Op::BitOr | Op::BitXor | Op::Shl | Op::Shr | Op::UShr
            | Op::BitNot | Op::Eq | Op::Ne | Op::Lt | Op::Le | Op::Gt | Op::Ge
            | Op::EqSkip | Op::NeSkip | Op::LtSkip | Op::LeSkip | Op::GtSkip
            | Op::GeSkip | Op::Jump | Op::JumpIfFalse | Op::JumpIfTrue => true,
            Op::LoadConst => matches!(pbody.consts.get(ins.bx() as usize), Some(Const::Number(_))),
            Op::GetField | Op::SetField => true, // arms manage the cache
            _ => false,
        };
        // R_ITMP is callee-saved, so an intermediate survives intervening
        // ops and calls; it dies only when its vreg is rewritten by
        // something other than an integer arm, or at a merge
        if c.itmp == Some(ins.a) && !matches!(ins.op, Op::Add | Op::Sub | Op::Mul | Op::Mod) {
            c.itmp = None;
        }
        if jump_targets[pc.saturating_add(1)] {
            c.itmp = None;
        }
        // the array cache follows the same conservative set plus its own
        // arms; a back edge runs a safepoint, which can move the arena
        let apreserves = match ins.op {
            Op::Len | Op::GetIndex => true, // arms manage it
            Op::Jump => ins.sbx() >= 0,
            _ => preserves,
        };
        if !apreserves {
            acache = None;
        } else if acache == Some(ins.a) && !matches!(ins.op, Op::Jump) {
            // the register holding the array was overwritten — including
            // by the op that just cached it, as in `a = a[0]`
            acache = None;
        }
        if !preserves {
            fcache = None;
        } else if let Some(v) = fcache {
            // an op that overwrites the cached vreg's register drops it
            let writes_a = !matches!(
                ins.op,
                Op::SetField | Op::SetIndex | Op::ArrayPush | Op::StoreCell
                    | Op::SetUpval | Op::Jump | Op::JumpIfFalse | Op::JumpIfTrue
                    | Op::EqSkip | Op::NeSkip | Op::LtSkip | Op::LeSkip
                    | Op::GtSkip | Op::GeSkip
            );
            if writes_a && ins.a == v {
                fcache = None;
            }
        }
    }
    let e1 = c.pc_labels[pbody.code.len()];
    c.a.bind(e1);
    let e2 = c.pc_labels[pbody.code.len() + 1];
    c.a.bind(e2);
    c.a.mov_imm64(1, Value::UNDEFINED.bits());
    c.a.b(c.ret);

    c.a.bind(deopt_entry);
    c.a.movz(0, 2, 0);
    c.a.mov(1, R_STARTPC);
    c.a.b(out);
    let deopt_exit = c.deopt_exit;
    c.a.bind(deopt_exit);
    c.a.b(out); // x0=2, x1=resume pc set at the deopting site

    let ret = c.ret;
    c.a.bind(ret);
    c.a.movz(0, 0, 0);
    c.a.b(out);
    let bail = c.bail;
    c.a.bind(bail);
    c.a.movz(0, 1, 0);
    c.a.b(out);
    let await_exit = c.await_exit;
    c.a.bind(await_exit);
    c.a.movz(0, 3, 0); // suspend: info stashed in realm.jit_await
    c.a.bind(out);
    if c.int_lane {
        c.a.ldp_post(9, 10, SP, 16); // discard the depth slot
    }
    for i in (0..d_pairs).rev() {
        // ldp d(8+2i), d(9+2i), [sp], #16
        let rt = 8 + 2 * i;
        c.a.raw(0x6CC1_0000 | ((rt + 1) << 10) | (31 << 5) | rt);
    }
    c.a.ldp_post(27, 28, SP, 16);
    c.a.ldp_post(25, 26, SP, 16);
    c.a.ldp_post(23, 24, SP, 16);
    c.a.ldp_post(21, 22, SP, 16);
    c.a.ldp_post(19, 20, SP, 16);
    c.a.ldp_post(29, 30, SP, 16);
    c.a.ret();

    Some(c.a.finish())
}

/// Magic-number constants for signed division by a fixed divisor:
/// (multiplier, shift) such that n/d == (smulh(n, m) >> s) + sign_fix.
/// Standard Granlund-Montgomery derivation.
fn magic_div(d: i64) -> Option<(i64, u32)> {
    if d.abs() < 2 || d == i64::MIN {
        return None;
    }
    let ad = d.unsigned_abs();
    let t = (1u128 << 63) + (if d > 0 { 0 } else { 1 });
    let anc = t - 1 - (t % ad as u128);
    let mut p: u32 = 63;
    let (mut q1, mut r1) = ((1u128 << p) / anc, (1u128 << p) % anc);
    let (mut q2, mut r2) = ((1u128 << p) / ad as u128, (1u128 << p) % ad as u128);
    loop {
        p += 1;
        if p > 127 {
            return None;
        }
        q1 <<= 1;
        r1 <<= 1;
        if r1 >= anc {
            q1 += 1;
            r1 -= anc;
        }
        q2 <<= 1;
        r2 <<= 1;
        if r2 >= ad as u128 {
            q2 += 1;
            r2 -= ad as u128;
        }
        let delta = ad as u128 - r2;
        if !(q1 < delta || (q1 == delta && r1 == 0)) {
            break;
        }
    }
    // A multiplier that needs 65 bits is the normal case for many
    // divisors, not a failure: it is carried as a negative i64 and the
    // emitter adds the dividend back, which is what the `d > 0 && m < 0`
    // arm below exists for. Rejecting it here meant `%` silently took the
    // slow path for 2, 100, 128, 1024, 1000003 and 1000000007, among
    // others — including the modulus in five of the suite's benchmarks.
    if q2 + 1 > u64::MAX as u128 {
        return None;
    }
    let mut m = (q2 + 1) as u64 as i64;
    if d < 0 {
        m = m.wrapping_neg();
    }
    Some((m, p - 64))
}

/// Prove every speculated vreg of a loop holds an i32, and put the laned
/// one in R_LANE. Emitted both on the fall-in edge and at OSR entry — a
/// loop that tiers up mid-flight arrives at the header label directly,
/// below the fall-in guard, and would otherwise run the body against an
/// uninitialised lane register.
fn emit_lane_guard(c: &mut C, vs: &[u8], laned: Option<u8>, fail: Label) {
    for &v in vs {
        let d = c.fetch(v, 0);
        c.a.fcvtzs(10, d);
        c.a.scvtf(1, 10);
        c.a.fcmp(1, d); // integral?
        c.a.b_cond(Cond::Ne, fail);
        c.a.sxtw(12, 10);
        c.a.cmp_reg(12, 10);
        c.a.b_cond(Cond::Ne, fail); // wider than i32
        if Some(v) == laned {
            c.a.mov(R_LANE, 10);
        }
    }
}

/// The laned vreg for a header, if any: the first speculated vreg whose
/// every in-loop write goes through an arm that maintains the register.
fn lane_pick(pbody: &tsc_ir::ProtoBody, facts: &Facts, h: usize, vs: &[u8]) -> Option<u8> {
    let end = pbody
        .code
        .iter()
        .enumerate()
        .filter(|(p2, i)| {
            i.op == Op::Jump && i.sbx() < 0 && (*p2 as i64 + i.sbx() as i64 + 1) as usize == h
        })
        .map(|(p2, _)| p2)
        .max()
        .unwrap_or(h);
    // Legality is not profitability. The lane only removes conversions
    // where the loop actually performs one: a Mod by a constant divisor,
    // or a bitwise op. In a loop whose Mod divisor is a variable — primes'
    // `n % d` — there is nothing to remove and the per-write syncs are
    // pure cost, measured at 23%.
    let converts = pbody.code[h..=end].iter().enumerate().any(|(off, i)| match i.op {
        Op::Mod => facts
            .const_ops
            .get(h + off)
            .and_then(|o| o[1])
            .is_some_and(|d| magic_div(d).is_some()),
        Op::BitAnd | Op::BitOr | Op::BitXor | Op::Shl | Op::Shr | Op::UShr
        | Op::BitNot => true,
        _ => false,
    });
    if !converts {
        return None;
    }
    vs.iter().copied().find(|&v| {
        pbody.code[h..=end].iter().enumerate().all(|(off, i)| {
            if i.a != v {
                return true;
            }
            match i.op {
                Op::Add | Op::Sub | Op::Mul => true,
                // a divisor with no magic number falls back to a path that
                // writes the home but not the lane register
                Op::Mod => facts
                    .const_ops
                    .get(h + off)
                    .and_then(|o| o[1])
                    .is_some_and(|d| magic_div(d).is_some()),
                Op::SetField | Op::SetIndex | Op::ArrayPush | Op::StoreCell
                | Op::SetUpval | Op::Jump | Op::JumpIfFalse | Op::JumpIfTrue
                | Op::Return | Op::EqSkip | Op::NeSkip | Op::LtSkip
                | Op::LeSkip | Op::GtSkip | Op::GeSkip => true,
                _ => false,
            }
        })
    })
}

/// Mod with a known integer divisor: the divisor needs no runtime
/// integer check, and n/d becomes smulh+shift instead of sdiv (~10-cycle
/// latency on Apple cores, and this sits in the loop-carried chain).
fn emit_mod_const(c: &mut C, a_reg: u8, db: u32, d: i64, fmod_addr: usize, int_in: Option<u32>) {
    let Some((m, sh)) = magic_div(d) else {
        // |d| < 2 or unrepresentable: fall back to the generic path
        c.a.mov_imm64(9, d as u64);
        c.a.scvtf(1, 9);
        emit_mod_num(c, a_reg, db, 1, fmod_addr);
        return;
    };
    let slow = c.a.new_label();
    let done = c.a.new_label();
    // dividend must be an exact integer (divisor is one by construction).
    // When it already lives in an integer register the check is not just
    // redundant, it is the pair of conversions this lane exists to remove.
    if let Some(src) = int_in {
        if src != 10 {
            c.a.mov(10, src);
        }
    } else {
        c.a.fcvtzs(10, db);
        c.a.scvtf(2, 10);
        c.a.fcmp(2, db);
        c.a.b_cond(Cond::Ne, slow);
    }
    // q = (smulh(n, m) >> sh) + (sign bit)
    c.a.mov_imm64(11, m as u64);
    c.a.smulh(12, 10, 11);
    if d > 0 && m < 0 {
        c.a.add_reg(12, 12, 10);
    } else if d < 0 && m > 0 {
        c.a.sub_reg(12, 12, 10);
    }
    if sh > 0 {
        c.a.asr_imm(12, 12, sh);
    }
    c.a.add_reg_lsr(12, 12, 12, 63);
    // r = n - q*d
    c.a.mov_imm64(11, d as u64);
    c.a.msub(13, 12, 11, 10);
    let nonzero = c.a.new_label();
    c.a.cbnz(13, nonzero);
    // remainder 0: ±0 carrying the dividend's sign
    c.a.fmov_xd(14, db);
    c.a.mov_imm64(12, 0x8000_0000_0000_0000);
    c.a.and_reg(14, 14, 12);
    if c.lane == Some(a_reg) {
        c.a.movz(R_LANE, 0, 0); // ±0 is 0 in the integer lane
    }
    c.put_x(a_reg, 14);
    c.a.b(done);
    c.a.bind(nonzero);
    // the lane must follow the result, or the next iteration reads the
    // previous value out of the register while the home holds the new one
    if c.lane == Some(a_reg) {
        c.a.mov(R_LANE, 13);
    }
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
    c.a.mov_imm64(9, d as u64);
    c.a.scvtf(1, 9);
    c.a.mov_imm64(8, fmod_addr as u64);
    c.a.blr(8);
    c.zero_cache();
    c.put(a_reg, 0);
    c.a.bind(done);
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
    c.zero_cache();
    c.put(a_reg, 0);
    c.a.bind(done);
}

#[allow(clippy::too_many_arguments)]
/// Callee shapes the direct-call inliner accepts: tiny, straight-line,
/// pure-register bodies whose every op has a no-fallback inline template
/// (no h_step — the pc would resolve against the caller's proto). Args
/// must be read-only (a failed mid-body guard falls back to the generic
/// call, which re-reads them).
pub fn inlinable(callee: &FunctionProto) -> bool {
    if callee.is_async || callee.arity > 4 {
        return false;
    }
    let b = callee.body();
    if b.code.len() > 12 || b.n_regs > 24 {
        return false;
    }
    b.code.iter().all(|i| {
        let dst_ok = i.a >= callee.arity || matches!(i.op, Op::Return);
        dst_ok
            && match i.op {
                Op::LoadInt
                | Op::LoadUndef
                | Op::LoadNull
                | Op::LoadBool
                | Op::Move
                | Op::Add
                | Op::Sub
                | Op::Mul
                | Op::Div
                | Op::Mod
                | Op::Neg
                | Op::BitAnd
                | Op::BitOr
                | Op::BitXor
                | Op::Shl
                | Op::Shr
                | Op::UShr
                | Op::BitNot
                | Op::Return => true,
                Op::LoadConst => {
                    matches!(b.consts.get(i.bx() as usize), Some(Const::Number(_)))
                }
                Op::GetUpval => callee
                    .upvals
                    .get(i.b as usize)
                    .is_some(),
                _ => false,
            }
    })
}

/// Splice a tiny callee's body at a direct-call site. Callee reg r maps to
/// caller vreg window+r (the call window — clobbering it is already the
/// Call contract). Every arithmetic operand is number-guarded to `generic`
/// (the ordinary call sequence); args are immutable so the fallback sees
/// them intact.
#[allow(clippy::too_many_arguments)]
fn emit_inline_call(
    c: &mut C,
    ins: Instr,
    proto_word: u64,
    callee: &FunctionProto,
    o: &HeapOffsets,
    fmod_addr: usize,
    generic: Label,
    done: Label,
) {
    let w = ins.a + 1; // window base vreg
    let map = |r: u8| w + r;
    // stack must already cover the callee temps (high-water usually does)
    c.a.ldr_imm(9, R_REALM, o.realm_stack_len);
    c.a.lsr_imm(10, R_BASE, 3);
    c.a.add_imm(10, 10, w as u32 + callee.body().n_regs as u32);
    c.a.cmp_reg(10, 9);
    c.a.b_cond(Cond::Hi, generic);
    // callee identity: closure tag + stored proto word
    c.fetch_x(ins.a, 8);
    c.a.lsr_imm(10, 8, 48);
    c.a.movz(11, 0xFFFD, 0); // TAG_CLOSURE
    c.a.cmp_reg(10, 11);
    c.a.b_cond(Cond::Ne, generic);
    c.a.orr_reg32(12, 31, 8);
    c.a.ldr_imm(10, R_REALM, o.realm_closures_ptr);
    c.index_addr(10, 10, 12, o.closure_size);
    c.a.ldr_imm(11, 10, o.closure_proto_off);
    c.a.mov_imm64(12, proto_word);
    c.a.cmp_reg(11, 12);
    c.a.b_cond(Cond::Ne, generic);
    // x14 = closure arena address (kept for GetUpval; arith avoids x14
    // except Mod, whose int path uses 10-14 — reload there if needed)
    c.a.mov(14, 10);

    let guard = |c: &mut C, v: u8, g: Label| {
        c.fetch_x(v, 8);
        c.a.lsr_imm(10, 8, 48);
        c.a.cmp_reg(10, R_TAGLIM);
        c.a.b_cond(Cond::Hs, g);
    };
    for cins in callee.body().code.iter() {
        match cins.op {
            Op::LoadInt => c.put_bits(map(cins.a), Value::number(cins.sbx() as f64).bits()),
            Op::LoadUndef => c.put_bits(map(cins.a), Value::UNDEFINED.bits()),
            Op::LoadNull => c.put_bits(map(cins.a), Value::NULL.bits()),
            Op::LoadBool => c.put_bits(
                map(cins.a),
                if cins.b != 0 { Value::TRUE.bits() } else { Value::FALSE.bits() },
            ),
            Op::LoadConst => {
                let bits = match &callee.body().consts[cins.bx() as usize] {
                    Const::Number(n) => Value::number(*n).bits(),
                    _ => unreachable!("inlinable checked"),
                };
                c.put_bits(map(cins.a), bits);
            }
            Op::Move => {
                let src = c.fetch(map(cins.b), 0);
                c.put(map(cins.a), src);
            }
            Op::Add | Op::Sub | Op::Mul | Op::Div => {
                guard(c, map(cins.b), generic);
                guard(c, map(cins.c), generic);
                let db = c.fetch(map(cins.b), 0);
                let dc = c.fetch(map(cins.c), 1);
                let dst = if map(cins.a) < LOW { (8 + map(cins.a)) as u32 } else { 2 };
                match cins.op {
                    Op::Add => c.a.fadd(dst, db, dc),
                    Op::Sub => c.a.fsub(dst, db, dc),
                    Op::Mul => c.a.fmul(dst, db, dc),
                    _ => c.a.fdiv(dst, db, dc),
                }
                if map(cins.a) >= LOW {
                    c.a.str_d_imm(dst, R_SLOTS, C::slot(map(cins.a)));
                }
            }
            Op::Mod => {
                guard(c, map(cins.b), generic);
                guard(c, map(cins.c), generic);
                let db = c.fetch(map(cins.b), 0);
                let dc = c.fetch(map(cins.c), 1);
                emit_mod_num(c, map(cins.a), db, dc, fmod_addr);
                c.zero_cache(); // fmod path is a blr
                // x14 (closure addr) may be clobbered: recompute lazily in
                // GetUpval below — mark by zeroing
                c.a.movz(14, 0, 0);
            }
            Op::Neg => {
                guard(c, map(cins.b), generic);
                let db = c.fetch(map(cins.b), 0);
                let dst = if map(cins.a) < LOW { (8 + map(cins.a)) as u32 } else { 0 };
                c.a.fneg(dst, db);
                if map(cins.a) >= LOW {
                    c.a.str_d_imm(dst, R_SLOTS, C::slot(map(cins.a)));
                }
            }
            Op::BitAnd | Op::BitOr | Op::BitXor | Op::Shl | Op::Shr | Op::UShr
            | Op::BitNot => {
                let unary = cins.op == Op::BitNot;
                guard(c, map(cins.b), generic);
                if !unary {
                    guard(c, map(cins.c), generic);
                }
                let db = c.fetch(map(cins.b), 0);
                c.a.fcvtzs(10, db);
                if !unary {
                    let dc = c.fetch(map(cins.c), 1);
                    c.a.fcvtzs(11, dc);
                }
                let mut unsigned = false;
                match cins.op {
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
                    _ => c.a.mvn32(10, 10),
                }
                let dst = if map(cins.a) < LOW { (8 + map(cins.a)) as u32 } else { 2 };
                if unsigned {
                    c.a.ucvtf_w(dst, 10);
                } else {
                    c.a.scvtf_w(dst, 10);
                }
                if map(cins.a) >= LOW {
                    c.a.str_d_imm(dst, R_SLOTS, C::slot(map(cins.a)));
                }
            }
            Op::GetUpval => {
                // x14 holds the closure arena address unless Mod zeroed it
                let have = c.a.new_label();
                c.a.cbnz(14, have);
                c.fetch_x(ins.a, 8);
                c.a.orr_reg32(12, 31, 8);
                c.a.ldr_imm(14, R_REALM, o.realm_closures_ptr);
                c.index_addr(14, 14, 12, o.closure_size);
                c.a.bind(have);
                c.a.ldr_imm(11, 14, o.closure_upvals_ptr);
                c.a.ldr_imm(8, 11, cins.b as u32 * 8);
                match callee.upvals[cins.b as usize] {
                    tsc_ir::UpvalSrc::ParentLocalValue(_) => {}
                    _ => {
                        // cell deref (cells are old-only, non-moving)
                        c.a.orr_reg32(9, 31, 8);
                        c.a.ldr_imm(10, R_REALM, o.realm_cells_ptr);
                        c.a.ldr_reg_lsl3(8, 10, 9);
                    }
                }
                c.put_x(map(cins.a), 8);
            }
            Op::Return => {
                c.fetch_x(map(cins.a), 8);
                c.put_x(ins.a, 8);
                c.a.b(done);
                return;
            }
            _ => unreachable!("inlinable checked"),
        }
    }
    // fell off the end (no Return): result undefined
    c.put_bits(ins.a, Value::UNDEFINED.bits());
    c.a.b(done);
}

#[allow(clippy::too_many_arguments)]
fn emit_op(
    c: &mut C,
    proto: &FunctionProto,
    pc: usize,
    ins: Instr,
    facts: &Facts,
    fmod_addr: usize,
    pow_addr: usize,
    lit_shapes: &[(u64, u32)],
    inlines: &[Option<(u64, std::sync::Arc<FunctionProto>)>],
    fcache: &mut Option<u8>,
    acache: &mut Option<u8>,
) {
    let pbody = proto.body();
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
        Op::LoadConst => match pbody.consts.get(ins.bx() as usize) {
            Some(Const::Number(n)) => c.put_bits(ins.a, Value::number(*n).bits()),
            // string consts allocate — thin helper (no spill/reload)
            _ => {
                c.a.mov(0, R_REALM);
                c.a.mov(1, R_PROTO);
                c.a.mov_imm64(2, ins.bx() as u64);
                c.thin(c.helpers.load_const);
                c.put_x(ins.a, 0);
            }
        },
        Op::Move => {
            let src = c.fetch(ins.b, 0);
            c.put(ins.a, src);
        }

        Op::Add | Op::Sub | Op::Mul => {
            // integer lane: when either operand already lives in an
            // integer register, stay there. Leaving the lane would mean a
            // conversion out and another back in, and both would sit in
            // the middle of the dependency chain.
            let known_int = |c: &C, v: u8, side: usize| {
                c.lane == Some(v)
                    || c.itmp == Some(v)
                    || c.int_ok.contains(&v)
                    || facts.const_ops.get(pc).and_then(|o| o[side]).is_some()
            };
            if num_bc
                && c.lane_on
                && (c.lane == Some(ins.b)
                    || c.itmp == Some(ins.b)
                    || c.lane == Some(ins.c)
                    || c.itmp == Some(ins.c))
                && known_int(c, ins.b, 0)
                && known_int(c, ins.c, 1)
            {
                let ib = c.int_src(ins.b, 10);
                let ic = c.int_src(ins.c, 11);
                match ins.op {
                    Op::Add => c.a.add_reg(14, ib, ic),
                    Op::Sub => c.a.sub_reg(14, ib, ic),
                    Op::Mul => c.a.mul(14, ib, ic),
                    _ => unreachable!(),
                }
                c.int_dst(ins.a, 14, pc);
                return;
            }
            if num_bc {
                let db = c.fetch(ins.b, 0);
                let dc = c.fetch(ins.c, 1);
                let dst = if ins.a < LOW { (8 + ins.a) as u32 } else { 2 };
                match ins.op {
                    Op::Add => c.a.fadd(dst, db, dc),
                    Op::Sub => c.a.fsub(dst, db, dc),
                    Op::Mul => c.a.fmul(dst, db, dc),
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
                if ins.op == Op::Add {
                    // string `+` is common enough that spilling every
                    // d-register into h_step for it dominated the cost
                    c.a.mov(0, R_REALM);
                    c.a.mov(1, R_PROTO);
                    c.a.mov_imm64(2, pc as u64);
                    c.fetch_x(ins.b, 3);
                    c.fetch_x(ins.c, 4);
                    c.thin(c.helpers.add_slow);
                    c.put_x(ins.a, 0);
                } else {
                    c.step_full(pc);
                }
                c.a.bind(done);
            }
        }
        Op::Div => {
            if num_bc {
                let db = c.fetch(ins.b, 0);
                let dc = c.fetch(ins.c, 1);
                let dst = if ins.a < LOW { (8 + ins.a) as u32 } else { 2 };
                c.a.fdiv(dst, db, dc);
                if ins.a >= LOW {
                    c.a.str_d_imm(dst, R_SLOTS, C::slot(ins.a));
                }
            } else {
                c.step_full(pc);
            }
        }

        Op::Mod => {
            let const_div = facts.const_ops.get(pc).and_then(|o| o[1]);
            if num_bc {
                let db = c.fetch(ins.b, 0);
                if let Some(d) = const_div {
                    let int_in = (c.lane_on
                        && (c.lane == Some(ins.b) || c.itmp == Some(ins.b)))
                        .then(|| c.int_src(ins.b, 10));
                    emit_mod_const(c, ins.a, db, d, fmod_addr, int_in);
                    return;
                }
                let dc = c.fetch(ins.c, 1);
                emit_mod_num(c, ins.a, db, dc, fmod_addr);
            } else {
                let slow = c.a.new_label();
                let done = c.a.new_label();
                c.fetch_x(ins.b, 8);
                c.guard_number(8, slow);
                c.a.fmov_dx(0, 8);
                if let Some(d) = const_div {
                    // divisor is a compile-time integer: no fetch, no
                    // guard, no runtime integer round-trip for it
                    emit_mod_const(c, ins.a, 0, d, fmod_addr, None);
                } else {
                    c.fetch_x(ins.c, 9);
                    c.guard_number(9, slow);
                    c.a.fmov_dx(1, 9);
                    emit_mod_num(c, ins.a, 0, 1, fmod_addr);
                }
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
            c.zero_cache();
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
                c.zero_cache(); // GC may have evacuated the cached object
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
            // Fastest path: compile-time inlining of a tiny monomorphic
            // callee (from the warmup-filled CallIc) — no frame, no call.
            // Any failed guard falls through to the generic sequence.
            let inline_generic = c.a.new_label();
            let inline_done = c.a.new_label();
            let mut inlined = false;
            if let (Some(o), Some(Some((pw, callee)))) =
                (c.offsets, inlines.get(pc))
            {
                if callee.arity as usize == ins.b as usize
                    && (ins.a as usize + 1 + callee.body().n_regs as usize) < 256
                {
                    emit_inline_call(
                        c, ins, *pw, callee, &o, fmod_addr, inline_generic,
                        inline_done,
                    );
                    inlined = true;
                }
            }
            c.a.bind(inline_generic);
            // calls push frames and can GC: full spill/reload around them.
            // Fast path: per-pc direct-call IC (monomorphic compiled sync
            // callee, argc == arity) jumps straight into the callee's
            // compiled entry — no helper, no safepoint (loop back-edges
            // and the callee's own safepoints cover GC), no Arc traffic.
            c.spill_low();
            let slow = c.a.new_label();
            let done = c.a.new_label();
            if let Some(o) = c.offsets {
                // callee value from its (just spilled) slot
                c.a.ldr_imm(9, R_SLOTS, C::slot(ins.a));
                c.a.lsr_imm(10, 9, 48);
                c.a.movz(11, 0xFFFD, 0); // TAG_CLOSURE
                c.a.cmp_reg(10, 11);
                c.a.b_cond(Cond::Ne, slow);
                // per-pc IC entry
                c.a.mov_imm64(13, c.tics_base + (pc as u64) * 8);
                c.a.ldr_imm(13, 13, 0);
                c.a.cbz(13, slow);
                // proto identity: stored Arc word in the closure slot
                c.a.orr_reg32(12, 31, 9); // w12 = closure ref
                c.a.ldr_imm(14, R_REALM, o.realm_closures_ptr);
                c.index_addr(14, 14, 12, o.closure_size);
                c.a.ldr_imm(15, 14, o.closure_proto_off);
                c.a.ldr_imm(16, 13, 0); // ic.proto_word
                c.a.cmp_reg(15, 16);
                c.a.b_cond(Cond::Ne, slow);
                // argc == arity (guaranteed at fill; guard stays for
                // polymorphic sites that later re-point the closure slot)
                c.a.ldr_w_imm(16, 13, 24); // ic.arity
                c.a.cmp_imm(16, ins.b as u32);
                c.a.b_cond(Cond::Ne, slow);
                // stack capacity: base/8 + a + 1 + n_regs <= stack.len
                c.a.ldr_w_imm(17, 13, 28); // ic.n_regs
                c.a.lsr_imm(16, R_BASE, 3);
                c.a.add_reg(16, 16, 17);
                c.a.add_imm(16, 16, ins.a as u32 + 1);
                c.a.ldr_imm(17, R_REALM, o.realm_stack_len);
                c.a.cmp_reg(16, 17);
                c.a.b_cond(Cond::Hi, slow);
                // depth check
                c.a.movz(16, 10_000, 0);
                let d = c.depth(17);
                c.a.cmp_reg(d, 16);
                c.a.b_cond(Cond::Hs, slow);
                // enter the compiled callee directly
                c.a.mov(0, R_REALM);
                c.a.ldr_imm(1, 13, 8); // ic.proto_data
                c.a.add_imm(2, R_BASE, (ins.a as u32 + 1) * 8);
                c.a.mov(3, 12);
                let d = c.depth(4);
                c.a.add_imm(4, d, 1);
                c.a.movz(5, 0, 0);
                c.a.ldr_imm(16, 13, 16); // ic.code
                c.a.blr(16);
                // refresh slots base (callee frames may have grown the stack)
                c.a.ldr_imm(16, R_REALM, o.realm_stack_ptr);
                c.a.add_reg(R_SLOTS, 16, R_BASE);
                let resume = c.a.new_label();
                c.a.cbnz(0, resume);
                // success: x1 holds the return value bits
                c.a.str_imm(1, R_SLOTS, C::slot(ins.a));
                c.a.b(done);
                // deopt/error: finish through the resume helper
                c.a.bind(resume);
                c.a.mov(2, 0);
                c.a.mov(3, 1);
                c.a.mov(0, R_REALM);
                c.a.mov_imm64(13, c.tics_base + (pc as u64) * 8);
                c.a.ldr_imm(13, 13, 0);
                c.a.ldr_imm(1, 13, 8); // ic.proto_data
                c.a.add_imm(4, R_BASE, (ins.a as u32 + 1) * 8);
                c.a.ldr_imm(14, R_REALM, o.realm_closures_ptr);
                c.a.ldr_imm(5, R_SLOTS, C::slot(ins.a));
                c.a.orr_reg32(5, 31, 5); // closure ref
                let d = c.depth(6);
                c.a.add_imm(6, d, 1);
                c.thin(c.helpers.call_resume);
                c.a.str_imm(0, R_SLOTS, C::slot(ins.a));
                c.a.b(done);
            }
            c.a.bind(slow);
            c.a.mov(0, R_REALM);
            c.a.mov(1, R_PROTO);
            c.a.mov_imm64(2, pc as u64);
            c.a.mov(3, R_BASE);
            c.a.mov_imm64(4, ins.a as u64);
            c.a.mov_imm64(5, ins.b as u64);
            let d = c.depth(6);
            c.a.mov(6, d);
            c.a.mov_imm64(8, c.helpers.call as u64);
            c.a.blr(8);
            c.a.cmp_reg(0, R_SENTINEL);
            c.a.b_cond(Cond::Eq, c.bail);
            c.a.add_reg(R_SLOTS, 1, R_BASE);
            c.a.bind(done);
            c.reload_low();
            let _ = inlined;
            c.a.bind(inline_done);
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
            let full = c.a.new_label();
            if let Some(o) = c.offsets {
                if *fcache == Some(ins.b) {
                    // CSE'd: object address in x15 (validated, 0 = cold
                    // path clobbered it), its shape id in x16 — skip
                    // tag / arena / shape loads; only this pc's IC check
                    // A repeat access is bound by its dependent loads. When
                    // the site's IC is warm the shape and slot are already
                    // compile-time constants, so reading the live IC table
                    // here would add a dependent load for values we know.
                    let baked = facts
                        .ic_baked
                        .get(pc)
                        .copied()
                        .flatten()
                        .filter(|&(_, slot)| (slot as usize) < o.obj_inline_n as usize);
                    c.a.cbz(15, full);
                    if let Some((sid, slot)) = baked {
                        c.a.mov_imm64(13, sid as u64);
                        c.a.cmp_reg(16, 13);
                        c.a.b_cond(Cond::Ne, full);
                        c.a.ldr_imm(8, 15, o.obj_inline + slot * 8);
                    } else {
                        if pc < 4096 {
                            c.a.ldr_imm(14, R_ICS, (pc as u32) * 8);
                        } else {
                            c.a.mov_imm64(13, c.ics_base + (pc as u64) * 8);
                            c.a.ldr_imm(14, 13, 0);
                        }
                        c.a.lsr_imm(13, 14, 32);
                        c.a.cmp_reg(13, 16);
                        c.a.b_cond(Cond::Ne, full);
                        c.a.sub_imm32(13, 14, 1);
                        c.a.cmp_imm(13, o.obj_inline_n);
                        c.a.b_cond(Cond::Hs, full);
                        c.index_addr(17, 15, 13, 8);
                        c.a.ldr_imm(8, 17, o.obj_inline);
                    }
                    c.put_x(ins.a, 8);
                    c.a.b(done);
                }
                c.a.bind(full);
                // baked monomorphic stub: the site's warmed IC gives the
                // shape id and slot as compile-time immediates, so the
                // access is tag + shape compare + one load — no IC-table
                // read, no slot arithmetic. A miss falls into the generic
                // inline path below (which refreshes the IC).
                if let Some((sid, slot)) = facts
                    .ic_baked
                    .get(pc)
                    .copied()
                    .flatten()
                    .filter(|&(_, slot)| (slot as usize) < o.obj_inline_n as usize)
                {
                    let generic = c.a.new_label();
                    c.fetch_x(ins.b, 8);
                    c.a.lsr_imm(10, 8, 48);
                    c.a.movz(11, 0xFFFB, 0); // TAG_OBJ
                    c.a.cmp_reg(10, 11);
                    c.a.b_cond(Cond::Ne, generic);
                    c.a.orr_reg32(9, 31, 8);
                    c.arena_base(10, 9, o.obj_bases_off);
                    c.index_addr(10, 10, 9, o.obj_size);
                    c.a.ldr_imm(11, 10, o.obj_shape_arc);
                    c.a.ldr_w_imm(12, 11, o.shape_id_delta);
                    c.a.mov_imm64(13, sid as u64);
                    c.a.cmp_reg(12, 13);
                    c.a.b_cond(Cond::Ne, generic);
                    c.a.mov(15, 10); // CSE cache: validated address
                    c.a.mov(16, 12); //            + shape id
                    c.a.ldr_imm(8, 10, o.obj_inline + slot * 8);
                    c.put_x(ins.a, 8);
                    c.a.b(done);
                    c.a.bind(generic);
                }
                // inline IC'd property load (reads only, no GC)
                c.fetch_x(ins.b, 8);
                c.a.lsr_imm(10, 8, 48);
                c.a.movz(11, 0xFFFB, 0); // TAG_OBJ
                c.a.cmp_reg(10, 11);
                c.a.b_cond(Cond::Ne, slow);
                c.a.orr_reg32(9, 31, 8); // w9 = payload ref
                c.arena_base(10, 9, o.obj_bases_off);
                c.index_addr(10, 10, 9, o.obj_size);
                c.a.ldr_imm(11, 10, o.obj_shape_arc);
                c.a.ldr_w_imm(12, 11, o.shape_id_delta);
                if pc < 4096 {
                    c.a.ldr_imm(14, R_ICS, (pc as u32) * 8);
                } else {
                    c.a.mov_imm64(13, c.ics_base + (pc as u64) * 8);
                    c.a.ldr_imm(14, 13, 0);
                }
                c.a.lsr_imm(13, 14, 32);
                c.a.cmp_reg(13, 12);
                c.a.b_cond(Cond::Ne, slow);
                // populate the CSE cache: validated address + shape id
                c.a.mov(15, 10);
                c.a.mov(16, 12);
                c.a.sub_imm32(13, 14, 1);
                c.a.cmp_imm(13, o.obj_inline_n);
                c.a.b_cond(Cond::Hs, slow); // overflow slot -> helper
                c.index_addr(17, 10, 13, 8);
                c.a.ldr_imm(8, 17, o.obj_inline);
                c.put_x(ins.a, 8);
                c.a.b(done);
            } else {
                c.a.bind(full);
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
            if c.offsets.is_some() && ins.a != ins.b {
                *fcache = Some(ins.b);
            } else {
                *fcache = None;
            }
        }
        Op::SetField => {
            let slow = c.a.new_label();
            let done = c.a.new_label();
            let full = c.a.new_label();
            if let Some(o) = c.offsets {
                if *fcache == Some(ins.a) {
                    // CSE'd store: cached validated address in x15, shape
                    // id in x16 — only this pc's IC + number-value guard.
                    // As on the load side, a warm IC makes the shape and
                    // slot compile-time constants, so reading the live IC
                    // table would add a dependent load before the store
                    // address is even known. The value is guarded numeric,
                    // so the store needs no barrier either way.
                    let baked = facts
                        .ic_baked
                        .get(pc)
                        .copied()
                        .flatten()
                        .filter(|&(_, slot)| (slot as usize) < o.obj_inline_n as usize);
                    c.a.cbz(15, full);
                    c.fetch_x(ins.c, 9);
                    c.guard_number(9, full);
                    if let Some((sid, slot)) = baked {
                        c.a.mov_imm64(13, sid as u64);
                        c.a.cmp_reg(16, 13);
                        c.a.b_cond(Cond::Ne, full);
                        c.a.str_imm(9, 15, o.obj_inline + slot * 8);
                    } else {
                        if pc < 4096 {
                            c.a.ldr_imm(14, R_ICS, (pc as u32) * 8);
                        } else {
                            c.a.mov_imm64(13, c.ics_base + (pc as u64) * 8);
                            c.a.ldr_imm(14, 13, 0);
                        }
                        c.a.lsr_imm(13, 14, 32);
                        c.a.cmp_reg(13, 16);
                        c.a.b_cond(Cond::Ne, full);
                        c.a.sub_imm32(13, 14, 1);
                        c.a.cmp_imm(13, o.obj_inline_n);
                        c.a.b_cond(Cond::Hs, full);
                        c.index_addr(17, 15, 13, 8);
                        c.a.str_imm(9, 17, o.obj_inline);
                    }
                    c.a.b(done);
                }
                c.a.bind(full);
                if let Some((sid, slot)) = facts
                    .ic_baked
                    .get(pc)
                    .copied()
                    .flatten()
                    .filter(|&(_, slot)| (slot as usize) < o.obj_inline_n as usize)
                {
                    let generic = c.a.new_label();
                    c.fetch_x(ins.a, 8);
                    c.a.lsr_imm(10, 8, 48);
                    c.a.movz(11, 0xFFFB, 0);
                    c.a.cmp_reg(10, 11);
                    c.a.b_cond(Cond::Ne, generic);
                    c.fetch_x(ins.c, 9);
                    c.guard_number(9, generic); // heap values -> helper
                    c.a.orr_reg32(12, 31, 8);
                    c.arena_base(10, 12, o.obj_bases_off);
                    c.index_addr(10, 10, 12, o.obj_size);
                    c.a.ldr_imm(11, 10, o.obj_shape_arc);
                    c.a.ldr_w_imm(12, 11, o.shape_id_delta);
                    c.a.mov_imm64(13, sid as u64);
                    c.a.cmp_reg(12, 13);
                    c.a.b_cond(Cond::Ne, generic);
                    c.a.mov(15, 10);
                    c.a.mov(16, 12);
                    c.a.str_imm(9, 10, o.obj_inline + slot * 8);
                    c.a.b(done);
                    c.a.bind(generic);
                }
                // inline IC'd property store, number values only: a number
                // stored into any object never needs a write barrier, and
                // existing-slot stores never transition shapes
                c.fetch_x(ins.a, 8);
                c.a.lsr_imm(10, 8, 48);
                c.a.movz(11, 0xFFFB, 0); // TAG_OBJ
                c.a.cmp_reg(10, 11);
                c.a.b_cond(Cond::Ne, slow);
                c.fetch_x(ins.c, 9);
                c.guard_number(9, slow); // heap values -> helper (barrier)
                c.a.orr_reg32(12, 31, 8); // w12 = payload ref
                c.arena_base(10, 12, o.obj_bases_off);
                c.index_addr(10, 10, 12, o.obj_size);
                c.a.ldr_imm(11, 10, o.obj_shape_arc);
                c.a.ldr_w_imm(12, 11, o.shape_id_delta);
                if pc < 4096 {
                    c.a.ldr_imm(14, R_ICS, (pc as u32) * 8);
                } else {
                    c.a.mov_imm64(13, c.ics_base + (pc as u64) * 8);
                    c.a.ldr_imm(14, 13, 0);
                }
                c.a.lsr_imm(13, 14, 32);
                c.a.cmp_reg(13, 12);
                c.a.b_cond(Cond::Ne, slow); // empty IC or shape miss
                // populate the CSE cache: validated address + shape id
                c.a.mov(15, 10);
                c.a.mov(16, 12);
                c.a.sub_imm32(13, 14, 1); // slot (+1 encoding)
                c.a.cmp_imm(13, o.obj_inline_n);
                c.a.b_cond(Cond::Hs, slow); // overflow slot -> helper
                c.index_addr(17, 10, 13, 8);
                c.a.str_imm(9, 17, o.obj_inline);
                c.a.b(done);
            } else {
                c.a.bind(full);
            }
            c.a.bind(slow);
            c.a.mov(0, R_REALM);
            c.a.mov(1, R_PROTO);
            c.a.mov_imm64(2, pc as u64);
            c.fetch_x(ins.a, 3);
            c.a.mov_imm64(4, ins.b as u64);
            c.fetch_x(ins.c, 5);
            c.thin(c.helpers.set_field);
            c.a.bind(done);
            if c.offsets.is_some() {
                *fcache = Some(ins.a);
            }
        }
        Op::GetIndex => {
            let slow = c.a.new_label();
            let done = c.a.new_label();
            if let Some(o) = c.offsets {
                // the index guard clobbers x10, so it has to run before
                // the base pointer lands there
                c.fetch_x(ins.c, 9);
                c.guard_number(9, slow);
                let reuse = *acache == Some(ins.b) && c.int_lane;
                c.array_base(10, ins.b, o.arr_bases_off, o.arr_size, slow, reuse);
                *acache = Some(ins.b);
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
                let reuse = *acache == Some(ins.b) && c.int_lane;
                c.array_base(10, ins.b, o.arr_bases_off, o.arr_size, slow, reuse);
                *acache = Some(ins.b);
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
            let slow = c.a.new_label();
            let done = c.a.new_label();
            if let Some(o) = c.offsets {
                // Inline only for a YOUNG array. A push into spare capacity
                // allocates nothing, so the only thing the helper does that
                // matters here is `barrier_arr` — and that returns straight
                // away for young containers, which are traced wholesale. An
                // old array keeps the helper so the barrier still runs, and
                // that is what lets this store any value type rather than
                // only numbers.
                //
                // The helper also bumps `allocs_since_gc`, deliberately not
                // replicated: that counter drives the old-space collection
                // trigger, and a push that reuses existing capacity has not
                // allocated. Growth still goes through the helper and still
                // counts.
                c.fetch_x(ins.a, 8);
                c.a.lsr_imm(10, 8, 48);
                c.a.movz(11, 0xFFFC, 0); // TAG_ARR
                c.a.cmp_reg(10, 11);
                c.a.b_cond(Cond::Ne, slow);
                c.a.ubfx32(13, 8, 31, 1); // YOUNG_BIT
                c.a.cbz(13, slow); // old array -> helper (barrier)
                c.a.orr_reg32(12, 31, 8);
                c.arena_base(10, 12, o.arr_bases_off);
                c.index_addr(10, 10, 12, o.arr_size);
                c.a.ldr_imm(13, 10, o.vec_len);
                c.a.ldr_imm(14, 10, o.vec_cap);
                c.a.cmp_reg(13, 14);
                c.a.b_cond(Cond::Hs, slow); // full -> helper (realloc)
                c.a.ldr_imm(17, 10, o.vec_ptr);
                c.fetch_x(ins.b, 9);
                c.a.str_reg_lsl3(9, 17, 13);
                c.a.add_imm(13, 13, 1);
                c.a.str_imm(13, 10, o.vec_len);
                c.a.b(done);
            }
            c.a.bind(slow);
            c.a.mov(0, R_REALM);
            c.a.mov(1, R_PROTO);
            c.a.mov_imm64(2, pc as u64);
            c.fetch_x(ins.a, 3);
            c.fetch_x(ins.b, 4);
            c.thin(c.helpers.push);
            c.a.bind(done);
        }
        Op::GetGlobal => {
            c.a.mov(0, R_REALM);
            c.a.mov(1, R_PROTO);
            c.a.mov_imm64(2, pc as u64);
            c.a.mov_imm64(3, ins.bx() as u64);
            c.thin(c.helpers.get_global);
            c.put_x(ins.a, 0);
        }

        // closure-cell ops: inline loads when the capture kind is known
        // statically from this proto's upval descriptors
        Op::GetUpval => {
            let src = proto.upvals.get(ins.b as usize).copied();
            let kind = match src {
                Some(tsc_ir::UpvalSrc::ParentLocalValue(_)) => Some(false), // plain value
                Some(tsc_ir::UpvalSrc::ParentLocal(_)) => Some(true),       // cell
                _ => None, // ParentUpval: kind unknown -> helper
            };
            match (c.offsets, kind) {
                (Some(o), Some(is_cell)) => {
                    c.a.ldr_imm(10, R_REALM, o.realm_closures_ptr);
                    let cl = c.closure(9);
                    c.index_addr(10, 10, cl, o.closure_size);
                    c.a.ldr_imm(11, 10, o.closure_upvals_ptr);
                    c.a.ldr_imm(8, 11, ins.b as u32 * 8);
                    if is_cell {
                        c.a.orr_reg32(9, 31, 8); // w9 = cell ref payload
                        c.a.ldr_imm(10, R_REALM, o.realm_cells_ptr);
                        c.a.ldr_reg_lsl3(8, 10, 9);
                    }
                    c.put_x(ins.a, 8);
                }
                _ => {
                    c.a.mov(0, R_REALM);
                    let cl = c.closure(1);
                    c.a.mov(1, cl);
                    c.a.mov_imm64(2, ins.b as u64);
                    c.thin(c.helpers.get_upval);
                    c.put_x(ins.a, 0);
                }
            }
        }
        Op::SetUpval => {
            c.a.mov(0, R_REALM);
            let cl = c.closure(1);
            c.a.mov(1, cl);
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
        Op::NewObjectLit | Op::NewArrayLit => {
            let n = if ins.op == Op::NewArrayLit {
                ins.c as usize
            } else {
                match &pbody.consts[ins.c as usize] {
                    Const::Keys(k) => k.len(),
                    _ => unreachable!("NewObjectLit const is Keys"),
                }
            };
            let bump = ins.op == Op::NewObjectLit
                && c.offsets.filter(|o| o.bump_alloc).is_some()
                && lit_shapes
                    .get(pc)
                    .is_some_and(|&(sp, sn)| sp != 0 && (sn as usize) <= 3);
            if !bump {
                // helper reads values from slots — flush the ones living in
                // d-registers (allocation never GCs, no reload needed)
                for v in ins.b..ins.b + n as u8 {
                    if v < LOW {
                        c.a.str_d_imm((8 + v) as u32, R_SLOTS, C::slot(v));
                    }
                }
            }
            if ins.op == Op::NewArrayLit {
                c.a.mov(0, R_REALM);
                c.a.mov(1, R_BASE);
                c.a.mov_imm64(2, ins.b as u64);
                c.a.mov_imm64(3, n as u64);
                c.thin(c.helpers.new_array_lit);
            } else if let Some(&(shape_ptr, sn)) = lit_shapes.get(pc).filter(|&&(sp, _)| sp != 0) {
                if bump {
                    // inline nursery bump: len<cap -> write the 64-byte Obj
                    // at the tail, bump len+bytes, result = len|YOUNG_BIT.
                    // Values read straight from d-reg homes (no slot flush).
                    let o = c.offsets.unwrap();
                    let slow = c.a.new_label();
                    let done = c.a.new_label();
                    c.a.ldr_imm(10, R_REALM, o.nursery_objs_len);
                    c.a.ldr_imm(11, R_REALM, o.nursery_objs_cap);
                    c.a.cmp_reg(10, 11);
                    c.a.b_cond(Cond::Hs, slow); // full -> helper (realloc)
                    c.a.ldr_imm(12, R_REALM, o.nursery_objs_ptr);
                    c.a.add_reg_lsl(13, 12, 10, 6); // + len * 64
                    c.a.mov_imm64(14, shape_ptr);
                    c.a.str_imm(14, 13, 0); // shape
                    // vlen (u32 + padding: 64-bit store covers both)
                    c.a.mov_imm64(14, sn as u64);
                    c.a.str_imm(14, 13, o.obj_vlen);
                    for i in 0..sn as u8 {
                        c.fetch_x(ins.b + i, 9);
                        c.a.str_imm(9, 13, o.obj_inline + i as u32 * 8);
                    }
                    if (sn as usize) < 3 {
                        c.a.mov_imm64(9, Value::UNDEFINED.bits());
                        for i in sn as u8..3 {
                            c.a.str_imm(9, 13, o.obj_inline + i as u32 * 8);
                        }
                    }
                    for (i, &w) in o.empty_vec_words.iter().enumerate() {
                        c.a.mov_imm64(9, w);
                        c.a.str_imm(9, 13, o.obj_overflow + i as u32 * 8);
                    }
                    c.a.add_imm(14, 10, 1);
                    c.a.str_imm(14, R_REALM, o.nursery_objs_len);
                    c.a.ldr_imm(14, R_REALM, o.nursery_bytes_off);
                    c.a.add_imm(14, 14, 72);
                    c.a.str_imm(14, R_REALM, o.nursery_bytes_off);
                    // result: TAG_OBJ | YOUNG_BIT | index
                    c.a.mov_imm64(9, (0xFFFBu64 << 48) | 0x8000_0000);
                    c.a.orr_reg(9, 9, 10);
                    c.put_x(ins.a, 9);
                    c.a.b(done);
                    c.a.bind(slow);
                    // helper reads values from slots — flush d-reg homes
                    for v in ins.b..ins.b + sn as u8 {
                        if v < LOW {
                            c.a.str_d_imm((8 + v) as u32, R_SLOTS, C::slot(v));
                        }
                    }
                    c.a.mov(0, R_REALM);
                    c.a.mov(1, R_BASE);
                    c.a.mov_imm64(2, ins.b as u64);
                    c.a.mov_imm64(3, sn as u64);
                    c.a.mov_imm64(4, shape_ptr);
                    c.thin(c.helpers.new_object_lit2);
                    c.put_x(ins.a, 0);
                    c.a.bind(done);
                } else {
                    // shape baked at compile time: no per-alloc cache lookup
                    c.a.mov(0, R_REALM);
                    c.a.mov(1, R_BASE);
                    c.a.mov_imm64(2, ins.b as u64);
                    c.a.mov_imm64(3, sn as u64);
                    c.a.mov_imm64(4, shape_ptr);
                    c.thin(c.helpers.new_object_lit2);
                }
            } else {
                c.a.mov(0, R_REALM);
                c.a.mov(1, R_PROTO);
                c.a.mov_imm64(2, pc as u64);
                c.a.mov(3, R_BASE);
                c.a.mov_imm64(4, ins.b as u64);
                c.a.mov_imm64(5, ins.c as u64);
                c.thin(c.helpers.new_object_lit);
            }
            if !bump {
                c.put_x(ins.a, 0);
            }
        }

        Op::Closure => {
            // flush the d-reg homes the child's upval sources read from
            // slots (helper never GCs — no reload needed)
            let child = &pbody.protos[ins.bx() as usize];
            for u in &child.upvals {
                let src = match *u {
                    tsc_ir::UpvalSrc::ParentLocal(reg)
                    | tsc_ir::UpvalSrc::ParentLocalValue(reg) => reg,
                    tsc_ir::UpvalSrc::ParentUpval(_) => continue,
                };
                if src < LOW {
                    c.a.str_d_imm((8 + src) as u32, R_SLOTS, C::slot(src));
                }
            }
            c.a.mov(0, R_REALM);
            c.a.mov(1, R_PROTO);
            c.a.mov_imm64(2, pc as u64);
            c.a.mov(3, R_BASE);
            let cl = c.closure(4);
            c.a.mov(4, cl);
            c.thin(c.helpers.new_closure);
            c.put_x(ins.a, 0);
        }
        Op::Concat => {
            c.a.mov(0, R_REALM);
            c.fetch_x(ins.b, 1);
            c.fetch_x(ins.c, 2);
            c.thin(c.helpers.concat);
            c.put_x(ins.a, 0);
        }

        Op::Await => {
            // slots must be current: a pending await suspends the frame and
            // the coroutine snapshot is taken from the slots
            c.spill_low();
            c.a.mov(0, R_REALM);
            c.fetch_x(ins.a, 1);
            c.a.mov_imm64(2, ins.a as u64);
            c.a.mov_imm64(3, (pc + 1) as u64);
            c.a.mov_imm64(8, c.helpers.await_ as u64);
            c.a.blr(8);
            c.a.cmp_reg(0, R_SENTINEL);
            c.a.b_cond(Cond::Eq, c.bail);
            c.a.add_reg(R_SLOTS, 1, R_BASE);
            c.a.mov_imm64(9, JIT_AWAIT_SENTINEL);
            c.a.cmp_reg(0, 9);
            c.a.b_cond(Cond::Eq, c.await_exit);
            c.reload_low(); // restore homes first, then land the result
            c.put_x(ins.a, 0);
        }
        // closures, cells, upvals, Concat, TypeOf, Not, NewObject,
        // NewArray, string consts: exact interpreter semantics.
        // ponytail: NewObject/NewArray via step costs spill+reload per
        // alloc; inline bump-nursery alloc is the upgrade path.
        _ => c.step_full(pc),
    }
}

#[cfg(test)]
mod magic_div_tests {
    use super::magic_div;

    /// The quotient exactly as the emitted code computes it, so the test
    /// checks the constants against the sequence that will run.
    fn magic_quot(n: i64, d: i64, m: i64, sh: u32) -> i64 {
        let mut q = ((n as i128 * m as i128) >> 64) as i64;
        if d > 0 && m < 0 {
            q = q.wrapping_add(n);
        } else if d < 0 && m > 0 {
            q = q.wrapping_sub(n);
        }
        if sh > 0 {
            q >>= sh;
        }
        q.wrapping_add(((q as u64) >> 63) as i64)
    }

    #[test]
    fn magic_matches_division_over_the_i32_range() {
        let divisors = [
            2i64, 3, 7, 10, 13, 50, 97, 100, 128, 997, 1000, 1024, 100000,
            1000003, 1000000007, -7, -100, -1024, -1000000007,
        ];
        for d in divisors {
            let (m, sh) = magic_div(d).unwrap_or_else(|| panic!("no magic for {d}"));
            for n in [
                0i64, 1, -1, 2, -2, 12345, -12345, 999999, -999999,
                i32::MAX as i64, i32::MIN as i64, 1 << 40, -(1 << 40),
                d - 1, d, d + 1, d * 3 + 1, -(d * 3 + 1),
            ] {
                assert_eq!(magic_quot(n, d, m, sh), n / d, "{n} / {d}");
                let r = n - magic_quot(n, d, m, sh) * d;
                assert_eq!(r, n % d, "{n} % {d}");
            }
        }
    }

    /// Only genuinely undefined divisors have no magic number.
    #[test]
    fn only_degenerate_divisors_are_rejected() {
        assert!(magic_div(0).is_none());
        assert!(magic_div(1).is_none());
        assert!(magic_div(-1).is_none());
        assert!(magic_div(i64::MIN).is_none());
        for d in [2i64, 100, 128, 1024, 1000003, 1000000007] {
            assert!(magic_div(d).is_some(), "{d} should have a magic number");
        }
    }
}
