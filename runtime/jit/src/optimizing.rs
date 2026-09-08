//! The optimizing compiler: compiles EVERY non-async function. Type facts from
//! tsc-types pick lanes per site, they never reject:
//!   - proven-Num ops   -> unboxed FP on d-registers, no guards
//!   - unproven arith   -> tag-guarded inline fast path, h_step fallback
//!   - object/array read-> baseline-style inline IC template (helper fallback)
//!   - mutations        -> thin helpers (args by value, no slot traffic)
//!   - everything else  -> h_step with full spill/reload around it
//!
//! Register plan: vreg i < 8 lives in d(8+i) holding raw Value bits (a
//! number's Value bits ARE its f64 bits); vreg >= 8 stays in its stack
//! slot. GP plan matches baseline (x19 realm, x20 base, x22 closure,
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
use crate::baseline::{Helpers, HeapOffsets};
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
/// Realm.const_cache entries (mirrors tsr_realm::CONST_CACHE; 16-byte (key, Value) pairs).
const CONST_CACHE: usize = 512;
const CLOSURE_SLOT: u32 = 24;
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
const DEPTH_SLOT: u32 = 16;
/// `proto.jit.code` from a `*const FunctionProto`
const JIT_CODE_OFF: u32 =
    (std::mem::offset_of!(FunctionProto, jit) + std::mem::offset_of!(tsc_ir::JitState, code)) as u32;
/// proto pointer's frame slot in int_lane mode (x26 is a lane there)
const PROTO_SLOT: u32 = 0;
/// integer lane registers, in assignment order: x23 (depth's), x26
/// (proto's) and x27 (sentinel's), all stashed or rematerialized in
/// int_lane mode
const LANE_REGS: [u32; 3] = [23, 26, 27];
/// Holds one in-flight integer intermediate — an expression like
/// `(s * 31 + i) % M` only ever has one live at a time.
///
/// Shares x22 with the array-base cache, which is what makes it usable:
/// a caller-saved scratch register does not survive the ops that sit
/// between the halves of an expression (a `LoadConst` between the add and
/// the mod was enough to lose it), and then the lane pays to keep the
/// intermediate in sync without ever collecting the saving. A function
/// takes whichever of the two it can use, never both.
const R_ITMP: u32 = 21;
const R_SLOTS: u32 = 25;
const R_SENTINEL: u32 = 27;
const R_TAGLIM: u32 = 28;
/// Element kinds a GetIndex site can be specialized to. Mirrors
/// `tsc_types::EK_*` (this crate does not depend on the analysis, the
/// same way field representations arrive as plain bytes).
const EK_I32: u8 = 0;
#[allow(dead_code)]
const EK_NUM: u8 = 1;
const EK_ANY: u8 = 2;
const R_STARTPC: u32 = 15; // stashed x5, still live at the entry guards
// x21 was the IC table base. Every read of it sits on a generic fallback
// path that a warmed, baked IC never takes, so materialising the address
// as an immediate there costs nothing and buys the third callee-saved
// register — which is what lets the integer lane and the array-base
// cache stop competing.
const SAFEPOINT_INTERVAL: u16 = 1024;
/// vregs 0..LOW live in d(8+i).
const LOW: u8 = 8;

/// JumpIf condition fact (mirrors tsc_types::CondFact; tsr-jit stays
/// decoupled from the analysis crate).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum JCond {
    Num,
    Int,
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
    /// integer speculation is on: `IntV op IntV` may deopt on overflow.
    pub int_static: bool,
    /// vregs whose arithmetic already overflowed i32 at run time: their
    /// ops convert to doubles once instead of dispatching on the tag
    pub no_int: u64,
    /// per-arg representation the guard pins: 0 any number, 1 int
    /// (integral double converts in place), 2 double (int converts)
    pub arg_repr: &'a [u8],
    /// per header: vregs pinned double there; the fall-in edge and OSR
    /// entry convert an int-tagged value (non-number deopts)
    pub dbl_hdrs: &'a [(usize, Vec<u8>)],
    /// per header: vregs typed double there; converted at OSR entry
    /// only (the fall-in edge is already typed by the same analysis)
    pub dbl_osr: &'a [(usize, Vec<u8>)],
    /// per-pc (b, c): operand is a known integer constant
    pub const_ops: &'a [[Option<i64>; 2]],
    /// per-pc: warmed GetField/SetField IC contents (shape id, slot) —
    /// baked into the code as immediates, guarded by a shape compare
    pub ic_baked: &'a [Option<(u32, u32)>],
    /// loop-header speculation: (header pc, vregs) — number-guard on the
    /// fall-in edge and at OSR entry; deopt resumes at the header
    pub loop_spec: &'a [(usize, Vec<u8>)],
    /// per header: vregs typed integer there; an OSR entry normalizes
    /// them to int-tagged homes (empty when small ints are off)
    pub int_hdrs: &'a [(usize, Vec<u8>)],
    /// Loop-carried vregs worth holding unboxed in an integer register.
    /// Non-empty means this function pays for the extra frame slot that
    /// frees x23; empty means its frame is untouched.
    pub int_spec: &'a [(usize, Vec<u8>)],
    /// per-pc field representation at warm GetField/SetField sites
    /// (tsc_types::REPR_*): a typed read deopts on a shape miss instead
    /// of taking the generic path, since what follows relies on it
    pub field_repr: &'a [u8],
    /// per-pc GetIndex element kind (tsc_types::EK_*): the site guards
    /// it and the analysis types the loaded element from it
    pub elem_kind: &'a [u8],
    /// per-pc, per-operand: proven i32 integer
    pub int_facts: &'a [[bool; 3]],
    /// per-pc, per-operand: a double for certain (never int-tagged); the
    /// unguarded FP lanes need this, not `num`
    pub dbl: &'a [[bool; 3]],
    /// per NewObjectLit pc: value classes (0 int, 1 num, 2 unknown)
    pub lit_vals: &'a [Vec<u8>],
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
    /// Small ints are live: int-tagged values exist and compiled code
    /// produces them.
    smi: bool,
    ics_base: u64,
    tics_base: u64,
    n_low: u8,
    /// This function traded x23 for a frame slot to get an integer lane.
    int_lane: bool,
    /// vregs held as raw integers in lane registers for the current
    /// loop nest: (vreg, register).
    lanes: Vec<(u8, u32)>,
    /// vreg whose integer value is currently in R_ITMP (block-local).
    itmp: Option<u8>,
    /// R_ITMP holds `itmp`'s value but its home does not: the write is
    /// deferred until the vreg is still live where the register is
    /// reassigned, cleared, spilled or branched away from
    itmp_dirty: bool,
    /// the bytecode (for liveness at those points)
    code: Vec<Instr>,
    /// x22 is the array-base cache in this function.
    acache_on: bool,
    /// x22 is the integer lane's intermediate in this function.
    lane_on: bool,
    /// currently emitting inside a loop that took the lane, so x22 holds
    /// the integer intermediate and must not be blanked as a stale cache
    /// vregs the current loop header proved to hold an i32. Converting one
    /// needs no verification; anything outside this set does, because a
    /// bare `fcvtzs` would silently truncate a fraction.
    int_ok: Vec<u8>,
    /// Out-of-line code emitted after the epilogue: the rarely-taken
    /// side of a fast-path branch (a deopt, a product's -0 check), so
    /// the fast path never takes a branch.
    stubs: Vec<Stub>,
    /// per pc: the lanes live there (for exit syncs)
    lanes_at: Vec<Vec<(u8, u32)>>,
    /// integer speculation on: an int `%` yields an int or deopts (-0,
    /// x % 0), which is what lets the analysis type it IntV
    int_static: bool,
    /// `Skip; Jump` fusion: the Jump at pc+1 is a forward jump nobody
    /// else targets, so the skip branches on the inverted condition to
    /// its target and the Jump itself is not emitted.
    fuse: Option<Label>,
    skip_next: bool,
}

enum Stub {
    /// deopt to `pc` (homes intact; `lanes` as at the branch site)
    Deopt { l: Label, pc: usize, lanes: Vec<(u8, u32)>, itmp: Option<(u8, bool)> },
    /// a branch leaving a laned loop: write the lanes it drops to their
    /// homes, then go
    Exit { l: Label, sync: Vec<(u8, u32)>, target: Label },
    /// a zero product owes -0 when either factor is negative: deopt
    /// then, else back to `ok`
    MulZero { l: Label, ib: u32, ic: u32, ok: Label, deopt: Label },
}

impl C {
    fn slot(i: u8) -> u32 {
        i as u32 * 8
    }

    /// R_ITMP's vreg, when its home is stale and the vreg is read again
    /// from `pc` on (before being rewritten): the sync it needs.
    fn itmp_live_at(&self, pc: usize) -> Option<u8> {
        match self.itmp {
            Some(v) if self.itmp_dirty && live_at(&self.code, pc, v) => Some(v),
            _ => None,
        }
    }
    /// Write R_ITMP's value to its vreg's home (it was deferred).
    fn sync_itmp(&mut self, v: u8) {
        self.sync_lane(v, R_ITMP);
        self.itmp_dirty = false;
    }
    /// R_ITMP is about to be reassigned (to `new`, or freed): give the
    /// old vreg its home back if anything from `pc` on still reads it.
    fn retire_itmp(&mut self, pc: usize, new: Option<u8>) {
        if let Some(old) = self.itmp {
            if new != Some(old) {
                if let Some(v) = self.itmp_live_at(pc) {
                    self.sync_itmp(v);
                }
            }
        }
        self.itmp_dirty = false;
    }
    /// A register already holding vreg `v` as an integer: its lane, or
    /// the intermediate.
    fn tmp_reg(&self, v: u8) -> Option<u32> {
        self.lane_of(v).or_else(|| (self.itmp == Some(v)).then_some(R_ITMP))
    }
    /// Drop the intermediate at a point control may leave the block
    /// (a merge, a loop end): its home first, if still needed.
    fn clear_itmp(&mut self, pc: usize) {
        self.retire_itmp(pc, None);
        self.itmp = None;
    }

    /// Operand vreg into an FP register (its home dreg, or `scratch`).
    /// A laned vreg's home is stale: its register is boxed instead.
    fn fetch(&mut self, v: u8, scratch: u32) -> u32 {
        if self.itmp == Some(v) {
            self.box_lane(9, R_ITMP);
            self.a.fmov_dx(scratch, 9);
            return scratch;
        }
        if let Some(r) = self.lane_of(v) {
            if self.smi {
                self.box_lane(9, r);
                self.a.fmov_dx(scratch, 9);
            } else {
                self.a.scvtf(scratch, r);
            }
            return scratch;
        }
        if v < LOW {
            (8 + v) as u32
        } else {
            self.a.ldr_d_imm(scratch, R_SLOTS, Self::slot(v));
            scratch
        }
    }

    /// x{x} = the boxed Value of the integer in lane register x{r}.
    fn box_lane(&mut self, x: u32, r: u32) {
        if self.smi {
            self.a.orr_reg32(x, 31, r);
            self.a.movk(x, tsr_memory::TAG_INT as u16, 48);
        } else {
            self.a.scvtf(0, r);
            self.a.fmov_xd(x, 0);
        }
    }
    /// Write lane register x{r}'s value to vreg `v`'s home (d-reg or
    /// slot) — the sync a laned vreg pays only at loop exits and spills.
    fn sync_lane(&mut self, v: u8, r: u32) {
        if self.smi {
            self.box_lane(9, r);
            if v < LOW {
                self.a.fmov_dx((8 + v) as u32, 9);
            } else {
                self.a.str_imm(9, R_SLOTS, Self::slot(v));
            }
        } else if v < LOW {
            self.a.scvtf((8 + v) as u32, r);
        } else {
            self.a.scvtf(0, r);
            self.a.str_d_imm(0, R_SLOTS, Self::slot(v));
        }
    }
    /// vreg `v`'s current Value into its slot for a helper that reads
    /// slots (a laned vreg from its register, a low vreg from its home).
    fn spill_vreg(&mut self, v: u8) {
        if self.itmp == Some(v) && self.itmp_dirty {
            self.box_lane(9, R_ITMP);
            self.a.str_imm(9, R_SLOTS, Self::slot(v));
            return;
        }
        if let Some(r) = self.lane_of(v) {
            self.box_lane(9, r);
            self.a.str_imm(9, R_SLOTS, Self::slot(v));
        } else if v < LOW {
            self.a.str_d_imm((8 + v) as u32, R_SLOTS, Self::slot(v));
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

    /// The lane register holding vreg `v` as a raw integer, if any.
    fn lane_of(&self, v: u8) -> Option<u32> {
        self.lanes.iter().find(|(lv, _)| *lv == v).map(|(_, r)| *r)
    }
    /// x{x} = the proto pointer (x26 in plain mode, a frame slot when
    /// the lanes own it).
    fn proto_into(&mut self, x: u32) {
        if self.int_lane {
            self.a.ldr_imm(x, SP, PROTO_SLOT);
        } else {
            self.a.mov(x, R_PROTO);
        }
    }
    /// Flags = x{x} vs the error sentinel (x27 in plain mode; an
    /// immediate when the lanes own it — x9 is dead after any blr).
    fn cmp_sentinel(&mut self, x: u32) {
        if self.int_lane {
            self.a.mov_imm64(9, JIT_ERR_SENTINEL);
            self.a.cmp_reg(x, 9);
        } else {
            self.a.cmp_reg(x, R_SENTINEL);
        }
    }

    /// vreg bits into a GP register (a laned vreg from its register).
    fn fetch_x(&mut self, v: u8, x: u32) {
        if self.itmp == Some(v) {
            self.box_lane(x, R_ITMP);
            return;
        }
        if let Some(r) = self.lane_of(v) {
            self.box_lane(x, r);
            return;
        }
        self.home_x(v, x);
    }
    /// vreg `v`'s home bits into a GP register, whatever the lanes say
    /// (for an arm that just wrote the home and must re-derive the lane).
    fn home_x(&mut self, v: u8, x: u32) {
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

    /// Every home to its slot, laned vregs from their registers (x9).
    fn spill_low(&mut self) {
        for i in 0..self.n_low {
            self.a.str_d_imm((8 + i) as u32, R_SLOTS, Self::slot(i));
        }
        for (v, r) in self.lanes.clone() {
            self.box_lane(9, r);
            self.a.str_imm(9, R_SLOTS, Self::slot(v));
        }
        if let Some(v) = self.itmp {
            if self.itmp_dirty {
                self.box_lane(9, R_ITMP);
                self.a.str_imm(9, R_SLOTS, Self::slot(v));
            }
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

    /// x10 = tag of x{src}; branch to `slow` when neither double nor int.
    fn guard_numeric(&mut self, src: u32, slow: Label) {
        self.a.lsr_imm(10, src, 48);
        self.a.cmp_reg(10, R_TAGLIM);
        self.a.b_cond(Cond::Hi, slow);
    }
    /// x10 = tag of x{src}; branch to `slow` when not a double.
    fn guard_number(&mut self, src: u32, slow: Label) {
        self.a.lsr_imm(10, src, 48);
        self.a.cmp_reg(10, R_TAGLIM);
        self.a.b_cond(Cond::Hs, slow);
    }


    /// Box the i32 in w{x} as an int-tagged Value in x{x} (2 instrs).
    fn box_int(&mut self, x: u32) {
        self.a.orr_reg32(x, 31, x); // zero-extend: mov wX, wX
        self.a.movk(x, tsr_memory::TAG_INT as u16, 48);
    }
    /// x{x} = the sign-extended i32 held by the int-tagged value in x{x}.
    fn unbox_int(&mut self, x: u32) {
        self.a.sxtw(x, x);
    }
    /// d{d} = the numeric value in x{x} as a double, whichever
    /// representation it has. Clobbers x17 (the operand pair x8/x9 stays
    /// intact, so two of these can run back to back).
    fn any_to_dbl(&mut self, x: u32, d: u32) {
        let dbl = self.a.new_label();
        let done = self.a.new_label();
        self.a.lsr_imm(17, x, 48);
        self.a.cmp_reg(17, R_TAGLIM);
        self.a.b_cond(Cond::Ne, dbl);
        self.a.sxtw(x, x);
        self.a.scvtf(d, x);
        self.a.b(done);
        self.a.bind(dbl);
        self.a.fmov_dx(d, x);
        self.a.bind(done);
    }

    /// x{dst} = the cell address in the boxed value x{src} (low 48 bits).
    fn unbox(&mut self, dst: u32, src: u32) {
        self.a.ubfx64(dst, src, 0, 48);
    }
    /// Branch to `young` when the cell at x{addr} is in the nursery.
    /// Clobbers x{scratch}.
    fn young_test(&mut self, addr: u32, scratch: u32, o: &HeapOffsets, young: Label) {
        self.a.ldr_imm(scratch, R_REALM, o.young_base);
        self.a.eor_reg(scratch, scratch, addr);
        self.a.lsr_imm(scratch, scratch, o.young_shift);
        self.a.cbz(scratch, young);
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
        if let Some(r) = self.lane_of(v) {
            return r;
        }
        if let Some(r) = self.tmp_reg(v) {
            return r;
        }
        if self.smi {
            // an int-tagged home: the lane guard and every int producer
            // keep int_ok homes that way
            self.fetch_x(v, scratch);
            self.unbox_int(scratch);
        } else {
            let d = self.fetch(v, 0);
            self.a.fcvtzs(scratch, d);
        }
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

        let home = self.lane_of(v).unwrap_or(R_ITMP);
        if home == R_ITMP {
            self.retire_itmp(pc + 1, Some(v));
        }
        if src != home {
            self.a.mov(home, src);
        }
        if home != R_ITMP {
            self.itmp = None;
            return; // laned: the register is the value until a sync point
        }
        self.itmp = Some(v);
        if self.smi {
            self.a.mov(14, home);
            self.box_int(14);
            self.put_x(v, 14);
        } else {
            self.a.scvtf(3, home);
            self.put(v, 3);
        }
    }

    /// `int_dst` for a result already proven to fit i32, held
    /// zero-extended in w{src}: the integer register takes it
    /// sign-extended, the home takes it int-tagged. Clobbers x{src}.
    ///
    /// Small ints on: an integer register (lane or intermediate) holds
    /// its value in the low word only — consumers use w-forms or sign-
    /// extend themselves — so the move here is free and off the chain.
    fn int_dst_w(&mut self, v: u8, src: u32, pc: usize) {
        let home = self.lane_of(v).unwrap_or(R_ITMP);
        if home == R_ITMP {
            self.retire_itmp(pc + 1, Some(v));
        }
        self.a.mov(home, src);
        if home != R_ITMP {
            self.itmp = None;
            return; // laned: the register is the value until a sync point
        }
        // the register is the value; the home is written only where
        // something will read it (liveness at the reassignment)
        self.itmp = Some(v);
        self.itmp_dirty = true;
    }

    /// After an arm wrote vreg `v`'s home without keeping its integer
    /// views: bring the lane / intermediate registers that claim `v`
    /// back in sync and (small ints on) make an int_ok home int-tagged
    /// again — or deopt past `pc` when the home no longer holds an exact
    /// i32. Clobbers x8, x9, x12, d0, d1.
    fn fix_int_write(&mut self, v: u8, pc: usize) {
        let regs: Vec<u32> =
            self.lane_of(v).into_iter().chain((self.itmp == Some(v)).then_some(R_ITMP)).collect();
        let int_home = self.smi && self.int_ok.contains(&v);
        if regs.is_empty() && !int_home {
            return;
        }
        let r = regs.first().copied().unwrap_or(9);
        let done = self.a.new_label();
        let bad = self.a.new_label();
        self.home_x(v, 8);
        if self.smi {
            let is_int = self.a.new_label();
            self.a.lsr_imm(9, 8, 48);
            self.a.cmp_reg(9, R_TAGLIM);
            self.a.b_cond(Cond::Eq, is_int);
            self.a.b_cond(Cond::Hi, bad);
            self.a.fmov_dx(0, 8);
            self.a.fcvtzs(r, 0);
            self.a.scvtf(1, r);
            self.a.fcmp(1, 0);
            self.a.b_cond(Cond::Ne, bad);
            self.a.sxtw(12, r);
            self.a.cmp_reg(12, r);
            self.a.b_cond(Cond::Ne, bad); // wider than i32
            if int_home {
                self.a.orr_reg32(8, 31, r);
                self.a.movk(8, tsr_memory::TAG_INT as u16, 48);
                self.put_x(v, 8);
            }
            self.a.b(done);
            self.a.bind(is_int);
            self.a.mov(r, 8); // low word is the int
        } else {
            self.a.fmov_dx(0, 8);
            self.a.fcvtzs(r, 0);
            self.a.scvtf(1, r);
            self.a.fcmp(1, 0);
            self.a.b_cond(Cond::Ne, bad);
            self.a.sxtw(12, r);
            self.a.cmp_reg(12, r);
            self.a.b_cond(Cond::Ne, bad);
        }
        self.a.b(done);
        self.a.bind(bad);
        // the op has already run and its home is right: resume AFTER it
        // (resuming at pc re-applied `acc += x` — lane_overflow was 794
        // over). The home, not the (stale) lane register, is the truth
        // for this vreg at this spill.
        let saved = self.lanes.clone();
        self.lanes.retain(|(lv, _)| *lv != v);
        self.deopt_at(pc + 1);
        self.lanes = saved;
        self.a.bind(done);
        for &r2 in &regs[1..] {
            self.a.mov(r2, r);
        }
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

    /// x{dst} = the array cell address, tag-checked. When `reuse` is set
    /// the previous one is still in R_ACACHE (zeroed by any safepoint or
    /// slow path) and it collapses to a test and a move.
    fn array_base(&mut self, dst: u32, v: u8, slow: Label, reuse: bool, cache: bool) {
        let derived = self.a.new_label();
        let cached = self.a.new_label();
        if reuse {
            self.a.cbnz(R_ACACHE, cached);
        }
        self.fetch_x(v, 8);
        self.a.lsr_imm(10, 8, 48);
        self.a.movz(11, tsr_memory::TAG_ARR as u16, 0); // TAG_ARR
        self.a.cmp_reg(10, 11);
        self.a.b_cond(Cond::Ne, slow);
        self.unbox(dst, 8);
        if cache {
            self.a.mov(R_ACACHE, dst);
        }
        if reuse {
            self.a.b(derived);
            self.a.bind(cached);
            self.a.mov(dst, R_ACACHE);
            self.a.bind(derived);
        }
    }
    /// The array cell at x{arr} must have the element kind this site
    /// was specialized to; anything else deopts. `ek` is a
    /// `tsc_types::EK_*`. Clobbers x17.
    fn arr_kind_guard(&mut self, arr: u32, ek: u8, bad: Label) {
        self.a.ldr_imm(17, arr, 0);
        self.a.ubfx64(17, 17, 4, 4);
        if ek == EK_I32 {
            self.a.cmp_imm(17, tsr_memory::cells::K_ARR_I32 as u32);
            self.a.b_cond(Cond::Ne, bad);
        } else {
            // numbers: either of the numeric kinds, which sort above
            // every other cell kind
            self.a.cmp_imm(17, tsr_memory::cells::K_ARR_I32 as u32);
            self.a.b_cond(Cond::Lo, bad);
        }
    }
    /// The value in x{val} may be stored into an array whose meta word
    /// is in x{meta} without widening its element kind; otherwise
    /// branch to `wide` (the helper, which widens). Clobbers x11, x17.
    fn arr_store_ok(&mut self, meta: u32, val: u32, wide: Label) {
        // x11 and x16 only: the array arms hold the element pointer in
        // x17 and the length in x14 across this check
        let ok = self.a.new_label();
        self.a.ubfx64(11, meta, 4, 4);
        self.a.cmp_imm(11, tsr_memory::cells::K_ARR as u32);
        self.a.b_cond(Cond::Eq, ok); // already the widest kind
        self.a.lsr_imm(16, val, 48);
        self.a.cmp_reg(16, R_TAGLIM);
        self.a.b_cond(Cond::Eq, ok); // an int suits both numeric kinds
        self.a.cmp_imm(11, tsr_memory::cells::K_ARR_NUM as u32);
        self.a.b_cond(Cond::Ne, wide);
        self.a.cmp_reg(16, R_TAGLIM);
        self.a.b_cond(Cond::Hi, wide); // not a number
        self.a.bind(ok);
    }
    /// x{len} = length of the array cell at x{arr} (meta high word).
    fn arr_len(&mut self, len: u32, arr: u32) {
        self.a.ldr_imm(len, arr, 0);
        self.a.lsr_imm(len, len, 32);
    }
    /// x{vals} = address of element 0 of the array cell at x{arr}.
    fn arr_vals(&mut self, vals: u32, arr: u32, o: &HeapOffsets) {
        self.a.ldr_imm(vals, arr, o.arr_elems);
        self.a.add_imm(vals, vals, o.elems_vals);
    }

    /// May the value in `xv` be stored inline into the object in `xobj`
    /// (both boxed)? A number always. Anything else only when the field
    /// admits it (`ref_ok`) and no barrier is owed: the object is young,
    /// not yet aged, or aged and already dirty — the next minor rescans
    /// it either way. The first store into a clean aged object takes
    /// `fail`, whose helper records it. Clobbers x11, x12, x14, x17.
    fn ref_store_check(&mut self, xv: u32, xobj: u32, ref_ok: bool, o: HeapOffsets, fail: Label) {
        let ok = self.a.new_label();
        self.a.lsr_imm(11, xv, 48);
        self.a.cmp_reg(11, R_TAGLIM);
        // a number (with small ints on, the int tag counts) needs no barrier
        if !ref_ok {
            // numbers only: the plain guard, no extra branch on the fast path
            self.a.b_cond(if self.smi { Cond::Hi } else { Cond::Hs }, fail);
            return;
        }
        self.a.b_cond(if self.smi { Cond::Ls } else { Cond::Lo }, ok);
        self.unbox(12, xobj);
        self.young_test(12, 14, &o, ok);
        self.a.ldr_imm(14, 12, 0); // meta
        self.a.movz(17, (tsr_memory::cells::M_AGED | tsr_memory::cells::M_DIRTY) as u16, 0);
        self.a.and_reg(14, 14, 17);
        self.a.cmp_imm(14, tsr_memory::cells::M_AGED as u32);
        self.a.b_cond(Cond::Eq, fail); // aged and clean -> helper records it
        self.a.bind(ok);
    }

    /// x13 = a fresh cell of `words` words: nursery bump, or the old
    /// space's bump region when pretenuring. `slow` when the region is
    /// full (the helper starts a chunk / runs past the soft limit).
    /// Clobbers x9, x14. Allocation never collects.
    fn bump(&mut self, words: u32, o: &HeapOffsets, slow: Label) {
        let young = self.a.new_label();
        let done = self.a.new_label();
        self.a.ldr_imm(9, R_REALM, o.pretenure_off);
        self.a.cbz(9, young);
        self.bump_region(words, o.old_top, o.old_limit, slow);
        self.a.b(done);
        self.a.bind(young);
        self.bump_region(words, o.young_top, o.young_limit, slow);
        self.a.bind(done);
    }
    /// Old-space cell of `words` (closures, cells: never young): pop the
    /// size-class free list and log the boxed cell in the born buffer,
    /// else bump (the born range logs those). `slow` when neither has
    /// room. Leaves x13 = cell, x0 = the cell boxed with `tag`; clobbers
    /// x9, x14.
    fn bump_old(&mut self, words: u32, tag: u64, o: &HeapOffsets, slow: Label) {
        let bump = self.a.new_label();
        let done = self.a.new_label();
        self.a.ldr_imm(13, R_REALM, o.old_small + words * 8);
        self.a.cbz(13, bump);
        self.a.ldr_imm(9, R_REALM, o.born_len);
        self.a.ldr_imm(14, R_REALM, o.born_cap);
        self.a.cmp_reg(9, 14);
        self.a.b_cond(Cond::Hs, slow); // buffer full: the helper drains it
        self.a.ldr_imm(14, 13, 8); // next free
        self.a.str_imm(14, R_REALM, o.old_small + words * 8);
        self.a.mov_imm64(14, tag << 48);
        self.a.orr_reg(0, 14, 13);
        self.a.ldr_imm(14, R_REALM, o.born_ptr);
        self.a.str_reg_lsl3(0, 14, 9);
        self.a.add_imm(9, 9, 1);
        self.a.str_imm(9, R_REALM, o.born_len);
        self.a.b(done);
        self.a.bind(bump);
        self.bump_region(words, o.old_top, o.old_limit, slow);
        self.tag_addr(0, tag);
        self.a.bind(done);
    }
    fn bump_region(&mut self, words: u32, top: u32, limit: u32, slow: Label) {
        self.a.ldr_imm(13, R_REALM, top);
        self.a.ldr_imm(9, R_REALM, limit);
        self.a.add_imm(14, 13, words * 8);
        self.a.cmp_reg(14, 9);
        self.a.b_cond(Cond::Hi, slow);
        self.a.str_imm(14, R_REALM, top);
    }
    /// Box the cell address in x13 with `tag` into x{dst}.
    fn tag_addr(&mut self, dst: u32, tag: u64) {
        self.a.mov_imm64(9, tag << 48);
        self.a.orr_reg(dst, 9, 13);
    }

    /// Closure creation: an old-space bump with the proto pointer and
    /// the captured values written in place. `reg(r)` maps the child's
    /// parent-register sources to vregs (the window for an inlined
    /// callee); `clo` gives the current closure's address in a register
    /// for ParentUpval sources; the helper gets the (proto, pc, base)
    /// triple and covers a full chunk or a capture that is not a cell.
    #[allow(clippy::too_many_arguments)]
    fn emit_closure(
        &mut self,
        dst: u8,
        child: &std::sync::Arc<FunctionProto>,
        reg: &dyn Fn(u8) -> u8,
        clo: &dyn Fn(&mut C) -> u32,
        slow_proto: u64,
        slow_pc: usize,
        slow_base_off: u32,
        o: Option<HeapOffsets>,
    ) {
        let slow = self.a.new_label();
        let done = self.a.new_label();
        let n = child.upvals.len();
        if let Some(o) = o.filter(|_| n <= 8) {
            // the cell stores a raw proto pointer: keep the target alive
            tsr_memory::hold_proto(child);
            self.bump_old(2 + n as u32, tsr_memory::TAG_CLOSURE, &o, slow); // TAG_CLOSURE
            self.a.mov_imm64(14, tsr_memory::cells::meta(tsr_memory::cells::K_CLOSURE, 2 + n, n));
            self.a.str_imm(14, 13, 0);
            self.a.mov_imm64(14, std::sync::Arc::as_ptr(child) as u64);
            self.a.str_imm(14, 13, o.closure_proto);
            for (i, u) in child.upvals.iter().enumerate() {
                match *u {
                    tsc_ir::UpvalSrc::ParentLocal(r) => {
                        self.fetch_x(reg(r), 9);
                        self.a.lsr_imm(11, 9, 48);
                        self.a.movz(14, tsr_memory::TAG_CELL as u16, 0); // TAG_CELL
                        self.a.cmp_reg(11, 14);
                        self.a.b_cond(Cond::Ne, slow);
                    }
                    tsc_ir::UpvalSrc::ParentLocalValue(r) => self.fetch_x(reg(r), 9),
                    tsc_ir::UpvalSrc::ParentUpval(idx) => {
                        let cr = clo(self);
                        self.a.ldr_imm(9, cr, o.closure_upvals + idx as u32 * 8);
                    }
                }
                self.a.str_imm(9, 13, o.closure_upvals + i as u32 * 8);
            }
            self.a.b(done); // x0 = the boxed closure
        }
        self.a.bind(slow);
        // flush the d-reg homes the helper's upval sources read from slots
        for u in &child.upvals {
            let src = match *u {
                tsc_ir::UpvalSrc::ParentLocal(r) | tsc_ir::UpvalSrc::ParentLocalValue(r) => reg(r),
                tsc_ir::UpvalSrc::ParentUpval(_) => continue,
            };
            self.spill_vreg(src);
        }
        self.a.mov(0, R_REALM);
        self.a.mov_imm64(1, slow_proto);
        self.a.mov_imm64(2, slow_pc as u64);
        self.a.add_imm(3, R_BASE, slow_base_off);
        let cr = clo(self);
        self.a.mov(4, cr);
        self.a.mov_imm64(5, child as *const std::sync::Arc<FunctionProto> as u64);
        self.thin_keep_arrays(self.helpers.new_closure);
        self.a.bind(done);
        self.put_x(dst, 0);
    }

    /// Compare a shape id: an immediate while ids fit twelve bits (they
    /// are small and sequential; widening hands out fresh ones).
    fn cmp_sid(&mut self, reg: u32, sid: u32) {
        if sid < 4096 {
            self.a.cmp_imm(reg, sid);
        } else {
            self.a.mov_imm64(13, sid as u64);
            self.a.cmp_reg(reg, 13);
        }
    }

    /// Deopt to the interpreter at `pc`, which re-runs the instruction
    /// from the (still intact) register homes.
    fn deopt_at(&mut self, pc: usize) {
        self.spill_low();
        self.a.movz(0, 2, 0);
        self.a.mov_imm64(1, pc as u64);
        self.a.b(self.deopt_exit);
    }
    /// A label that deopts to `pc`, emitted out of line: branch to it
    /// on the rare condition and the fast path stays straight.
    fn deopt_stub(&mut self, pc: usize) -> Label {
        let l = self.a.new_label();
        let lanes = self.lanes.clone();
        let itmp = self.itmp.map(|v| (v, self.itmp_dirty));
        self.stubs.push(Stub::Deopt { l, pc, lanes, itmp });
        l
    }
    /// The label to branch to for bytecode `target` from `pc`: the
    /// target itself, or a stub that first syncs the lanes the branch
    /// leaves behind (the target is outside their loop).
    fn exit_label(&mut self, pc: usize, target: usize) -> Label {
        let here = self.lanes_at.get(pc).cloned().unwrap_or_default();
        let there = self.lanes_at.get(target).cloned().unwrap_or_default();
        let mut sync: Vec<(u8, u32)> = here.into_iter().filter(|l| !there.contains(l)).collect();
        if let Some(v) = self.itmp_live_at(target) {
            sync.push((v, R_ITMP));
        }
        let t = self.pc_labels[target];
        if sync.is_empty() {
            return t;
        }
        let l = self.a.new_label();
        self.stubs.push(Stub::Exit { l, sync, target: t });
        l
    }

    /// The boxed number in `xv` has an exact i32 value and is not -0 —
    /// what an Int32 field may hold. Clobbers d0, d1, x11, x12 only —
    /// the literal template holds its object address in x13.
    fn int32_check(&mut self, xv: u32, fail: Label) {
        let ok = self.a.new_label();
        if self.smi {
            // Int32 is the int tag itself: an integral double widens the
            // field (the helper does it) rather than passing as an int
            self.a.lsr_imm(11, xv, 48);
            self.a.cmp_reg(11, R_TAGLIM);
            self.a.b_cond(Cond::Ne, fail);
            return;
        }
        self.a.fmov_dx(0, xv);
        self.a.fcvtzs(11, 0);
        self.a.scvtf(1, 11);
        self.a.fcmp(1, 0);
        self.a.b_cond(Cond::Ne, fail);
        self.a.sxtw(12, 11);
        self.a.cmp_reg(12, 11);
        self.a.b_cond(Cond::Ne, fail);
        self.a.mov_imm64(12, (-0.0f64).to_bits());
        self.a.cmp_reg(xv, 12);
        self.a.b_cond(Cond::Eq, fail);
        self.a.bind(ok);
    }

    /// Array literal of `n` values from vregs `first..`: one bump holding
    /// the array cell and its element chunk back to back, the helper as
    /// the slow path. Result in `dst`.
    /// `kind` is the cell kind the literal's values give the array
    /// (`K_ARR_I32` / `K_ARR_NUM` / `K_ARR`).
    fn emit_arr_lit(&mut self, dst: u8, first: u8, n: usize, kind: u64) {
        let c = self;
        let slow = c.a.new_label();
        let done = c.a.new_label();
        if let Some(o) = c.offsets.filter(|o| o.bump_alloc && n <= 8) {
            use tsr_memory::cells::{meta, K_ELEMS};
            let cap = n.max(4);
            let words = 2 + 1 + cap;
            c.bump(words as u32, &o, slow);
            c.a.mov_imm64(14, meta(kind, 2, n));
            c.a.str_imm(14, 13, 0);
            c.a.add_imm(14, 13, 16);
            c.a.str_imm(14, 13, o.arr_elems);
            c.a.mov_imm64(14, meta(K_ELEMS, 1 + cap, cap));
            c.a.str_imm(14, 13, 16);
            for i in 0..n {
                c.fetch_x(first + i as u8, 9);
                c.a.str_imm(9, 13, 16 + o.elems_vals + i as u32 * 8);
            }
            // slack past `n` is never read
            c.tag_addr(0, tsr_memory::TAG_ARR); // TAG_ARR
            c.a.b(done);
        }
        c.a.bind(slow);
        // helper reads values from slots — flush the ones living in
        // d-registers (allocation never GCs, no reload needed)
        for v in first..first + n as u8 {
            c.spill_vreg(v);
        }
        c.a.mov(0, R_REALM);
        c.a.mov(1, R_BASE);
        c.a.mov_imm64(2, first as u64);
        c.a.mov_imm64(3, n as u64);
        c.thin(c.helpers.new_array_lit);
        c.a.bind(done);
        c.put_x(dst, 0);
    }

    /// Object literal with a baked shape: `sn` values from vregs
    /// `first..`, one bump for objects that fit the inline slots, the
    /// shape-baked helper otherwise. `checks[i]` is what value i must be
    /// shown to be before it may enter the shape's field: 0 nothing, 1
    /// Int32 (value already known a number), 2 number then Int32, 3
    /// number. A miss takes the helper, which widens the shape. Result
    /// in `dst`.
    fn emit_obj_lit(&mut self, dst: u8, first: u8, sn: u8, shape_ptr: u64, checks: &[u8]) {
        let c = self;
        let slow = c.a.new_label();
        let done = c.a.new_label();
        if let Some(o) = c.offsets.filter(|o| o.bump_alloc && sn as u32 <= o.obj_inline_n) {
            use tsr_memory::cells::{meta, K_OBJ};
            c.bump(o.obj_words, &o, slow);
            c.a.mov_imm64(14, meta(K_OBJ, o.obj_words as usize, sn as usize));
            c.a.str_imm(14, 13, 0);
            c.a.mov_imm64(14, shape_ptr);
            c.a.str_imm(14, 13, o.obj_shape);
            for i in 0..sn {
                c.fetch_x(first + i, 9);
                let cls = checks.get(i as usize).copied().unwrap_or(0);
                if cls == 2 || cls == 3 {
                    // a number: with small ints on, an int-tagged value is one
                    c.a.lsr_imm(11, 9, 48);
                    c.a.cmp_reg(11, R_TAGLIM);
                    c.a.b_cond(if c.smi { Cond::Hi } else { Cond::Hs }, slow);
                }
                if cls == 1 || cls == 2 {
                    c.int32_check(9, slow);
                }
                c.a.str_imm(9, 13, o.obj_inline + i as u32 * 8);
            }
            c.a.str_imm(31, 13, o.obj_spill); // xzr: no spill
            c.tag_addr(0, tsr_memory::TAG_OBJ); // TAG_OBJ
            c.a.b(done);
        }
        c.a.bind(slow);
        for v in first..first + sn {
            c.spill_vreg(v);
        }
        // shape baked at compile time: no per-alloc cache lookup
        c.a.mov(0, R_REALM);
        c.a.mov(1, R_BASE);
        c.a.mov_imm64(2, first as u64);
        c.a.mov_imm64(3, sn as u64);
        c.a.mov_imm64(4, shape_ptr);
        c.thin_keep_arrays(c.helpers.new_object_lit2);
        c.a.bind(done);
        c.put_x(dst, 0);
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
        self.cmp_sentinel(0);
        self.a.b_cond(Cond::Eq, self.bail);
        self.a.add_reg(R_SLOTS, 1, R_BASE);
        self.zero_cache();
    }

    /// `thin` for a helper that cannot touch the array arena (strings,
    /// objects, cells, closures, globals — anything but arrs growth).
    /// x22 is callee-saved and holds an array slot address that only an
    /// arrs realloc can invalidate, so it survives; x15 is caller-saved
    /// and must go.
    fn thin_keep_arrays(&mut self, addr: usize) {
        self.a.mov_imm64(8, addr as u64);
        self.a.blr(8);
        self.cmp_sentinel(0);
        self.a.b_cond(Cond::Eq, self.bail);
        self.a.add_reg(R_SLOTS, 1, R_BASE);
        self.a.movz(15, 0, 0);
    }

    /// Full interpreter semantics for instruction `pc` via h_step, with
    /// slot coherence: spill d-regs before (h_step reads slots), reload
    /// after (h_step writes its result to a slot). ctrl stays in x0.
    fn step_full(&mut self, pc: usize) {
        self.spill_low();
        self.a.mov(0, R_REALM);
        self.proto_into(1);
        self.a.mov_imm64(2, pc as u64);
        self.a.mov(3, R_BASE);
        let cl = self.closure(4);
        self.a.mov(4, cl);
        self.a.mov_imm64(8, self.helpers.step as u64);
        self.a.blr(8);
        self.cmp_sentinel(0);
        self.a.b_cond(Cond::Eq, self.bail);
        self.a.add_reg(R_SLOTS, 1, R_BASE);
        self.reload_low();
        self.zero_cache();
    }
}

/// Signed-integer condition for a compare op (`js_cond` gives the FP one).
/// The array kind a literal's value classes give it (`lit_vals`: 0 int,
/// 1 number, 2 anything). Unknown classes make the widest kind, which
/// admits every later store.
pub fn lit_arr_kind(classes: &[u8], n: usize) -> u64 {
    use tsr_memory::cells::{K_ARR, K_ARR_I32, K_ARR_NUM};
    if classes.len() < n {
        return K_ARR;
    }
    classes[..n].iter().fold(K_ARR_I32, |k, c| match (k, c) {
        (_, 2) => K_ARR,
        (K_ARR, _) => K_ARR,
        (_, 1) => K_ARR_NUM,
        (k, _) => k,
    })
}

fn int_cond(op: Op) -> Cond {
    match op {
        Op::Lt | Op::LtSkip => Cond::Lt,
        Op::Le | Op::LeSkip => Cond::Le,
        Op::Gt | Op::GtSkip => Cond::Gt,
        Op::Ge | Op::GeSkip => Cond::Ge,
        Op::Eq | Op::EqSkip => Cond::Eq,
        Op::Ne | Op::NeSkip => Cond::Ne,
        _ => unreachable!(),
    }
}

/// What the analysis proved about an operand's representation.
#[derive(Clone, Copy, PartialEq)]
enum Cls {
    /// an integer constant in i32 range
    IntK(i64),
    /// an int-tagged value
    Int,
    /// a double for certain
    Dbl,
    /// numeric, representation unknown
    Num,
    Other,
}

impl Cls {
    /// The class with `Other` demoted to `Num` (after a numeric guard).
    fn min_num(self) -> Cls {
        if self == Cls::Other { Cls::Num } else { self }
    }
}

fn cls_of(facts: &Facts, pc: usize, side: usize, smi: bool) -> Cls {
    if let Some(k) = facts.const_ops.get(pc).and_then(|o| o[side]) {
        if (i32::MIN as i64..=i32::MAX as i64).contains(&k) {
            return Cls::IntK(k);
        }
    }
    let f = |v: &[[bool; 3]]| v.get(pc).is_some_and(|x| x[side + 1]);
    if f(facts.dbl) {
        Cls::Dbl
    } else if smi && f(facts.int_facts) {
        Cls::Int
    } else if f(facts.num) {
        Cls::Num
    } else {
        Cls::Other
    }
}

/// Load an operand as a double into d{d} (or return its home). Only for
/// `IntK`/`Dbl`/`Int` classes; `Num` needs the dynamic path.
fn load_dbl(c: &mut C, cls: Cls, v: u8, d: u32) -> u32 {
    // scratch x12: the operand pair x8/x9 of a caller stays intact
    match cls {
        Cls::IntK(k) => {
            c.a.mov_imm64(12, (k as f64).to_bits());
            c.a.fmov_dx(d, 12);
            d
        }
        Cls::Int => {
            c.fetch_x(v, 12);
            c.a.sxtw(12, 12);
            c.a.scvtf(d, 12);
            d
        }
        _ => c.fetch(v, d),
    }
}

/// Load an operand as an i32 in the low word of x{x} (consumers use
/// the w-forms; the upper word is the tag or a sign extension). `IntK`
/// or `Int` (an int-tagged home, or the lane / intermediate register).
fn load_int(c: &mut C, cls: Cls, v: u8, x: u32) -> u32 {
    match cls {
        Cls::IntK(k) => {
            c.a.mov_imm64(x, k as u64);
            x
        }
        _ => {
            if let Some(r) = c.lane_of(v) {
                return r;
            }
            if let Some(r) = c.tmp_reg(v) {
                return r;
            }
            c.fetch_x(v, x);
            x
        }
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
/// policy (same discriminator as the baseline compiler: measured, keeps fnv-shaped
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
    inlines: &[Option<(u64, std::sync::Arc<FunctionProto>, Vec<(u64, u32, u64)>)>],
) -> Option<Vec<u32>> {
    let pbody = proto.body();
    if pbody.code.len() > crate::baseline::MAX_CODE {
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

    // Integer lanes. Loops outer-first; each takes its picks into the
    // lane registers its enclosing loops left free, so a nest keeps the
    // outer counter live through the inner loop. Per pc: the lanes and
    // int_ok vregs of every enclosing laned loop.
    let loops: Vec<(usize, usize)> = {
        let mut hs: Vec<(usize, usize)> = pbody
            .code
            .iter()
            .enumerate()
            .filter(|(_, i)| i.op == Op::Jump && i.sbx() < 0)
            .map(|(pc, i)| ((pc as i64 + i.sbx() as i64 + 1) as usize, pc))
            .collect();
        hs.sort_by_key(|&(h, e)| (std::cmp::Reverse(e - h), h));
        hs.dedup_by_key(|&mut (h, _)| h);
        hs.reverse(); // innermost first: the hot loop picks its registers first
        hs
    };
    let mut lane_assign: Vec<(usize, Vec<(u8, u32)>)> = Vec::new();
    let mut lanes_at: Vec<Vec<(u8, u32)>> = vec![Vec::new(); pbody.code.len() + 2];
    let mut int_ok_at: Vec<Vec<u8>> = vec![Vec::new(); pbody.code.len() + 2];
    for &(h, end) in &loops {
        let Some((_, vs)) = facts.int_spec.iter().find(|(hh, _)| *hh == h) else { continue };
        // registers the loops nested inside this one already hold
        let mut taken: Vec<(u8, u32)> = Vec::new();
        for pc in h..=end {
            for l in &lanes_at[pc] {
                if !taken.contains(l) {
                    taken.push(*l);
                }
            }
        }
        let free: Vec<u32> =
            LANE_REGS.iter().copied().filter(|r| !taken.iter().any(|(_, t)| t == r)).collect();
        let picks: Vec<(u8, u32)> = lane_picks(pbody, facts, h, vs)
            .into_iter()
            .filter(|v| !taken.iter().any(|(tv, _)| tv == v))
            .zip(free)
            .collect();
        if picks.is_empty() {
            continue;
        }
        for pc in h..=end {
            lanes_at[pc].extend(picks.iter().copied());
            int_ok_at[pc].extend(vs.iter().copied());
        }
        lane_assign.push((h, picks));
    }

    let n_low = pbody.n_regs.min(LOW);
    let mut c = {
        let mut a = Asm::new();
        // x22 serves one purpose per function: the array-base cache where
        // there are arrays to index, otherwise the integer lane's
        // intermediate. x23 is the lane value itself.
        // x22 serves the integer lane inside loops that have one and the
        // array-base cache everywhere else. Deciding per function instead
        // left one of them unused in any function that does both, which is
        // every realistic one.
        let acache_on = pbody.code.iter().any(|i| matches!(i.op, Op::Len | Op::GetIndex));
        let lane_on = !lane_assign.is_empty();
        let int_lane = acache_on || lane_on;
        let bail = a.new_label();
        let await_exit = a.new_label();
        let deopt_exit = a.new_label();
        let ret = a.new_label();
        let pc_labels: Vec<Label> = (0..pbody.code.len() + 2).map(|_| a.new_label()).collect();
        C { a, pc_labels, bail, await_exit, deopt_exit, ret, helpers, offsets, smi: tsr_memory::smi_on(), ics_base, tics_base, n_low, int_lane, lanes: Vec::new(), itmp: None, itmp_dirty: false, code: pbody.code.clone(), acache_on, lane_on, int_ok: Vec::new(), stubs: Vec::new(), lanes_at: Vec::new(), int_static: facts.int_static, fuse: None, skip_next: false }
    };
    let out = c.a.new_label();

    // prologue: baseline frame + only the d-pairs this fn actually uses as
    // vreg homes (small leaf functions push/pop far less)
    let d_pairs = c.n_low.div_ceil(2) as u32;
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
        c.a.stp_pre(1, 1, SP, -16); // proto: x26 is a lane register
        if c.acache_on {
            c.a.movz(R_ACACHE, 0, 0);
        }
    } else {
        c.a.mov(R_DEPTH, 4);
        c.a.mov(R_CLOSURE, 3);
        c.a.mov(R_PROTO, 1);
        c.a.mov_imm64(R_SENTINEL, JIT_ERR_SENTINEL);
    }
    c.a.mov(R_REALM, 0);
    c.a.mov(R_BASE, 2);
    c.a.movz(R_TAGLIM, 0xFFF9, 0);
    c.a.movz(R_POLL, SAFEPOINT_INTERVAL, 0);

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
        match facts.arg_repr.get(i).copied().unwrap_or(0) {
            // int-tagged, or an integral double normalized in place
            1 => emit_lane_guard(&mut c, &[i as u8], &[], deopt_entry),
            2 => emit_dbl_guard(&mut c, &[i as u8], deopt_entry),
            _ => {
                c.fetch_x(i as u8, 8);
                c.a.lsr_imm(9, 8, 48);
                c.a.cmp_reg(9, R_TAGLIM);
                c.a.b_cond(Cond::Hi, deopt_entry); // numeric: an int or a double
            }
        }
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
            let laned_here = !lanes_at[h].is_empty();
            let int_hdr = facts.int_hdrs.iter().find(|(hh, _)| *hh == h);
            let dbl_hdr = facts.dbl_hdrs.iter().find(|(hh, _)| *hh == h);
            let dbl_osr = facts.dbl_osr.iter().find(|(hh, _)| *hh == h);
            if facts.loop_spec.iter().any(|(hh, _)| *hh == h)
                || laned_here
                || int_hdr.is_some()
                || dbl_hdr.is_some()
                || dbl_osr.is_some()
            {
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
                    c.a.b_cond(Cond::Hi, fail);
                }
                // int-typed vregs first: the interpreter may hand over an
                // integral double where the body reads int bits
                if let Some((_, hv)) = int_hdr {
                    emit_lane_guard(&mut c, hv, &[], fail);
                }
                if let Some((_, dv)) = dbl_hdr {
                    emit_dbl_guard(&mut c, dv, fail);
                }
                if let Some((_, dv)) = dbl_osr {
                    emit_dbl_guard(&mut c, dv, fail);
                }
                // every enclosing laned loop's registers, outer first
                for (lh, laned) in lane_assign.iter() {
                    let lend = loops.iter().find(|(x, _)| x == lh).map_or(*lh, |(_, e)| *e);
                    if *lh <= h && h <= lend {
                        let (_, vs) = facts.int_spec.iter().find(|(hh, _)| hh == lh).unwrap();
                        emit_lane_guard(&mut c, vs, laned, fail);
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
            | Op::GeSkip => {
                // A skip's landing pc is only a merge if the instruction
                // it skips over can also fall through to it. In the loop
                // idiom the emitter produces — `LtSkip; Jump <exit>` —
                // that instruction is an unconditional jump, so the
                // landing pc has exactly one predecessor and the caches
                // survive. Marking it regardless invalidated the array
                // base immediately after `Len` had computed it, so every
                // `a[i]` in a counted loop re-derived the whole thing.
                let falls_through = !matches!(
                    pbody.code.get(pc + 1).map(|i| i.op),
                    Some(Op::Jump) | Some(Op::Return) | Some(Op::Halt) | None
                );
                if falls_through {
                    jump_targets[pc + 2] = true;
                }
            }
            _ => {}
        }
    }
    let in_lane: Vec<bool> = lanes_at.iter().map(|l| !l.is_empty()).collect();
    c.lanes_at = lanes_at.clone();
    let mut fcache: Option<u8> = None;
    // which vreg's array data pointer is in R_ACACHE (0 there = invalid)
    let mut acache: Option<u8> = None;
    // Per pc: the innermost enclosing loop's primary array — the vreg of
    // the loop's first Len/GetIndex, provided nothing in the loop rewrites
    // it. Its base lives in x22 for the whole loop: the header seeds it
    // and other arrays' accesses leave the register alone.
    let primary_at: Vec<Option<u8>> = {
        let mut v = vec![None; pbody.code.len() + 2];
        let mut hs: Vec<(usize, usize)> = pbody
            .code
            .iter()
            .enumerate()
            .filter(|(_, i)| i.op == Op::Jump && i.sbx() < 0)
            .map(|(pc, i)| ((pc as i64 + i.sbx() as i64 + 1) as usize, pc))
            .collect();
        hs.sort_by_key(|&(h, e)| std::cmp::Reverse(e - h)); // outer first
        for (h, e) in hs {
            // An inner loop inherits the enclosing loop's primary: it must
            // not write x22 with another array's base, or the outer loop
            // would read it on its back edge (mod_sign's nested loops
            // exited early on a stale length).
            let inherited = v[h];
            let prim = inherited.or_else(|| {
                pbody.code[h..=e]
                    .iter()
                    .find(|i| matches!(i.op, Op::Len | Op::GetIndex | Op::ArrayPush | Op::SetIndex))
                    .map(|i| if matches!(i.op, Op::ArrayPush | Op::SetIndex) { i.a } else { i.b })
                    .filter(|&a| !pbody.code[h..=e].iter().any(|i| writes_a_op(i.op) && i.a == a))
            });
            for p in h..=e {
                v[p] = prim;
            }
        }
        v
    };
    // loop headers: the furthest back edge targeting each
    let loop_end_of = |h: usize| -> Option<usize> {
        pbody
            .code
            .iter()
            .enumerate()
            .filter(|(p2, i)| {
                i.op == Op::Jump && i.sbx() < 0 && (*p2 as i64 + i.sbx() as i64 + 1) as usize == h
            })
            .map(|(p2, _)| p2)
            .max()
    };
    for (pc, ins) in pbody.code.iter().enumerate() {
        if jump_targets[pc] {
            fcache = None;
            // The array cache may survive a loop header when the loop's
            // every array op is on the cached vreg and nothing rewrites
            // it: then the back edge arrives with the same base, and the
            // runtime `cbnz x22` still covers helpers and safepoints
            // (which zero it). `for (i < a.length) a[i]` re-derived the
            // base on every iteration — 18 of objects' 138 instructions.
            // Seeded rather than merged: the fall-in edge zeroes x22 and
            // the loop's first array op derives the base, every later
            // iteration reuses it. Only when the loop touches exactly one
            // array vreg and never rewrites it.
            let single = loop_end_of(pc).and(primary_at[pc]);
            match single {
                Some(v) if c.acache_on => {
                    if acache != Some(v) {
                        c.a.movz(R_ACACHE, 0, 0); // fall-in: derive on first use
                    }
                    acache = Some(v);
                }
                _ => acache = None,
            }
        }
        // The lane ends with its loop. Left set, a later loop sharing the
        // vreg would read a register nothing reloaded: two consecutive
        // loops on one counter ran the second from the first's final
        // value (found the day array loops became eligible).
        // (this header's own picks join after its guard, which reads
        // their homes)
        c.lanes = lanes_at[pc].clone();
        if let Some((_, own)) = lane_assign.iter().find(|(h, _)| *h == pc) {
            c.lanes.retain(|l| !own.contains(l));
        }
        c.int_ok = int_ok_at[pc].clone();
        if !in_lane[pc] {
            c.clear_itmp(pc);
        }
        // loop-header speculation: guard the fall-in path (back-edges jump
        // to the label BELOW these guards and are already proven)
        // integer lane: at a loop header, take one int_spec vreg into a
        // raw integer register for the body. The guard runs on the fall-in
        // edge only — back edges branch to the label below it — so an
        // integer chain pays one conversion per loop entry instead of two
        // per iteration.
        if let Some((_, laned)) = lane_assign.iter().find(|(h, _)| *h == pc) {
            let (_, vs) = facts.int_spec.iter().find(|(h, _)| *h == pc).unwrap();
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
            c.clear_itmp(pc);
            c.lanes = lanes_at[pc].clone();
        }
        // double pinning: the fall-in edge converts an int (`let zr = 0`
        // into a double recurrence); back edges arrive as doubles
        if let Some((_, vs)) = facts.dbl_hdrs.iter().find(|(h, _)| *h == pc) {
            let fail = c.a.new_label();
            let pass = c.a.new_label();
            emit_dbl_guard(&mut c, vs, fail);
            c.a.b(pass);
            c.a.bind(fail);
            c.spill_low();
            c.a.movz(0, 2, 0);
            c.a.mov_imm64(1, pc as u64);
            c.a.b(c.deopt_exit);
            c.a.bind(pass);
        }
        if let Some((_, vs)) = facts.loop_spec.iter().find(|(h, _)| *h == pc) {
            let fail = c.a.new_label();
            let pass = c.a.new_label();
            for &v in vs {
                c.fetch_x(v, 8);
                c.a.lsr_imm(10, 8, 48);
                c.a.cmp_reg(10, R_TAGLIM);
                c.a.b_cond(Cond::Hi, fail);
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
        if c.skip_next {
            // the Jump a Skip just fused away
            c.skip_next = false;
            continue;
        }
        c.fuse = match pbody.code.get(pc + 1) {
            Some(j) if j.op == Op::Jump && j.sbx() >= 0 && !jump_targets[pc + 1] => {
                let target = (pc as i64 + 1 + j.sbx() as i64 + 1) as usize;
                Some(c.exit_label(pc, target))
            }
            _ => None,
        };
        emit_op(&mut c, proto, pc, *ins, facts, fmod_addr, pow_addr, lit_shapes, inlines, &mut fcache, &mut acache, in_lane[pc], primary_at[pc]);
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
        if c.itmp == Some(ins.a) && !matches!(ins.op, Op::Add | Op::Sub | Op::Mul | Op::Mod | Op::GetField) {
            // rewritten by another arm: the home is the value
            c.itmp = None;
            c.itmp_dirty = false;
        }
        if jump_targets[pc.saturating_add(1)] {
            c.clear_itmp(pc + 1);
        }
        // the array cache follows the same conservative set plus its own
        // arms; a back edge runs a safepoint, which can move the arena
        // Compile-time: the cache survives any op whose fast path keeps
        // x22 and whose slow path zeroes it at run time (the next reuse
        // finds 0 and derives), and any helper that cannot grow the array
        // arena (thin_keep_arrays). Only arrs growth moves a slot.
        let apreserves = match ins.op {
            Op::Len | Op::GetIndex | Op::ArrayPush | Op::SetIndex | Op::NewArrayLit => true, // arms manage it
            // an inlined call's fast path touches no array; its generic
            // fallback reloads and zeroes x22 at run time
            Op::Call => inlines.get(pc).is_some_and(|i| i.is_some()),
            Op::Jump => ins.sbx() >= 0,
            Op::Concat | Op::LoadConst | Op::NewObjectLit | Op::NewObject | Op::Closure
            | Op::NewCell | Op::LoadCell | Op::StoreCell | Op::GetUpval | Op::SetUpval
            | Op::GetGlobal | Op::TypeOf | Op::Not => true,
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
        c.a.add_imm(SP, SP, 32); // discard the proto and depth slots
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

    // out-of-line stubs (each ends in a branch back or a deopt exit)
    for st in std::mem::take(&mut c.stubs) {
        match st {
            Stub::Deopt { l, pc, lanes, itmp } => {
                c.a.bind(l);
                let saved = std::mem::replace(&mut c.lanes, lanes);
                let (si, sd) = (c.itmp, c.itmp_dirty);
                (c.itmp, c.itmp_dirty) = match itmp {
                    Some((v, d)) => (Some(v), d),
                    None => (None, false),
                };
                c.deopt_at(pc);
                c.lanes = saved;
                (c.itmp, c.itmp_dirty) = (si, sd);
            }
            Stub::Exit { l, sync, target } => {
                c.a.bind(l);
                for (v, r) in sync {
                    c.sync_lane(v, r);
                }
                c.a.b(target);
            }
            Stub::MulZero { l, ib, ic, ok, deopt } => {
                c.a.bind(l);
                c.a.orr_reg32(12, ib, ic);
                c.a.cmp_imm32(12, 0);
                c.a.b_cond(Cond::Lt, deopt);
                c.a.b(ok);
            }
        }
    }

    Some(c.a.finish())
}

/// Does anything from `start` on read vreg `v` before rewriting it?
/// A DFS over the bytecode's forward and backward edges; operand sets
/// are exact for the plain ops and conservative (read) for the rest.
fn live_at(code: &[Instr], start: usize, v: u8) -> bool {
    let reads = |i: &Instr| -> bool {
        match i.op {
            Op::LoadConst | Op::LoadInt | Op::LoadBool | Op::LoadNull | Op::LoadUndef
            | Op::Jump | Op::GetGlobal | Op::Halt => false,
            Op::Return | Op::JumpIfFalse | Op::JumpIfTrue | Op::Await => i.a == v,
            Op::Call => v == i.a || (v > i.a && v as usize <= i.a as usize + i.b as usize),
            Op::NewArrayLit => v >= i.b && (v as usize) < i.b as usize + i.c as usize,
            Op::NewObjectLit | Op::Concat | Op::Closure => true,
            Op::SetField | Op::SetIndex | Op::ArrayPush | Op::StoreCell | Op::SetUpval
            | Op::NewCell => i.a == v || i.b == v || i.c == v,
            _ => i.b == v || i.c == v,
        }
    };
    let mut seen = vec![false; code.len() + 2];
    let mut stack = vec![start];
    while let Some(pc) = stack.pop() {
        if pc >= code.len() || seen[pc] {
            continue;
        }
        seen[pc] = true;
        let i = &code[pc];
        if reads(i) {
            return true;
        }
        if writes_a_op(i.op) && i.a == v {
            continue; // rewritten on this path
        }
        match i.op {
            Op::Jump => stack.push((pc as i64 + i.sbx() as i64 + 1) as usize),
            Op::JumpIfFalse | Op::JumpIfTrue => {
                stack.push(pc + 1);
                stack.push((pc as i64 + i.sbx() as i64 + 1) as usize);
            }
            Op::EqSkip | Op::NeSkip | Op::LtSkip | Op::LeSkip | Op::GtSkip | Op::GeSkip => {
                stack.push(pc + 1);
                stack.push(pc + 2);
            }
            Op::Return | Op::Halt => {}
            _ => stack.push(pc + 1),
        }
    }
    false
}

/// Magic-number constants for signed division by a fixed divisor:
/// (multiplier, shift) such that n/d == (smulh(n, m) >> s) + sign_fix.
/// Standard Granlund-Montgomery derivation.
fn magic_div(d: i64) -> Option<(i64, u32)> {
    // unsigned_abs, not abs: |i64::MIN| overflows, and the i64::MIN check
    // is no use behind an operand that panics before it runs
    if d.unsigned_abs() < 2 || d == i64::MIN {
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
/// Each vreg in `vs` becomes a double in its home: an int-tagged value
/// converts, a double passes, anything else branches to `fail`.
fn emit_dbl_guard(c: &mut C, vs: &[u8], fail: Label) {
    for &v in vs {
        let next = c.a.new_label();
        c.fetch_x(v, 8);
        c.a.lsr_imm(10, 8, 48);
        c.a.cmp_reg(10, R_TAGLIM);
        c.a.b_cond(Cond::Hi, fail);
        c.a.b_cond(Cond::Ne, next); // already a double
        c.a.sxtw(8, 8);
        c.a.scvtf(0, 8);
        c.put(v, 0);
        c.a.bind(next);
    }
}

/// `+ - *` on two numeric operands of unknown representation (small
/// ints on): both int-tagged -> 32-bit op, overflow or a -0 product
/// -> doubles; any other mix -> doubles. Result to vreg `va`'s home.
/// Callers must have proven both operands numeric.
fn emit_arith_dyn(c: &mut C, op: Op, vb: u8, vc: u8, va: u8) {
    let as_dbl = c.a.new_label();
    let done = c.a.new_label();
    let nz = c.a.new_label();
    let zero = c.a.new_label();
    if !c.int_static {
        // speculation withdrawn here: the values that reached this op
        // outgrew i32, so doubles are the common case — test for two
        // doubles first and run them straight from the homes
        let mixed = c.a.new_label();
        c.fetch_x(vb, 8);
        c.fetch_x(vc, 9);
        c.a.lsr_imm(10, 8, 48);
        c.a.cmp_reg(10, R_TAGLIM);
        c.a.b_cond(Cond::Hs, mixed);
        c.a.lsr_imm(11, 9, 48);
        c.a.cmp_reg(11, R_TAGLIM);
        c.a.b_cond(Cond::Hs, mixed);
        c.a.fmov_dx(0, 8);
        c.a.fmov_dx(1, 9);
        match op {
            Op::Add => c.a.fadd(0, 0, 1),
            Op::Sub => c.a.fsub(0, 0, 1),
            Op::Mul => c.a.fmul(0, 0, 1),
            _ => unreachable!(),
        }
        c.put(va, 0);
        c.a.b(done);
        c.a.bind(mixed);
        c.a.cmp_reg(10, R_TAGLIM);
        c.a.b_cond(Cond::Ne, as_dbl);
        c.a.cmp_reg(11, R_TAGLIM);
        c.a.b_cond(Cond::Ne, as_dbl);
    } else {
        c.fetch_x(vb, 8);
        c.fetch_x(vc, 9);
        c.a.lsr_imm(10, 8, 48);
        c.a.lsr_imm(11, 9, 48);
        c.a.cmp_reg(10, R_TAGLIM);
        c.a.b_cond(Cond::Ne, as_dbl);
        c.a.cmp_reg(11, R_TAGLIM);
        c.a.b_cond(Cond::Ne, as_dbl);
    }
    match op {
        Op::Add => {
            c.a.adds_reg32(14, 8, 9);
            c.a.b_cond(Cond::Vs, as_dbl);
        }
        Op::Sub => {
            c.a.subs_reg32(14, 8, 9);
            c.a.b_cond(Cond::Vs, as_dbl);
        }
        Op::Mul => {
            c.a.smull(14, 8, 9);
            c.a.cmp_ext_sxtw(14, 14);
            c.a.b_cond(Cond::Ne, as_dbl);
            c.a.orr_reg32(14, 31, 14);
            c.a.cbz32(14, zero);
        }
        _ => unreachable!(),
    }
    c.a.bind(nz);
    c.a.movk(14, tsr_memory::TAG_INT as u16, 48);
    c.put_x(va, 14);
    c.a.b(done);
    if op == Op::Mul {
        // a zero product owes -0 when either factor is negative
        c.a.bind(zero);
        c.a.orr_reg32(12, 8, 9);
        c.a.cmp_imm32(12, 0);
        c.a.b_cond(Cond::Ge, nz);
    }
    c.a.bind(as_dbl);
    c.any_to_dbl(8, 0);
    c.any_to_dbl(9, 1);
    match op {
        Op::Add => c.a.fadd(0, 0, 1),
        Op::Sub => c.a.fsub(0, 0, 1),
        Op::Mul => c.a.fmul(0, 0, 1),
        _ => unreachable!(),
    }
    c.put(va, 0);
    c.a.bind(done);
}

fn emit_lane_guard(c: &mut C, vs: &[u8], laned: &[(u8, u32)], fail: Label) {
    for &v in vs {
        if c.smi {
            // int-tagged already, or an integral i32 double that becomes
            // one here: inside the loop every int_ok home is int-tagged
            let is_int = c.a.new_label();
            let next = c.a.new_label();
            c.fetch_x(v, 8);
            c.a.lsr_imm(9, 8, 48);
            c.a.cmp_reg(9, R_TAGLIM);
            c.a.b_cond(Cond::Eq, is_int);
            c.a.b_cond(Cond::Hi, fail);
            c.a.fmov_dx(0, 8);
            c.a.fcvtzs(10, 0);
            c.a.scvtf(1, 10);
            c.a.fcmp(1, 0); // integral?
            c.a.b_cond(Cond::Ne, fail);
            c.a.sxtw(12, 10);
            c.a.cmp_reg(12, 10);
            c.a.b_cond(Cond::Ne, fail); // wider than i32
            c.a.mov(8, 10);
            c.box_int(8);
            c.put_x(v, 8);
            c.a.b(next);
            c.a.bind(is_int);
            c.a.sxtw(10, 8);
            c.a.bind(next);
        } else {
            let d = c.fetch(v, 0);
            c.a.fcvtzs(10, d);
            c.a.scvtf(1, 10);
            c.a.fcmp(1, d); // integral?
            c.a.b_cond(Cond::Ne, fail);
            c.a.sxtw(12, 10);
            c.a.cmp_reg(12, 10);
            c.a.b_cond(Cond::Ne, fail); // wider than i32
        }
        if let Some(&(_, r)) = laned.iter().find(|(lv, _)| *lv == v) {
            c.a.mov(r, 10);
        }
    }
}

/// The laned vreg for a header, if any: the first speculated vreg whose
/// every in-loop write goes through an arm that maintains the register.
/// The lane a loop takes, if any: its own pick, unless an inner loop
/// inside its range has a pick of its own. There is one lane register,
/// and the inner loop is the hot one, so the outer stays boxed rather
/// than sharing x23 and reading the inner counter after the inner loop
/// exits (which is what happened once array loops became eligible).
/// Does `op` write its `a` operand (as opposed to reading it)?
fn writes_a_op(op: Op) -> bool {
    !matches!(
        op,
        Op::SetField | Op::SetIndex | Op::ArrayPush | Op::StoreCell | Op::SetUpval
            | Op::Jump | Op::JumpIfFalse | Op::JumpIfTrue | Op::Return | Op::Halt
            | Op::EqSkip | Op::NeSkip | Op::LtSkip | Op::LeSkip | Op::GtSkip | Op::GeSkip
    )
}

fn lane_picks(pbody: &tsc_ir::ProtoBody, facts: &Facts, h: usize, vs: &[u8]) -> Vec<u8> {
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
    // An await leaves compiled code mid-loop and resumes without passing
    // the header, so nothing would re-establish the lane register: the
    // promises benchmark summed from a stale x23 the day array loops
    // became eligible. There is no integer chain worth keeping across a
    // suspension anyway.
    if pbody.code[h..=end].iter().any(|i| i.op == Op::Await) {
        return Vec::new();
    }
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
    // With small ints on, every write to a d-homed int is an fmov round
    // trip (~10 cycles on the loop-carried chain: primes ran 2.7x
    // slower with `d++` boxed); a GP home pays for itself without any
    // conversion to remove.
    if !converts && !tsr_memory::smi_on() {
        return Vec::new();
    }
    let handled = |v: u8| {
        pbody.code[h..=end].iter().enumerate().all(|(off, i)| {
            if i.a != v {
                return true;
            }
            match i.op {
                Op::Add | Op::Sub | Op::Mul => true,
                // ToInt32 results are ints (UShr may not fit one)
                Op::BitAnd | Op::BitOr | Op::BitXor | Op::Shl | Op::Shr | Op::BitNot => true,
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
    };
    // Prefer the value the converting op writes: that is the chain the
    // lane exists to keep unboxed. Taking the first eligible candidate
    // picked the loop counter in `(s * 31 + i) % M` — register order put
    // it ahead of the accumulator — which removes no conversion at all.
    let converted = |v: u8| {
        pbody.code[h..=end].iter().enumerate().any(|(off, i)| {
            i.a == v
                && match i.op {
                    Op::Mod => facts
                        .const_ops
                        .get(h + off)
                        .and_then(|o| o[1])
                        .is_some_and(|d| magic_div(d).is_some()),
                    Op::BitAnd | Op::BitOr | Op::BitXor | Op::Shl | Op::Shr
                    | Op::UShr | Op::BitNot => true,
                    _ => false,
                }
        })
    };
    // Before that: an accumulator whose home is a stack slot. Boxed, each
    // iteration is slot load -> fcvtzs -> add -> scvtf -> slot store, a
    // ~13-cycle loop-carried chain that nothing else in the loop can
    // hide behind — objects ran at exactly that pace with `acc` in slot
    // r8 while the lane held the `% 7` temporary it fed.
    let accumulates = |v: u8| {
        pbody.code[h..=end].iter().any(|i| {
            i.a == v && matches!(i.op, Op::Add | Op::Sub | Op::Mul) && (i.b == v || i.c == v)
        })
    };
    // Order: loop-carried values first (a boxed one round-trips through
    // its home on the carried chain every iteration), slot-homed ones
    // ahead of register-homed; then the converting ops' results; then
    // the rest.
    let mut out: Vec<u8> = Vec::new();
    for v in vs.iter().copied().filter(|&v| v >= LOW && handled(v) && accumulates(v)) {
        out.push(v);
    }
    for v in vs.iter().copied().filter(|&v| handled(v) && accumulates(v)) {
        if !out.contains(&v) {
            out.push(v);
        }
    }
    for v in vs.iter().copied().filter(|&v| handled(v) && converted(v)) {
        if !out.contains(&v) {
            out.push(v);
        }
    }
    for v in vs.iter().copied().filter(|&v| handled(v)) {
        if !out.contains(&v) {
            out.push(v);
        }
    }
    out
}

/// Mod with a known integer divisor: the divisor needs no runtime
/// integer check, and n/d becomes smulh+shift instead of sdiv (~10-cycle
/// latency on Apple cores, and this sits in the loop-carried chain).
fn emit_mod_const(c: &mut C, a_reg: u8, db: u32, d: i64, fmod_addr: usize, int_in: Option<u32>, pc: usize) {
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
        c.a.sxtw(10, src); // an int register is valid in its low word only
    } else if c.smi {
        // raw home bits: an int-tagged dividend is the integer itself, a
        // double must prove integral
        let dbl = c.a.new_label();
        let join = c.a.new_label();
        c.a.fmov_xd(8, db);
        c.a.lsr_imm(9, 8, 48);
        c.a.cmp_reg(9, R_TAGLIM);
        c.a.b_cond(Cond::Ne, dbl);
        c.a.sxtw(10, 8);
        c.a.b(join);
        c.a.bind(dbl);
        c.a.fcvtzs(10, db);
        c.a.scvtf(2, 10);
        c.a.fcmp(2, db);
        c.a.b_cond(Cond::Ne, slow);
        c.a.bind(join);
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
    // Sign fixup. JS `%` carries the dividend's sign, which a nonzero
    // integer remainder already has — so OR-ing the dividend's sign bit
    // is idempotent there, and supplies the -0 a zero remainder needs.
    //
    // Whether to branch on it depends on the divisor. A zero remainder
    // turns up once every |d| iterations, so the branch is unpredictable
    // for a small divisor and free for a large one: `% 7` mispredicts one
    // time in seven, and its `cbnz` was the single hottest instruction in
    // objects.ts. Misprediction costs about 15 cycles against roughly 1.5
    // for the branchless form, so it pays out to |d| near 10; beyond that
    // the branch predicts and the extra instructions are pure loss (1.3%
    // on gc_churn, whose modulus is 1000000007).
    let dst = if a_reg < LOW { (8 + a_reg) as u32 } else { 2 };
    // Inside a laned loop the integer remainder is worth keeping: the
    // consumer is usually an accumulate, and reading it back from the
    // boxed home costs the scvtf/fcvtzs pair on that loop-carried chain
    // (objects ran at that chain's latency). |r| < |d| fits i32.
    let keep = !c.lanes.is_empty() && c.lane_of(a_reg).is_none();
    if keep {
        c.retire_itmp(pc + 1, Some(a_reg));
    }
    if c.smi && i32::try_from(d).is_ok() {
        // small ints: the remainder is an int-tagged home (|r| < |d|
        // fits i32); only a zero remainder of a negative dividend is the
        // double -0, and that branch is predicted by the dividend's sign
        if let Some(r) = c.lane_of(a_reg) {
            c.a.mov(r, 13);
        }
        if keep {
            c.a.mov(R_ITMP, 13);
        }
        mod_int_result(c, a_reg, 10, 13, pc);
    } else if d.unsigned_abs() <= 16 {
        if let Some(r) = c.lane_of(a_reg) {
            c.a.mov(r, 13);
        }
        if keep {
            c.a.mov(R_ITMP, 13);
        }
        c.a.scvtf(dst, 13);
        c.a.fmov_xd(14, dst);
        // the dividend's sign bit: from the integer in x10 (its boxed
        // form may be int-tagged, whose bit 63 is set)
        c.a.asr_imm(9, 10, 63);
        c.a.mov_imm64(12, 0x8000_0000_0000_0000);
        c.a.and_reg(9, 9, 12);
        c.a.orr_reg(14, 14, 9);
        c.a.fmov_dx(dst, 14);
        if a_reg >= LOW {
            c.a.str_d_imm(dst, R_SLOTS, C::slot(a_reg));
        }
    } else {
        let nonzero = c.a.new_label();
        c.a.cbnz(13, nonzero);
        c.a.asr_imm(14, 10, 63);
        c.a.mov_imm64(12, 0x8000_0000_0000_0000);
        c.a.and_reg(14, 14, 12);
        if let Some(r) = c.lane_of(a_reg) {
            c.a.movz(r, 0, 0);
        }
        if keep {
            c.a.movz(R_ITMP, 0, 0);
        }
        c.put_x(a_reg, 14);
        c.a.b(done);
        c.a.bind(nonzero);
        if let Some(r) = c.lane_of(a_reg) {
            c.a.mov(r, 13);
        }
        if keep {
            c.a.mov(R_ITMP, 13);
        }
        c.a.scvtf(dst, 13);
        if a_reg >= LOW {
            c.a.str_d_imm(dst, R_SLOTS, C::slot(a_reg));
        }
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
    if keep {
        // a fractional result cannot be held as an integer; re-run the
        // op in the interpreter (inputs are still in their homes)
        let exact = c.a.new_label();
        c.a.fcvtzs(R_ITMP, 0);
        c.a.scvtf(1, R_ITMP);
        c.a.fcmp(1, 0);
        c.a.b_cond(Cond::Eq, exact);
        c.deopt_at(pc);
        c.a.bind(exact);
    }
    c.put(a_reg, 0);
    c.a.bind(done);
    if keep {
        c.itmp = Some(a_reg);
        c.itmp_dirty = false; // every tail wrote the home
    }
}

/// Box the integer remainder w{r} of dividend w{n} into vreg a's home:
/// int-tagged, except a zero remainder of a negative dividend, which
/// is -0. Clobbers x14.
fn mod_int_result(c: &mut C, a_reg: u8, n: u32, r: u32, pc: usize) {
    let ok = c.a.new_label();
    if c.lane_of(a_reg).is_some() || c.int_static {
        // a -0 has no integer form, so that case runs in the interpreter
        // (a laned register is the value; otherwise the home takes the
        // int — the analysis typed this result IntV)
        let bad = c.deopt_stub(pc);
        c.a.cmp_imm32(n, 0);
        c.a.b_cond(Cond::Ge, ok);
        c.a.cbz32(r, bad);
        c.a.bind(ok);
        if c.lane_of(a_reg).is_none() {
            c.a.orr_reg32(14, 31, r);
            c.a.movk(14, tsr_memory::TAG_INT as u16, 48);
            c.put_x(a_reg, 14);
        }
        return;
    }
    c.a.orr_reg32(14, 31, r);
    c.a.movk(14, tsr_memory::TAG_INT as u16, 48);
    c.a.cmp_imm32(n, 0);
    c.a.b_cond(Cond::Ge, ok);
    c.a.cbnz32(r, ok);
    c.a.movz(14, 0x8000, 48); // -0.0
    c.a.bind(ok);
    c.put_x(a_reg, 14);
}

/// Mod of two ints held in the low words of x{xb}/x{xc} (small ints
/// on): sdiv on the chain, `x % 0` is NaN. Result lands in vreg a's home
/// as an int (or -0).
fn emit_mod_int(c: &mut C, a_reg: u8, xb: u32, xc: u32, pc: usize) {
    let keep = !c.lanes.is_empty() && c.lane_of(a_reg).is_none();
    if keep {
        c.retire_itmp(pc + 1, Some(a_reg));
    }
    let zero = c.a.new_label();
    let done = c.a.new_label();
    c.a.cbz32(xc, zero);
    c.a.sdiv32(12, xb, xc);
    c.a.msub32(13, 12, xc, xb);
    if let Some(r) = c.lane_of(a_reg) {
        c.a.mov(r, 13);
    }
    if keep {
        c.a.mov(R_ITMP, 13);
    }
    mod_int_result(c, a_reg, xb, 13, pc);
    c.a.b(done);
    c.a.bind(zero);
    // x % 0 = NaN, which no integer register (or an IntV-typed result)
    // can hold
    if c.lane_of(a_reg).is_some() || keep || c.int_static {
        c.deopt_at(pc);
    } else {
        c.a.mov_imm64(14, f64::NAN.to_bits());
        c.put_x(a_reg, 14);
    }
    c.a.bind(done);
    if keep {
        c.itmp = Some(a_reg);
        c.itmp_dirty = false;
    } else if c.itmp == Some(a_reg) {
        c.itmp = None;
        c.itmp_dirty = false;
    }
}

/// Integer-fast-path Mod on numeric inputs in d{db}/d{dc}; falls back to
/// libm fmod. Result lands in vreg a's home.
fn emit_mod_num(c: &mut C, a_reg: u8, db: u32, dc: u32, fmod_addr: usize) {
    // the result is boxed straight to the home: no intermediate survives
    if c.itmp == Some(a_reg) {
        c.itmp = None;
        c.itmp_dirty = false;
    }
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
    // remainder 0: ±0 with the dividend's sign (from the integer)
    c.a.asr_imm(14, 10, 63);
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
    // 16: a three-field literal with its arithmetic and the emitter's dead
    // `LoadUndef; Return` epilogue is 14
    if b.code.len() > 16 || b.n_regs > 24 {
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
                // small literals: the templates take a mapped register
                // window and a baked shape; values are checked at run time
                // against the shape since the callee carries no facts
                Op::NewArrayLit => i.c as usize <= 8,
                Op::NewObjectLit => matches!(b.consts.get(i.c as usize), Some(Const::Keys(k)) if k.len() <= 3),
                // a closure creation is one helper call; its upval sources
                // are the callee's own registers, which live in the window
                Op::Closure => (i.bx() as usize) < b.protos.len(),
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
    pc: usize,
    ins: Instr,
    proto_word: u64,
    callee: &FunctionProto,
    callee_lits: &[(u64, u32, u64)],
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
    c.a.movz(11, tsr_memory::TAG_CLOSURE as u16, 0); // TAG_CLOSURE
    c.a.cmp_reg(10, 11);
    c.a.b_cond(Cond::Ne, generic);
    c.unbox(10, 8);
    c.a.ldr_imm(11, 10, o.closure_proto);
    c.a.mov_imm64(12, proto_word);
    c.a.cmp_reg(11, 12);
    c.a.b_cond(Cond::Ne, generic);
    // x14 = closure cell address (kept for GetUpval; arith avoids x14
    // except Mod, whose int path uses 10-14 — reload there if needed)
    c.a.mov(14, 10);

    // number guard: with small ints on, an int-tagged operand is a number
    // too (the arms dispatch on the tag or convert)
    let guard = |c: &mut C, v: u8, g: Label| {
        c.fetch_x(v, 8);
        c.a.lsr_imm(10, 8, 48);
        c.a.cmp_reg(10, R_TAGLIM);
        c.a.b_cond(if c.smi { Cond::Hi } else { Cond::Hs }, g);
    };
    // the (guarded) numeric vreg as a double in d{d} — or its home
    let dfetch = |c: &mut C, v: u8, d: u32| -> u32 {
        if c.smi {
            c.fetch_x(v, 8);
            c.any_to_dbl(8, d);
            d
        } else {
            c.fetch(v, d)
        }
    };
    for (cpc, cins) in callee.body().code.iter().enumerate() {
        match cins.op {
            Op::NewArrayLit => {
                // the callee's own analysis, run when the inline target
                // was chosen, says what this literal holds; K_ARR when it
                // could not be typed, which every later store admits
                let kind = callee_lits
                    .get(cpc)
                    .map(|&(_, _, k)| k)
                    .filter(|&k| k != 0)
                    .unwrap_or(tsr_memory::cells::K_ARR);
                c.emit_arr_lit(map(cins.a), map(cins.b), cins.c as usize, kind);
                c.a.movz(14, 0, 0); // closure addr clobbered; GetUpval reloads
            }
            Op::NewObjectLit => {
                let Some(&(shape_ptr, sn, _)) = callee_lits.get(cpc).filter(|&&(sp, _, _)| sp != 0)
                else {
                    c.a.b(generic);
                    continue;
                };
                // no facts for the callee: every value proves itself
                // against the field's current representation
                let shape = unsafe { &*(shape_ptr as *const tsr_memory::ShapeData) };
                let checks: Vec<u8> = (0..sn as usize)
                    .map(|j| match shape.repr(j) {
                        tsr_memory::Repr::Int32 => 2,
                        tsr_memory::Repr::Number => 3,
                        tsr_memory::Repr::Any => 0,
                    })
                    .collect();
                c.emit_obj_lit(map(cins.a), map(cins.b), sn as u8, shape_ptr, &checks);
                c.a.movz(14, 0, 0);
            }
            Op::LoadInt => c.put_bits(map(cins.a), if c.smi { Value::int(cins.sbx()) } else { Value::number(cins.sbx() as f64) }.bits()),
            Op::LoadUndef => c.put_bits(map(cins.a), Value::UNDEFINED.bits()),
            Op::LoadNull => c.put_bits(map(cins.a), Value::NULL.bits()),
            Op::LoadBool => c.put_bits(
                map(cins.a),
                if cins.b != 0 { Value::TRUE.bits() } else { Value::FALSE.bits() },
            ),
            Op::LoadConst => {
                let bits = match &callee.body().consts[cins.bx() as usize] {
                    Const::Number(n) => if c.smi { Value::num(*n) } else { Value::number(*n) }.bits(),
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
                if c.smi && cins.op != Op::Div {
                    emit_arith_dyn(c, cins.op, map(cins.b), map(cins.c), map(cins.a));
                    continue;
                }
                let db = dfetch(c, map(cins.b), 0);
                let dc = dfetch(c, map(cins.c), 1);
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
                // a divisor the callee loaded as an integer constant takes
                // the magic-multiply path (no sdiv on the chain, no second
                // integrality check); `x + bias % 100` ran the generic one
                let const_div = callee.body().code[..cpc]
                    .iter()
                    .rev()
                    .find(|i| writes_a_op(i.op) && i.a == cins.c)
                    .and_then(|i| (i.op == Op::LoadInt).then(|| i.sbx() as i64));
                match const_div {
                    Some(d) => {
                        // raw home: the constant path dispatches on the tag
                        let db = c.fetch(map(cins.b), 0);
                        emit_mod_const(c, map(cins.a), db, d, fmod_addr, None, pc);
                        c.itmp = None; // window vreg: not an intermediate for the caller
                        c.itmp_dirty = false;
                    }
                    None => {
                        guard(c, map(cins.c), generic);
                        let db = dfetch(c, map(cins.b), 0);
                        let dc = dfetch(c, map(cins.c), 1);
                        emit_mod_num(c, map(cins.a), db, dc, fmod_addr);
                    }
                }
                // the fmod slow paths zero the field cache themselves; x14
                // (closure addr) may be clobbered: GetUpval reloads on 0
                c.a.movz(14, 0, 0);
            }
            Op::Closure => {
                // the callee's window is the frame: its upval sources are
                // callee registers; ParentUpval reads the callee's closure
                let child = &callee.body().protos[cins.bx() as usize];
                let call_a = ins.a;
                c.emit_closure(
                    map(cins.a),
                    child,
                    &|r| map(r),
                    &|c: &mut C| {
                        c.fetch_x(call_a, 8);
                        c.unbox(4, 8);
                        4
                    },
                    callee as *const FunctionProto as u64,
                    cpc,
                    w as u32 * 8,
                    Some(*o),
                );
                c.a.movz(14, 0, 0);
            }
            Op::Neg => {
                guard(c, map(cins.b), generic);
                let db = dfetch(c, map(cins.b), 0);
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
                let db = dfetch(c, map(cins.b), 0);
                c.a.fcvtzs(10, db);
                if !unary {
                    let dc = dfetch(c, map(cins.c), 1);
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
                c.unbox(14, 8);
                c.a.bind(have);
                c.a.ldr_imm(8, 14, o.closure_upvals + cins.b as u32 * 8);
                match callee.upvals[cins.b as usize] {
                    tsc_ir::UpvalSrc::ParentLocalValue(_) => {}
                    _ => {
                        // cell deref (cells are old-only, non-moving)
                        c.unbox(9, 8);
                        c.a.ldr_imm(8, 9, o.cell_val);
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
    inlines: &[Option<(u64, std::sync::Arc<FunctionProto>, Vec<(u64, u32, u64)>)>],
    fcache: &mut Option<u8>,
    acache: &mut Option<u8>,
    in_lane: bool,
    primary: Option<u8>,
) {
    let pbody = proto.body();
    let num_bc = facts.num[pc][1] && facts.num[pc][2];
    let num_b = facts.num[pc][1];
    let smi = c.smi;
    // doubles for certain: the unguarded FP lanes need these
    let dbl_bc = facts.dbl[pc][1] && facts.dbl[pc][2];
    let dbl_b = facts.dbl[pc][1];
    match ins.op {
        Op::LoadInt => c.put_bits(ins.a, if smi { Value::int(ins.sbx()) } else { Value::number(ins.sbx() as f64) }.bits()),
        Op::LoadBool => c.put_bits(
            ins.a,
            if ins.b != 0 { Value::TRUE.bits() } else { Value::FALSE.bits() },
        ),
        Op::LoadNull => c.put_bits(ins.a, Value::NULL.bits()),
        Op::LoadUndef => c.put_bits(ins.a, Value::UNDEFINED.bits()),
        Op::LoadConst => match pbody.consts.get(ins.bx() as usize) {
            Some(Const::Number(n)) => c.put_bits(ins.a, if smi { Value::num(*n) } else { Value::number(*n) }.bits()),
            // A string constant is interned per realm in a direct-mapped
            // cache keyed on the constant's address — a compile-time
            // constant, so its slot is too. Probe inline; the helper
            // fills it on a miss. (alloc's push loop paid a call per
            // iteration for the literal "s".)
            Some(Const::Str(s)) if c.offsets.is_some() => {
                let o = c.offsets.unwrap();
                let key = std::sync::Arc::as_ptr(s) as *const u8 as usize;
                let slot = ((key >> 3) & (CONST_CACHE - 1)) as u32;
                let slow = c.a.new_label();
                let done = c.a.new_label();
                c.a.ldr_imm(10, R_REALM, o.const_cache_ptr);
                c.a.mov_imm64(9, key as u64);
                c.a.ldr_imm(11, 10, slot * 16);
                c.a.cmp_reg(11, 9);
                c.a.b_cond(Cond::Ne, slow);
                c.a.ldr_imm(0, 10, slot * 16 + 8);
                c.a.b(done);
                c.a.bind(slow);
                c.a.mov(0, R_REALM);
                c.proto_into(1);
                c.a.mov_imm64(2, ins.bx() as u64);
                c.thin_keep_arrays(c.helpers.load_const);
                c.a.bind(done);
                c.put_x(ins.a, 0);
            }
            // other consts — thin helper (no spill/reload)
            _ => {
                c.a.mov(0, R_REALM);
                c.proto_into(1);
                c.a.mov_imm64(2, ins.bx() as u64);
                c.thin_keep_arrays(c.helpers.load_const);
                c.put_x(ins.a, 0);
            }
        },
        Op::Move => {
            let src = c.fetch(ins.b, 0);
            c.put(ins.a, src);
        }

        Op::Add | Op::Sub | Op::Mul => {
            let (cb, cc) = (cls_of(facts, pc, 0, smi), cls_of(facts, pc, 1, smi));
            // this destination already overflowed i32 at run time
            let withdrawn = (facts.no_int >> (ins.a as u32).min(63)) & 1 == 1;
            let is_int = |x: Cls| matches!(x, Cls::Int | Cls::IntK(_));
            let is_dbl = |x: Cls| matches!(x, Cls::Dbl | Cls::IntK(_));
            // an operand the lane machinery holds as an integer: in the
            // lane / intermediate register, or an int_ok home
            let laned = |c: &C, v: u8| {
                c.lane_on && in_lane && (c.tmp_reg(v).is_some() || c.int_ok.contains(&v))
            };
            if smi && facts.int_static && !withdrawn && (is_int(cb) || laned(c, ins.b)) && (is_int(cc) || laned(c, ins.c)) {
                // proven ints: 32-bit flag-setting op straight off the
                // tagged bits (the low word is the int, no unboxing). A
                // result outside i32 — or a zero product owing a -0 —
                // deopts to this pc and the interpreter makes a double.
                let opnd = |c: &mut C, cls: Cls, v: u8, x: u32| -> u32 {
                    if let Cls::IntK(k) = cls {
                        c.a.mov_imm64(x, k as u64);
                        return x;
                    }
                    if let Some(r) = c.lane_of(v) {
                return r;
            }
                    if c.itmp == Some(v) {
                        return R_ITMP;
                    }
                    c.fetch_x(v, x);
                    x
                };
                let ib = opnd(c, cb, ins.b, 10);
                let bad = c.deopt_stub(pc);
                // a small constant right operand is an immediate
                let imm = match cc {
                    Cls::IntK(k) if (0..4096).contains(&k) => Some((k as u32, false)),
                    Cls::IntK(k) if (-4095..0).contains(&k) => Some(((-k) as u32, true)),
                    _ => None,
                };
                match (ins.op, imm) {
                    (Op::Add, Some((k, neg))) => {
                        if neg { c.a.subs_imm32(14, ib, k) } else { c.a.adds_imm32(14, ib, k) }
                        c.a.b_cond(Cond::Vs, bad);
                    }
                    (Op::Sub, Some((k, neg))) => {
                        if neg { c.a.adds_imm32(14, ib, k) } else { c.a.subs_imm32(14, ib, k) }
                        c.a.b_cond(Cond::Vs, bad);
                    }
                    (Op::Add, None) => {
                        let ic = opnd(c, cc, ins.c, 11);
                        c.a.adds_reg32(14, ib, ic);
                        c.a.b_cond(Cond::Vs, bad);
                    }
                    (Op::Sub, None) => {
                        let ic = opnd(c, cc, ins.c, 11);
                        c.a.subs_reg32(14, ib, ic);
                        c.a.b_cond(Cond::Vs, bad);
                    }
                    (Op::Mul, _) => {
                        let ic = opnd(c, cc, ins.c, 11);
                        c.a.smull(14, ib, ic);
                        c.a.cmp_ext_sxtw(14, 14); // fits i32?
                        c.a.b_cond(Cond::Ne, bad);
                        // A zero product owes -0 only when a factor is
                        // negative; a positive constant factor rules that
                        // out (0 * k is +0), so the check goes entirely.
                        let pos_k = matches!(cb, Cls::IntK(k) if k > 0)
                            || matches!(cc, Cls::IntK(k) if k > 0);
                        if !pos_k {
                            let ok = c.a.new_label();
                            let z = c.a.new_label();
                            c.a.cbz32(14, z);
                            c.stubs.push(Stub::MulZero { l: z, ib, ic, ok, deopt: bad });
                            c.a.bind(ok);
                        }
                    }
                    _ => unreachable!(),
                }
                if c.lane_on && in_lane {
                    // the lane / intermediate register bookkeeping owns
                    // this vreg: int_dst updates it and syncs the home
                    c.int_dst_w(ins.a, 14, pc);
                } else {
                    c.a.orr_reg32(14, 31, 14); // zero-extend for the box
                    c.a.movk(14, tsr_memory::TAG_INT as u16, 48);
                    c.put_x(ins.a, 14);
                    if c.itmp == Some(ins.a) {
                        c.itmp = None;
                        c.itmp_dirty = false;
                    }
                }
                return;
            }
            // integer lane (doubles): when either operand already lives
            // in an integer register, stay there. Leaving the lane would
            // mean a conversion out and another back in, and both would
            // sit in the middle of the dependency chain.
            let known_int = |c: &C, v: u8, side: usize| {
                c.tmp_reg(v).is_some()
                    || c.int_ok.contains(&v)
                    || facts.const_ops.get(pc).and_then(|o| o[side]).is_some()
            };
            if !smi
                && num_bc
                && c.lane_on
                && in_lane
                && (c.tmp_reg(ins.b).is_some() || c.tmp_reg(ins.c).is_some())
                && known_int(c, ins.b, 0)
                && known_int(c, ins.c, 1)
            {
                // a known integer constant is an immediate, not a boxed
                // home to reload and convert (`i + 1` did exactly that)
                let const_at = |c: &mut C, side: usize, v: u8, scratch: u32| -> u32 {
                    match facts.const_ops.get(pc).and_then(|o| o[side]) {
                        Some(k) => {
                            c.a.mov_imm64(scratch, k as u64);
                            scratch
                        }
                        None => c.int_src(v, scratch),
                    }
                };
                let ib = const_at(c, 0, ins.b, 10);
                let ic = const_at(c, 1, ins.c, 11);
                match ins.op {
                    Op::Add => c.a.add_reg(14, ib, ic),
                    Op::Sub => c.a.sub_reg(14, ib, ic),
                    Op::Mul => c.a.mul(14, ib, ic),
                    _ => unreachable!(),
                }
                c.int_dst(ins.a, 14, pc);
                return;
            }
            // Speculation withdrawn: this function's arithmetic is
            // double arithmetic (the analysis types it `Dbl`), so
            // convert both operands once and use the FP unit — the tag
            // dispatch below would cost four branches an iteration.
            if smi && withdrawn && num_bc {
                let db = if cb == Cls::Num {
                    c.fetch_x(ins.b, 8);
                    c.any_to_dbl(8, 0);
                    0
                } else {
                    load_dbl(c, cb, ins.b, 0)
                };
                let dc = if cc == Cls::Num {
                    c.fetch_x(ins.c, 9);
                    c.any_to_dbl(9, 1);
                    1
                } else {
                    load_dbl(c, cc, ins.c, 1)
                };
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
                c.fix_int_write(ins.a, pc);
                return;
            }
            if dbl_bc || (smi && is_dbl(cb) && is_dbl(cc) && (cb == Cls::Dbl || cc == Cls::Dbl)) {
                let db = load_dbl(c, cb, ins.b, 0);
                let dc = load_dbl(c, cc, ins.c, 1);
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
                // a laned / int_ok destination: the double result must
                // land in its integer views too (multi-lane: `p.x + p.y`
                // into x27 was left stale by this arm — objects read 0)
                c.fix_int_write(ins.a, pc);
            } else if smi && cb != Cls::Other && cc != Cls::Other {
                // numeric, representation unknown: dispatch on the tags
                emit_arith_dyn(c, ins.op, ins.b, ins.c, ins.a);
                c.fix_int_write(ins.a, pc);
            } else {
                // guarded: numbers inline, anything else (concat, errors)
                // through the interpreter step
                let slow = c.a.new_label();
                let done = c.a.new_label();
                c.fetch_x(ins.b, 8);
                c.fetch_x(ins.c, 9);
                c.guard_numeric(8, slow);
                c.guard_numeric(9, slow);
                c.any_to_dbl(8, 0);
                c.any_to_dbl(9, 1);
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
                    c.proto_into(1);
                    c.a.mov_imm64(2, pc as u64);
                    c.fetch_x(ins.b, 3);
                    c.fetch_x(ins.c, 4);
                    c.thin_keep_arrays(c.helpers.add_slow);
                    c.put_x(ins.a, 0);
                } else {
                    c.step_full(pc);
                }
                c.a.bind(done);
                c.fix_int_write(ins.a, pc);
            }
        }
        Op::Div => {
            let (cb, cc) = (cls_of(facts, pc, 0, smi), cls_of(facts, pc, 1, smi));
            if num_bc {
                let (db, dc) = if smi {
                    // every numeric class converts to a double, statically
                    // or by its tag; the quotient is a double
                    c.fetch_x(ins.b, 8);
                    c.fetch_x(ins.c, 9);
                    let db = if cb == Cls::Num { c.any_to_dbl(8, 0); 0 } else { load_dbl(c, cb, ins.b, 0) };
                    let dc = if cc == Cls::Num { c.any_to_dbl(9, 1); 1 } else { load_dbl(c, cc, ins.c, 1) };
                    (db, dc)
                } else {
                    (c.fetch(ins.b, 0), c.fetch(ins.c, 1))
                };
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
                let cb = cls_of(facts, pc, 0, smi);
                let int_b = (c.lane_on && in_lane && c.tmp_reg(ins.b).is_some())
                    || (facts.int_facts.get(pc).is_some_and(|f| f[1]) && (smi || const_div.is_some()));
                let cc = cls_of(facts, pc, 1, smi);
                if smi && const_div.is_none() && int_b && matches!(cc, Cls::Int | Cls::IntK(_)) {
                    let xb = load_int(c, cb, ins.b, 10);
                    let xc = load_int(c, cc, ins.c, 11);
                    emit_mod_int(c, ins.a, xb, xc, pc);
                    return;
                }
                // the dividend as a double in d0 (an int converts; an
                // unknown representation dispatches): the sign fixup and
                // the fallback read it
                let db = if !smi {
                    c.fetch(ins.b, 0)
                } else if int_b && const_div.is_some_and(|d| magic_div(d).is_some()) {
                    0 // the magic path reads the integer only
                } else if int_b {
                    let x = c.int_src(ins.b, 10);
                    c.a.scvtf_w(0, x);
                    0
                } else if cb == Cls::Num && const_div.is_some() {
                    c.fetch(ins.b, 0) // raw home: the constant path dispatches on the tag
                } else if cb == Cls::Num {
                    c.fetch_x(ins.b, 8);
                    c.any_to_dbl(8, 0);
                    0
                } else {
                    load_dbl(c, cb, ins.b, 0)
                };
                if let Some(d) = const_div {
                    let int_in = int_b.then(|| c.int_src(ins.b, 10));
                    emit_mod_const(c, ins.a, db, d, fmod_addr, int_in, pc);
                    return;
                }
                let dc = if !smi {
                    c.fetch(ins.c, 1)
                } else {
                    let cc = cls_of(facts, pc, 1, smi);
                    if cc == Cls::Num {
                        c.fetch_x(ins.c, 9);
                        c.any_to_dbl(9, 1);
                        1
                    } else {
                        load_dbl(c, cc, ins.c, 1)
                    }
                };
                emit_mod_num(c, ins.a, db, dc, fmod_addr);
                c.fix_int_write(ins.a, pc);
            } else {
                let slow = c.a.new_label();
                let done = c.a.new_label();
                c.fetch_x(ins.b, 8);
                if smi {
                    c.guard_numeric(8, slow);
                } else {
                    c.guard_number(8, slow);
                }
                if let Some(d) = const_div {
                    // divisor is a compile-time integer: no fetch, no
                    // guard, no runtime integer round-trip for it (raw
                    // bits in d0: the constant path dispatches on the tag)
                    c.a.fmov_dx(0, 8);
                    emit_mod_const(c, ins.a, 0, d, fmod_addr, None, pc);
                } else {
                    c.fetch_x(ins.c, 9);
                    if smi {
                        c.guard_numeric(9, slow);
                        c.any_to_dbl(8, 0);
                        c.any_to_dbl(9, 1);
                    } else {
                        c.guard_number(9, slow);
                        c.a.fmov_dx(0, 8);
                        c.a.fmov_dx(1, 9);
                    }
                    emit_mod_num(c, ins.a, 0, 1, fmod_addr);
                }
                c.a.b(done);
                c.a.bind(slow);
                c.step_full(pc);
                // the fast path left the result in an integer register
                // too; the helper wrote only the home
                c.fix_int_write(ins.a, pc);
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
            if dbl_b {
                let db = c.fetch(ins.b, 0);
                let dst = if ins.a < LOW { (8 + ins.a) as u32 } else { 0 };
                c.a.fneg(dst, db);
                if ins.a >= LOW {
                    c.a.str_d_imm(dst, R_SLOTS, C::slot(ins.a));
                }
            } else {
                // an int negates as a double here: -0 and i32::MIN make
                // the int form wrong, and Neg is rare
                let slow = c.a.new_label();
                let done = c.a.new_label();
                c.fetch_x(ins.b, 8);
                c.guard_numeric(8, slow);
                c.any_to_dbl(8, 0);
                c.a.fneg(0, 0);
                c.put(ins.a, 0);
                c.a.b(done);
                c.a.bind(slow);
                c.step_full(pc);
                c.a.bind(done);
            }
            c.fix_int_write(ins.a, pc);
        }
        Op::BitAnd | Op::BitOr | Op::BitXor | Op::Shl | Op::Shr | Op::UShr | Op::BitNot => {
            let unary = ins.op == Op::BitNot;
            let proven = if unary { num_b } else { num_bc };
            let slow = c.a.new_label();
            let done = c.a.new_label();
            // x{x} = ToInt32 of the operand: an int outright, a double
            // through fcvtzs (out-of-range doubles are the interpreter's)
            let to_i32 = |c: &mut C, cls: Cls, v: u8, x: u32, d: u32| match cls {
                Cls::IntK(k) => c.a.mov_imm64(x, k as u64),
                Cls::Int if c.smi => {
                    // lane / intermediate register first: the home is a
                    // 10-cycle round trip (`(s * 31 + i) & m` ran 3x slower)
                    let r = load_int(c, cls, v, x);
                    if r != x {
                        c.a.mov(x, r);
                    }
                }
                Cls::Dbl | Cls::Int => {
                    let dv = c.fetch(v, d);
                    c.a.fcvtzs(x, dv);
                }
                _ => {
                    c.fetch_x(v, x);
                    if c.smi {
                        let dbl = c.a.new_label();
                        let have = c.a.new_label();
                        c.a.lsr_imm(12, x, 48);
                        c.a.cmp_reg(12, R_TAGLIM);
                        c.a.b_cond(Cond::Ne, dbl);
                        c.unbox_int(x);
                        c.a.b(have);
                        c.a.bind(dbl);
                        c.a.fmov_dx(d, x);
                        c.a.fcvtzs(x, d);
                        c.a.bind(have);
                    } else {
                        c.a.fmov_dx(d, x);
                        c.a.fcvtzs(x, d);
                    }
                }
            };
            let (cb, cc) = (cls_of(facts, pc, 0, smi), cls_of(facts, pc, 1, smi));
            if proven {
                to_i32(c, cb, ins.b, 10, 0);
                if !unary {
                    to_i32(c, cc, ins.c, 11, 1);
                }
            } else {
                c.fetch_x(ins.b, 8);
                c.guard_numeric(8, slow);
                if !unary {
                    c.fetch_x(ins.c, 9);
                    c.guard_numeric(9, slow);
                }
                to_i32(c, if unary { Cls::Num } else { cb.min_num() }, ins.b, 10, 0);
                if !unary {
                    to_i32(c, cc.min_num(), ins.c, 11, 1);
                }
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
            if smi && !unsigned && c.lane_on && in_lane {
                // a laned / intermediate destination stays in its register
                c.int_dst_w(ins.a, 10, pc);
            } else if smi && !unsigned {
                c.box_int(10);
                c.put_x(ins.a, 10);
            } else if smi {
                // ToUint32: an int when it fits, else the double
                let big = c.a.new_label();
                let stored = c.a.new_label();
                c.a.cmp_imm(10, 0); // 64-bit view of the zero-extended w10
                c.a.mov_imm64(12, i32::MAX as u64);
                c.a.cmp_reg(10, 12);
                c.a.b_cond(Cond::Hi, big);
                c.box_int(10);
                c.put_x(ins.a, 10);
                c.a.b(stored);
                c.a.bind(big);
                c.a.ucvtf_w(dst, 10);
                c.put(ins.a, dst);
                c.a.bind(stored);
                c.fix_int_write(ins.a, pc);
            } else {
                if unsigned {
                    c.a.ucvtf_w(dst, 10);
                } else {
                    c.a.scvtf_w(dst, 10);
                }
                if ins.a >= LOW {
                    c.a.str_d_imm(dst, R_SLOTS, C::slot(ins.a));
                }
            }
            c.a.b(done);
            c.a.bind(slow);
            if !proven {
                c.step_full(pc);
            }
            c.a.bind(done);
        }

        Op::Eq | Op::Ne | Op::Lt | Op::Le | Op::Gt | Op::Ge => {
            let (cb, cc) = (cls_of(facts, pc, 0, smi), cls_of(facts, pc, 1, smi));
            let is_int = |x: Cls| matches!(x, Cls::Int | Cls::IntK(_));
            let is_dbl = |x: Cls| matches!(x, Cls::Dbl | Cls::IntK(_));
            if smi && is_int(cb) && is_int(cc) {
                let ib = load_int(c, cb, ins.b, 10);
                let ic = load_int(c, cc, ins.c, 11);
                c.a.cmp_reg32(ib, ic);
                c.a.cset(8, int_cond(ins.op));
                c.a.mov_imm64(9, Value::FALSE.bits());
                c.a.add_reg(8, 9, 8);
                c.put_x(ins.a, 8);
            } else if dbl_bc || (smi && is_dbl(cb) && is_dbl(cc)) {
                let db = load_dbl(c, cb, ins.b, 0);
                let dc = load_dbl(c, cc, ins.c, 1);
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
                c.guard_numeric(8, slow);
                c.guard_numeric(9, slow);
                c.any_to_dbl(8, 0);
                c.any_to_dbl(9, 1);
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
            let skip_to = c.exit_label(pc, pc + 2);
            // `Skip; Jump T` (the loop-exit idiom): one branch on the
            // inverted condition straight to T, no taken branch on the
            // loop path
            let fuse = c.fuse;
            let skip = |c: &mut C, cond: Cond| match fuse {
                Some(t) => c.a.b_cond(cond.invert(), t),
                None => c.a.b_cond(cond, skip_to),
            };
            let (mut cb, mut cc) = (cls_of(facts, pc, 0, smi), cls_of(facts, pc, 1, smi));
            let is_int = |x: Cls| matches!(x, Cls::Int | Cls::IntK(_));
            let is_dbl = |x: Cls| matches!(x, Cls::Dbl | Cls::IntK(_));
            // an operand the lane machinery holds as an integer
            let laned = |c: &C, v: u8| {
                c.lane_on && in_lane && (c.tmp_reg(v).is_some() || c.int_ok.contains(&v))
            };
            if smi && cb == Cls::Num && laned(c, ins.b) {
                cb = Cls::Int;
            }
            if smi && cc == Cls::Num && laned(c, ins.c) {
                cc = Cls::Int;
            }
            if smi && is_int(cb) && is_int(cc) {
                let ib = load_int(c, cb, ins.b, 10);
                let ic = load_int(c, cc, ins.c, 11);
                c.a.cmp_reg32(ib, ic);
                skip(c, int_cond(ins.op));
            } else if dbl_bc || (smi && is_dbl(cb) && is_dbl(cc)) {
                let db = load_dbl(c, cb, ins.b, 0);
                let dc = load_dbl(c, cc, ins.c, 1);
                c.a.fcmp(db, dc);
                skip(c, js_cond(ins.op));
            } else if smi && cb != Cls::Other && cc != Cls::Other {
                // numeric, representation unknown: ints compare as ints,
                // any other mix as doubles. A side proven int needs no
                // tag test (`n % d === 0`: one test, not two).
                let as_dbl = c.a.new_label();
                let done = c.a.new_label();
                let side = |c: &mut C, cls: Cls, v: u8, x: u32| -> u32 {
                    if is_int(cls) {
                        load_int(c, cls, v, x)
                    } else {
                        c.fetch_x(v, x);
                        c.a.lsr_imm(12, x, 48);
                        c.a.cmp_reg(12, R_TAGLIM);
                        c.a.b_cond(Cond::Ne, as_dbl);
                        x
                    }
                };
                let xb = side(c, cb, ins.b, 8);
                let xc = side(c, cc, ins.c, 9);
                c.a.cmp_reg32(xb, xc);
                skip(c, int_cond(ins.op));
                c.a.b(done);
                c.a.bind(as_dbl);
                // x8/x9 hold the raw values of the Num sides; int sides
                // reload as doubles
                let db = if is_int(cb) { load_dbl(c, cb, ins.b, 0) } else { c.any_to_dbl(8, 0); 0 };
                let dc = if is_int(cc) { load_dbl(c, cc, ins.c, 1) } else { c.any_to_dbl(9, 1); 1 };
                c.a.fcmp(db, dc);
                skip(c, js_cond(ins.op));
                c.a.bind(done);
            } else {
                let slow = c.a.new_label();
                let done = c.a.new_label();
                c.fetch_x(ins.b, 8);
                c.fetch_x(ins.c, 9);
                c.guard_numeric(8, slow);
                c.guard_numeric(9, slow);
                c.any_to_dbl(8, 0);
                c.any_to_dbl(9, 1);
                c.a.fcmp(0, 1);
                skip(c, js_cond(ins.op));
                c.a.b(done);
                c.a.bind(slow);
                c.step_full(pc);
                match fuse {
                    Some(t) => c.a.cbz(0, t), // ctrl=0: comparison false -> the jump
                    None => c.a.cbnz(0, skip_to), // ctrl=1: comparison true -> skip
                }
                c.a.bind(done);
            }
            c.skip_next = fuse.is_some();
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
                c.cmp_sentinel(0);
                c.a.b_cond(Cond::Eq, c.bail);
                c.a.add_reg(R_SLOTS, 1, R_BASE);
                c.reload_low();
                c.zero_cache(); // GC may have evacuated the cached object
                c.a.movz(R_POLL, SAFEPOINT_INTERVAL, 0);
                c.a.bind(no_poll);
            }
            let t = c.exit_label(pc, target);
            c.a.b(t);
        }
        Op::JumpIfFalse | Op::JumpIfTrue => {
            let target = (pc as i64 + ins.sbx() as i64 + 1) as usize;
            let t = c.exit_label(pc, target);
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
                JCond::Int => {
                    c.fetch_x(ins.a, 8);
                    match ins.op {
                        Op::JumpIfFalse => c.a.cbz32(8, t),
                        _ => c.a.cbnz32(8, t),
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
                    c.guard_numeric(8, slow);
                    c.any_to_dbl(8, 0);
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
            if let (Some(o), Some(Some((pw, callee, lits)))) =
                (c.offsets, inlines.get(pc))
            {
                if callee.arity as usize == ins.b as usize
                    && (ins.a as usize + 1 + callee.body().n_regs as usize) < 256
                {
                    emit_inline_call(
                        c, pc, ins, *pw, callee, lits, &o, fmod_addr, inline_generic,
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
                c.a.movz(11, tsr_memory::TAG_CLOSURE as u16, 0); // TAG_CLOSURE
                c.a.cmp_reg(10, 11);
                c.a.b_cond(Cond::Ne, slow);
                // per-pc IC entry
                c.a.mov_imm64(13, c.tics_base + (pc as u64) * 8);
                c.a.ldr_imm(13, 13, 0);
                c.a.cbz(13, slow);
                // proto identity: the proto pointer in the closure cell
                c.unbox(12, 9); // x12 = closure address
                c.a.ldr_imm(15, 12, o.closure_proto);
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
                // the callee's CURRENT code, through its proto: the IC's
                // baked pointer went stale when the callee recompiled
                // (withdrawn speculation, demotion) and every call kept
                // deopting through the old code
                c.a.ldr_imm(16, 1, JIT_CODE_OFF);
                c.a.cbz(16, slow);
                c.a.blr(16);
                // refresh slots base (callee frames may have grown the stack)
                c.a.ldr_imm(16, R_REALM, o.realm_stack_ptr);
                c.a.add_reg(R_SLOTS, 16, R_BASE);
                // the callee's own safepoints may have moved young cells:
                // drop the cached object / array addresses
                c.zero_cache();
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
                c.a.ldr_imm(5, R_SLOTS, C::slot(ins.a));
                c.unbox(5, 5); // closure address
                let d = c.depth(6);
                c.a.add_imm(6, d, 1);
                c.thin(c.helpers.call_resume);
                c.a.str_imm(0, R_SLOTS, C::slot(ins.a));
                c.a.b(done);
            }
            c.a.bind(slow);
            c.a.mov(0, R_REALM);
            c.proto_into(1);
            c.a.mov_imm64(2, pc as u64);
            c.a.mov(3, R_BASE);
            c.a.mov_imm64(4, ins.a as u64);
            c.a.mov_imm64(5, ins.b as u64);
            let d = c.depth(6);
            c.a.mov(6, d);
            c.a.mov_imm64(8, c.helpers.call as u64);
            c.a.blr(8);
            c.cmp_sentinel(0);
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
            // a typed Int32 read is an int-tagged value to what follows;
            // the field may hold an integral double (the shape's Int32
            // is a value property), so normalize
            let typed_pc = smi && facts.field_repr.get(pc).copied().unwrap_or(2) == 0;
            // inside a laned loop the int also lands in the intermediate
            // register: its consumer reads that instead of round-tripping
            // the value through the d-home (objects: three reads a pass)
            let mut int_tmp = typed_pc && c.lane_on && in_lane && c.lane_of(ins.a).is_none();
            // Take a free lane register instead of R_ITMP when R_ITMP is
            // holding a live arithmetic result: `acc + p.x + p.y + p.z`
            // has three integers live at once, and evicting the partial
            // sum boxed it to its home and reloaded it on the next read.
            // R_ITMP holds a sum something still reads: evicting it for
            // this field costs a box plus a reload on the loop-carried
            // chain, more than the one move keeping the field there would
            // save. Leave the field in its home and keep the sum.
            // `acc + p.x + p.y + p.z` evicted a partial sum on each of
            // its last two reads and ran 2.4x slower than node for it.
            if int_tmp && c.itmp != Some(ins.a) && c.itmp_live_at(pc + 1).is_some() {
                int_tmp = false;
            }
            if int_tmp {
                c.retire_itmp(pc + 1, Some(ins.a));
            }
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
                        c.cmp_sid(16, sid);
                        c.a.b_cond(Cond::Ne, full);
                        c.a.ldr_imm(8, 15, o.obj_inline + slot * 8);
                    } else {
                        c.a.mov_imm64(13, c.ics_base + (pc as u64) * 8);
                            c.a.ldr_imm(14, 13, 0);
                        c.a.lsr_imm(13, 14, 32);
                        c.a.cmp_reg(13, 16);
                        c.a.b_cond(Cond::Ne, full);
                        c.a.sub_imm32(13, 14, 1);
                        c.a.cmp_imm(13, o.obj_inline_n);
                        c.a.b_cond(Cond::Hs, full);
                        c.index_addr(17, 15, 13, 8);
                        c.a.ldr_imm(8, 17, o.obj_inline);
                    }
                    if int_tmp {
                        c.a.mov(R_ITMP, 8);
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
                    // a typed read (the analysis took this field's Int32 /
                    // Number representation as the result type) must not
                    // fall to the generic path on a miss: what follows
                    // relies on the type. Deopt and let the interpreter
                    // re-run it; ten of those re-tier the function.
                    let typed = facts.field_repr.get(pc).copied().unwrap_or(2) != 2;
                    let miss = if typed { c.a.new_label() } else { generic };
                    c.fetch_x(ins.b, 8);
                    c.a.lsr_imm(10, 8, 48);
                    c.a.movz(11, tsr_memory::TAG_OBJ as u16, 0); // TAG_OBJ
                    c.a.cmp_reg(10, 11);
                    c.a.b_cond(Cond::Ne, miss);
                    c.unbox(10, 8);
                    c.a.ldr_imm(11, 10, o.obj_shape);
                    c.a.ldr_w_imm(12, 11, o.shape_id);
                    c.cmp_sid(12, sid);
                    c.a.b_cond(Cond::Ne, miss);
                    c.a.mov(15, 10); // CSE cache: validated address
                    c.a.mov(16, 12); //            + shape id
                    c.a.ldr_imm(8, 10, o.obj_inline + slot * 8);
                    if int_tmp {
                        c.a.mov(R_ITMP, 8);
                    }
                    c.put_x(ins.a, 8);
                    c.a.b(done);
                    if typed {
                        c.a.bind(miss);
                        c.deopt_at(pc);
                    }
                    c.a.bind(generic);
                }
                // inline IC'd property load (reads only, no GC)
                c.fetch_x(ins.b, 8);
                c.a.lsr_imm(10, 8, 48);
                c.a.movz(11, tsr_memory::TAG_OBJ as u16, 0); // TAG_OBJ
                c.a.cmp_reg(10, 11);
                c.a.b_cond(Cond::Ne, slow);
                c.unbox(10, 8);
                c.a.ldr_imm(11, 10, o.obj_shape);
                c.a.ldr_w_imm(12, 11, o.shape_id);
                c.a.mov_imm64(13, c.ics_base + (pc as u64) * 8);
                    c.a.ldr_imm(14, 13, 0);
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
                if int_tmp {
                    c.a.mov(R_ITMP, 8);
                }
                c.put_x(ins.a, 8);
                c.a.b(done);
            } else {
                c.a.bind(full);
            }
            c.a.bind(slow);
            c.a.mov(0, R_REALM);
            c.proto_into(1);
            c.a.mov_imm64(2, pc as u64);
            c.fetch_x(ins.b, 3);
            c.a.mov_imm64(4, ins.c as u64);
            c.thin_keep_arrays(c.helpers.get_field);
            if int_tmp {
                c.a.mov(R_ITMP, 0);
            }
            c.put_x(ins.a, 0);
            c.a.bind(done);
            if int_tmp {
                c.itmp = Some(ins.a);
                c.itmp_dirty = false;
            } else if c.itmp == Some(ins.a) {
                c.itmp = None;
                c.itmp_dirty = false;
            }
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
                // an Int32 field takes only exact i32 values: a store of a
                // number not proven integral is checked inline, and a
                // miss goes to the helper, which widens the shape
                let int32_field = facts.field_repr.get(pc).copied().unwrap_or(2) == 0
                    && !facts.int_facts.get(pc).is_some_and(|f| f[2])
                    && c.lane_of(ins.c).is_none()
                    && c.itmp != Some(ins.c);
                // a non-number may be stored inline only where the field is
                // known to admit anything: a baked site whose repr is Any
                let ref_ok = facts.ic_baked.get(pc).copied().flatten().is_some()
                    && facts.field_repr.get(pc).copied().unwrap_or(2) == 2;
                // a value the lane already holds as an integer is a number
                let val_int = c.lane_of(ins.c).is_some()
                    || c.itmp == Some(ins.c)
                    || (in_lane && c.int_ok.contains(&ins.c));
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
                    if !val_int {
                        if ref_ok {
                            c.fetch_x(ins.a, 8); // the barrier check needs the ref
                        }
                        c.ref_store_check(9, 8, ref_ok, o, full);
                    }
                    if int32_field {
                        c.int32_check(9, full);
                    }
                    if let Some((sid, slot)) = baked {
                        c.cmp_sid(16, sid);
                        c.a.b_cond(Cond::Ne, full);
                        c.a.str_imm(9, 15, o.obj_inline + slot * 8);
                    } else {
                        c.a.mov_imm64(13, c.ics_base + (pc as u64) * 8);
                            c.a.ldr_imm(14, 13, 0);
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
                    c.a.movz(11, tsr_memory::TAG_OBJ as u16, 0);
                    c.a.cmp_reg(10, 11);
                    c.a.b_cond(Cond::Ne, generic);
                    c.fetch_x(ins.c, 9);
                    if !val_int {
                        c.ref_store_check(9, 8, ref_ok, o, generic);
                    }
                    if int32_field {
                        c.int32_check(9, generic);
                    }
                    c.unbox(10, 8);
                    c.a.ldr_imm(11, 10, o.obj_shape);
                    c.a.ldr_w_imm(12, 11, o.shape_id);
                    c.cmp_sid(12, sid);
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
                c.a.movz(11, tsr_memory::TAG_OBJ as u16, 0); // TAG_OBJ
                c.a.cmp_reg(10, 11);
                c.a.b_cond(Cond::Ne, slow);
                c.fetch_x(ins.c, 9);
                c.guard_number(9, slow); // heap values -> helper (barrier)
                c.unbox(10, 8);
                c.a.ldr_imm(11, 10, o.obj_shape);
                c.a.ldr_w_imm(12, 11, o.shape_id);
                c.a.mov_imm64(13, c.ics_base + (pc as u64) * 8);
                    c.a.ldr_imm(14, 13, 0);
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
            c.proto_into(1);
            c.a.mov_imm64(2, pc as u64);
            c.fetch_x(ins.a, 3);
            c.a.mov_imm64(4, ins.b as u64);
            c.fetch_x(ins.c, 5);
            c.thin_keep_arrays(c.helpers.set_field);
            c.a.bind(done);
            if c.offsets.is_some() {
                *fcache = Some(ins.a);
            }
        }
        Op::GetIndex => {
            let slow = c.a.new_label();
            let done = c.a.new_label();
            // a site specialized to an element kind must not fall back to
            // the helper: an out-of-bounds read hands back undefined,
            // which is not the type the consumers were compiled for
            let ek = facts.elem_kind.get(pc).copied().unwrap_or(EK_ANY);
            if let Some(o) = c.offsets {
                // An index the lane already holds as an integer needs no
                // tag guard and no integrality check: it is a sign-extended
                // i32, so a negative one fails the unsigned bounds compare.
                let const_ix = facts
                    .const_ops
                    .get(pc)
                    .and_then(|o| o[1])
                    .filter(|&k| (0..=i32::MAX as i64).contains(&k));
                let lane_ix = in_lane && (c.lane_of(ins.c).is_some() || c.itmp == Some(ins.c));
                // proven an i32 at this loop's header and kept one by every
                // write since: the boxed home converts bare
                let int_ix = in_lane && !lane_ix && c.int_ok.contains(&ins.c);
                if !lane_ix && !int_ix && const_ix.is_none() {
                    // the index guard clobbers x10, so it has to run before
                    // the base pointer lands there
                    c.fetch_x(ins.c, 9);
                    c.guard_numeric(9, slow);
                }
                let reuse = *acache == Some(ins.b) && c.int_lane;
                let cache = c.acache_on && primary.is_none_or(|p| p == ins.b);
                c.array_base(10, ins.b, slow, reuse, cache);
                if cache {
                    *acache = Some(ins.b);
                }
                if let Some(k) = const_ix {
                    c.a.mov_imm64(13, k as u64);
                } else if lane_ix {
                    let r = c.lane_of(ins.c).unwrap_or(R_ITMP);
                    c.a.sxtw(13, r);
                } else if int_ix {
                    if smi {
                        c.fetch_x(ins.c, 13);
                        c.unbox_int(13);
                    } else {
                        let d = c.fetch(ins.c, 0);
                        c.a.fcvtzs(13, d);
                    }
                } else if smi {
                    let have = c.a.new_label();
                    let dbl = c.a.new_label();
                    c.a.lsr_imm(11, 9, 48);
                    c.a.cmp_reg(11, R_TAGLIM);
                    c.a.b_cond(Cond::Ne, dbl);
                    c.a.sxtw(13, 9);
                    c.a.b(have);
                    c.a.bind(dbl);
                    c.a.fmov_dx(0, 9);
                    c.a.fcvtzs(13, 0);
                    c.a.scvtf(1, 13);
                    c.a.fcmp(1, 0);
                    c.a.b_cond(Cond::Ne, slow);
                    c.a.bind(have);
                } else {
                    c.a.fmov_dx(0, 9);
                    c.a.fcvtzs(13, 0);
                    c.a.scvtf(1, 13);
                    c.a.fcmp(1, 0);
                    c.a.b_cond(Cond::Ne, slow);
                }
                if ek != EK_ANY {
                    c.arr_kind_guard(10, ek, slow);
                }
                c.arr_len(14, 10);
                c.a.cmp_reg(13, 14);
                c.a.b_cond(Cond::Hs, slow);
                c.arr_vals(16, 10, &o);
                c.a.ldr_reg_lsl3(8, 16, 13);
                c.put_x(ins.a, 8);
                c.a.b(done);
            }
            c.a.bind(slow);
            if ek != EK_ANY {
                c.deopt_at(pc);
            } else {
                c.a.mov(0, R_REALM);
                c.proto_into(1);
                c.a.mov_imm64(2, pc as u64);
                c.fetch_x(ins.b, 3);
                c.fetch_x(ins.c, 4);
                c.thin_keep_arrays(c.helpers.get_index);
                c.put_x(ins.a, 0);
            }
            c.a.bind(done);
        }
        Op::SetIndex => {
            let slow = c.a.new_label();
            let done = c.a.new_label();
            if let Some(o) = c.offsets {
                // In-bounds store, any value. The helper's other job is
                // the write barrier, which does nothing for a young array
                // or one that is not old yet, and nothing again once an
                // old array is dirty (its whole contents are rescanned at
                // the next minor). Only the first store into a clean old
                // array — the one that must record it — takes the helper.
                // Growth (i == len) takes it too; that one allocates.
                let store = c.a.new_label();
                c.fetch_x(ins.a, 8);
                c.a.lsr_imm(10, 8, 48);
                c.a.movz(11, tsr_memory::TAG_ARR as u16, 0); // TAG_ARR
                c.a.cmp_reg(10, 11);
                c.a.b_cond(Cond::Ne, slow);
                // the index stays a boxed value in x9 (or the lane) until
                // the base is derived: arena_base stages the young bit in
                // x13, and the number guard clobbers x10
                let const_ix = facts
                    .const_ops
                    .get(pc)
                    .and_then(|o| o[0])
                    .filter(|&k| (0..=i32::MAX as i64).contains(&k));
                let lane_ix = in_lane && (c.lane_of(ins.b).is_some() || c.itmp == Some(ins.b));
                if !lane_ix && const_ix.is_none() {
                    c.fetch_x(ins.b, 9);
                    c.guard_numeric(9, slow);
                }
                c.unbox(10, 8);
                c.young_test(10, 11, &o, store);
                // aged and clean -> helper records it
                c.a.ldr_imm(14, 10, 0);
                c.a.movz(17, (tsr_memory::cells::M_AGED | tsr_memory::cells::M_DIRTY) as u16, 0);
                c.a.and_reg(14, 14, 17);
                c.a.cmp_imm(14, tsr_memory::cells::M_AGED as u32);
                c.a.b_cond(Cond::Eq, slow);
                c.a.bind(store);
                if let Some(k) = const_ix {
                    c.a.mov_imm64(13, k as u64);
                } else if lane_ix {
                    let r = c.lane_of(ins.b).unwrap_or(R_ITMP);
                    c.a.sxtw(13, r);
                } else if smi {
                    let have = c.a.new_label();
                    let dbl = c.a.new_label();
                    c.a.lsr_imm(11, 9, 48);
                    c.a.cmp_reg(11, R_TAGLIM);
                    c.a.b_cond(Cond::Ne, dbl);
                    c.a.sxtw(13, 9);
                    c.a.b(have);
                    c.a.bind(dbl);
                    c.a.fmov_dx(0, 9);
                    c.a.fcvtzs(13, 0);
                    c.a.scvtf(1, 13);
                    c.a.fcmp(1, 0);
                    c.a.b_cond(Cond::Ne, slow);
                    c.a.bind(have);
                } else {
                    c.a.fmov_dx(0, 9);
                    c.a.fcvtzs(13, 0);
                    c.a.scvtf(1, 13);
                    c.a.fcmp(1, 0);
                    c.a.b_cond(Cond::Ne, slow);
                }
                c.a.ldr_imm(12, 10, 0); // meta: kind low, length high
                c.a.lsr_imm(14, 12, 32);
                c.a.cmp_reg(13, 14);
                c.a.b_cond(Cond::Hs, slow); // growth or out of range
                c.fetch_x(ins.c, 9);
                // a store the array's element kind does not admit goes to
                // the helper, which widens it. An integer needs no check:
                // every array kind admits one.
                let val_int = facts.int_facts.get(pc).is_some_and(|f| f[2])
                    || c.lane_of(ins.c).is_some()
                    || c.itmp == Some(ins.c);
                if !val_int {
                    c.arr_store_ok(12, 9, slow);
                }
                c.arr_vals(17, 10, &o);
                c.a.str_reg_lsl3(9, 17, 13);
                c.a.b(done);
            }
            c.a.bind(slow);
            c.a.mov(0, R_REALM);
            c.proto_into(1);
            c.a.mov_imm64(2, pc as u64);
            c.fetch_x(ins.a, 3);
            c.fetch_x(ins.b, 4);
            c.fetch_x(ins.c, 5);
            c.thin(c.helpers.set_index);
            c.a.bind(done);
        }
        Op::Len => {
            let slow = c.a.new_label();
            let done = c.a.new_label();
            if c.offsets.is_some() {
                let reuse = *acache == Some(ins.b) && c.int_lane;
                let cache = c.acache_on && primary.is_none_or(|p| p == ins.b);
                c.array_base(10, ins.b, slow, reuse, cache);
                if cache {
                    *acache = Some(ins.b);
                }
                c.arr_len(14, 10);
                if smi {
                    c.box_int(14);
                    c.put_x(ins.a, 14);
                } else {
                    c.a.scvtf(0, 14);
                    c.put(ins.a, 0);
                }
                c.a.b(done);
            }
            c.a.bind(slow);
            c.a.mov(0, R_REALM);
            c.proto_into(1);
            c.a.mov_imm64(2, pc as u64);
            c.fetch_x(ins.b, 3);
            c.thin_keep_arrays(c.helpers.len);
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
                // young check needs the value; the slot address comes from
                // the cache when this is the loop's primary array (the
                // helper slow path zeroes x22, and growth keeps the slot)
                let reuse = *acache == Some(ins.a) && c.int_lane;
                let cache = c.acache_on && primary.is_none_or(|p| p == ins.a);
                c.array_base(10, ins.a, slow, reuse, cache);
                if cache {
                    *acache = Some(ins.a);
                }
                let young = c.a.new_label();
                c.young_test(10, 13, &o, young);
                c.a.b(slow); // aged array -> helper (barrier)
                c.a.bind(young);
                c.a.ldr_imm(12, 10, 0); // meta: len in the high word
                c.a.lsr_imm(13, 12, 32);
                c.a.ldr_imm(17, 10, o.arr_elems);
                c.a.ldr_imm(14, 17, 0); // chunk meta: cap in the high word
                c.a.lsr_imm(14, 14, 32);
                c.a.cmp_reg(13, 14);
                c.a.b_cond(Cond::Hs, slow); // full -> helper (regrow)
                c.a.add_imm(17, 17, o.elems_vals);
                c.fetch_x(ins.b, 9);
                // the pushed value must suit the array's element kind
                // (x12 already holds the meta word); the helper widens
                if !(facts.int_facts.get(pc).is_some_and(|f| f[1])
                    || c.lane_of(ins.b).is_some()
                    || c.itmp == Some(ins.b))
                {
                    c.arr_store_ok(12, 9, slow);
                }
                c.a.str_reg_lsl3(9, 17, 13);
                c.a.mov_imm64(14, 1u64 << 32);
                c.a.add_reg(12, 12, 14);
                c.a.str_imm(12, 10, 0);
                c.a.b(done);
            }
            c.a.bind(slow);
            c.a.mov(0, R_REALM);
            c.proto_into(1);
            c.a.mov_imm64(2, pc as u64);
            c.fetch_x(ins.a, 3);
            c.fetch_x(ins.b, 4);
            c.thin(c.helpers.push);
            c.a.bind(done);
        }
        Op::GetGlobal => {
            c.a.mov(0, R_REALM);
            c.proto_into(1);
            c.a.mov_imm64(2, pc as u64);
            c.a.mov_imm64(3, ins.bx() as u64);
            c.thin_keep_arrays(c.helpers.get_global);
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
                    let cl = c.closure(9);
                    c.a.ldr_imm(8, cl, o.closure_upvals + ins.b as u32 * 8);
                    if is_cell {
                        c.unbox(9, 8);
                        c.a.ldr_imm(8, 9, o.cell_val);
                    }
                    c.put_x(ins.a, 8);
                }
                _ => {
                    c.a.mov(0, R_REALM);
                    let cl = c.closure(1);
                    c.a.mov(1, cl);
                    c.a.mov_imm64(2, ins.b as u64);
                    c.thin_keep_arrays(c.helpers.get_upval);
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
            c.thin_keep_arrays(c.helpers.set_upval);
        }
        Op::LoadCell => {
            c.a.mov(0, R_REALM);
            c.proto_into(1);
            c.a.mov_imm64(2, pc as u64);
            c.fetch_x(ins.b, 3);
            c.thin_keep_arrays(c.helpers.load_cell);
            c.put_x(ins.a, 0);
        }
        Op::StoreCell => {
            c.a.mov(0, R_REALM);
            c.proto_into(1);
            c.a.mov_imm64(2, pc as u64);
            c.fetch_x(ins.a, 3);
            c.fetch_x(ins.b, 4);
            c.thin_keep_arrays(c.helpers.store_cell);
        }
        Op::NewObject => {
            c.a.mov(0, R_REALM);
            c.thin_keep_arrays(c.helpers.new_object);
            c.put_x(ins.a, 0);
        }
        Op::NewArray => {
            c.a.mov(0, R_REALM);
            c.a.mov_imm64(1, ins.b as u64);
            c.thin(c.helpers.new_array);
            c.put_x(ins.a, 0);
        }
        Op::NewCell => {
            let slow = c.a.new_label();
            let done = c.a.new_label();
            if let Some(o) = c.offsets {
                use tsr_memory::cells::{meta, K_CELL};
                c.bump_old(2, tsr_memory::TAG_CELL, &o, slow); // TAG_CELL
                c.a.mov_imm64(14, meta(K_CELL, 2, 0));
                c.a.str_imm(14, 13, 0);
                c.fetch_x(ins.a, 9);
                c.a.str_imm(9, 13, o.cell_val);
                c.a.b(done); // x0 = the boxed cell
            }
            c.a.bind(slow);
            c.a.mov(0, R_REALM);
            c.fetch_x(ins.a, 1);
            c.thin_keep_arrays(c.helpers.new_cell);
            c.a.bind(done);
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
            if ins.op == Op::NewArrayLit {
                let kind = lit_arr_kind(
                    facts.lit_vals.get(pc).map(|v| v.as_slice()).unwrap_or(&[]),
                    n,
                );
                c.emit_arr_lit(ins.a, ins.b, n, kind);
            } else if let Some(&(shape_ptr, sn)) = lit_shapes.get(pc).filter(|&&(sp, _)| sp != 0) {
                // which literal values need an Int32 check: the field is
                // Int32 (as of now — widening later only relaxes it) and
                // the value is a number not proven integral
                let checks: Vec<u8> = {
                    let shape = unsafe { &*(shape_ptr as *const tsr_memory::ShapeData) };
                    let classes = facts.lit_vals.get(pc).map(|v| v.as_slice()).unwrap_or(&[]);
                    (0..sn as usize)
                        .map(|j| {
                            let v = ins.b + j as u8;
                            // the loop's own int guard already proved it —
                            // possibly for the vreg this one was just copied
                            // from (the literal's operands are contiguous
                            // copies of the values)
                            let src = pbody.code[pc.saturating_sub(16)..pc]
                                .iter()
                                .rev()
                                .find(|i| writes_a_op(i.op) && i.a == v)
                                .and_then(|i| (i.op == Op::Move).then_some(i.b))
                                .unwrap_or(v);
                            let int_here = in_lane
                                && [v, src].iter().any(|&r| {
                                    c.lane_of(r).is_some() || c.itmp == Some(r) || c.int_ok.contains(&r)
                                });
                            (classes.get(j).copied().unwrap_or(2) == 1
                                && !int_here
                                && shape.repr(j) == tsr_memory::Repr::Int32) as u8
                        })
                        .collect()
                };
                c.emit_obj_lit(ins.a, ins.b, sn as u8, shape_ptr, &checks);
            } else {
                for v in ins.b..ins.b + n as u8 {
                    c.spill_vreg(v);
                }
                c.a.mov(0, R_REALM);
                c.proto_into(1);
                c.a.mov_imm64(2, pc as u64);
                c.a.mov(3, R_BASE);
                c.a.mov_imm64(4, ins.b as u64);
                c.a.mov_imm64(5, ins.c as u64);
                c.thin_keep_arrays(c.helpers.new_object_lit);
                c.put_x(ins.a, 0);
            }
        }

        Op::Closure => {
            let child = &pbody.protos[ins.bx() as usize];
            let o = c.offsets;
            c.emit_closure(
                ins.a,
                child,
                &|r| r,
                &|c: &mut C| c.closure(4),
                proto as *const FunctionProto as u64,
                pc,
                0,
                o,
            );
        }
        Op::Concat => {
            c.a.mov(0, R_REALM);
            c.fetch_x(ins.b, 1);
            c.fetch_x(ins.c, 2);
            c.thin_keep_arrays(c.helpers.concat);
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
            c.cmp_sentinel(0);
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
