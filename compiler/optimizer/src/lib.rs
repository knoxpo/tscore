//! tsc-optimizer
//!
//! Emit-time bytecode passes, run once per function before the proto is
//! sealed. One canonical bytecode: interpreter, Tier-1 and Tier-2 all see
//! the optimized form, and the golden differential suite verifies it.
//!
//! Passes (deliberately conservative — a skipped opportunity is fine, a
//! wrong rewrite is not):
//!   1. block-local copy propagation (rewrites reads through `Move`s)
//!   2. global dead-store elimination for pure defs (Move/Load*)
//!   3. loop-invariant constant hoisting into a preheader
//!
//! Jumps are decoded to absolute targets, passes edit an instruction list,
//! and re-encoding rebuilds relative offsets — so deletion/insertion is
//! safe. Skip-family ops ("skip next instruction") pin their successor:
//! nothing is ever inserted between a Skip and the following Jump, and the
//! Jump itself is control flow so no pass removes it.

use tsc_ir::{Const, Instr, Op, UpvalSrc};

/// Per-instruction operand effects: registers read / written.
/// `reads` may include a contiguous window (Call args, literal ops).
#[derive(Default, Clone)]
struct Effects {
    reads_buf: [u8; 16],
    n_reads: u8,
    /// A window read [start, start+len) too wide for the buffer.
    window: Option<(u8, u16)>,
    writes: Option<u8>,
    /// Has side effects beyond the register write (control, heap, throw).
    effectful: bool,
}

impl Effects {
    #[inline(always)]
    fn push_read(&mut self, r: u8) {
        if (self.n_reads as usize) < self.reads_buf.len() {
            self.reads_buf[self.n_reads as usize] = r;
            self.n_reads += 1;
        } else {
            // widen into a window covering everything (conservative)
            self.window = Some((0, 256));
        }
    }
    #[inline(always)]
    fn push_window(&mut self, start: u8, len: u16) {
        if len <= 16 - self.n_reads as u16 {
            for o in 0..len {
                self.reads_buf[self.n_reads as usize] = start + o as u8;
                self.n_reads += 1;
            }
        } else {
            self.window = Some((start, len));
        }
    }
    #[inline(always)]
    fn for_reads(&self, mut f: impl FnMut(u8)) {
        for i in 0..self.n_reads as usize {
            f(self.reads_buf[i]);
        }
        if let Some((start, len)) = self.window {
            for o in 0..len {
                let r = start as u16 + o;
                if r < 256 {
                    f(r as u8);
                }
            }
        }
    }
}

fn is_jump(op: Op) -> bool {
    matches!(op, Op::Jump | Op::JumpIfFalse | Op::JumpIfTrue)
}

fn is_skip(op: Op) -> bool {
    matches!(
        op,
        Op::EqSkip | Op::NeSkip | Op::LtSkip | Op::LeSkip | Op::GtSkip | Op::GeSkip
    )
}

/// Number of values a NewObjectLit reads (its Keys const length).
fn objlit_n(consts: &[Const], cidx: usize) -> usize {
    match consts.get(cidx) {
        Some(Const::Keys(ks)) => ks.len(),
        _ => 0,
    }
}

fn effects(ins: Instr, consts: &[Const], upval_srcs: &[Vec<UpvalSrc>]) -> Effects {
    use Op::*;
    let mut e = Effects::default();
    match ins.op {
        LoadConst | LoadInt | LoadBool | LoadNull | LoadUndef => {
            e.writes = Some(ins.a);
        }
        Move => {
            e.push_read(ins.b);
            e.writes = Some(ins.a);
        }
        Add | Sub | Mul | Div | Mod | Pow | BitAnd | BitOr | BitXor | Shl | Shr
        | UShr | Eq | Ne | Lt | Le | Gt | Ge | Concat | GetIndex => {
            e.push_read(ins.b);
            e.push_read(ins.c);
            e.writes = Some(ins.a);
            // arithmetic/compare can throw on bad types; Concat/GetIndex
            // allocate or throw
            e.effectful = true;
        }
        Neg | BitNot | Not | TypeOf | Len | GetField | LoadCell => {
            e.push_read(ins.b);
            e.writes = Some(ins.a);
            e.effectful = true;
        }
        EqSkip | NeSkip | LtSkip | LeSkip | GtSkip | GeSkip => {
            e.push_read(ins.b);
            e.push_read(ins.c);
            e.effectful = true;
        }
        Jump => e.effectful = true,
        JumpIfFalse | JumpIfTrue => {
            e.push_read(ins.a);
            e.effectful = true;
        }
        Call => {
            e.push_window(ins.a, ins.b as u16 + 1);
            e.writes = Some(ins.a);
            e.effectful = true;
        }
        Return | Halt => {
            e.push_read(ins.a);
            e.effectful = true;
        }
        Closure => {
            // reads the parent regs its child captures
            if let Some(srcs) = upval_srcs.get(ins.bx() as usize) {
                for u in srcs {
                    match *u {
                        UpvalSrc::ParentLocal(r) | UpvalSrc::ParentLocalValue(r) => {
                            e.push_read(r)
                        }
                        UpvalSrc::ParentUpval(_) => {}
                    }
                }
            }
            e.writes = Some(ins.a);
            e.effectful = true;
        }
        NewCell | Await => {
            e.push_read(ins.a);
            e.writes = Some(ins.a);
            e.effectful = true;
        }
        StoreCell | ArrayPush => {
            e.push_read(ins.a);
            e.push_read(ins.b);
            e.effectful = true;
        }
        SetUpval => {
            e.push_read(ins.b);
            e.effectful = true;
        }
        GetUpval | GetGlobal | NewObject | NewArray => {
            e.writes = Some(ins.a);
            e.effectful = true;
        }
        SetField => {
            e.push_read(ins.a);
            e.push_read(ins.c);
            e.effectful = true;
        }
        SetIndex => {
            e.push_read(ins.a);
            e.push_read(ins.b);
            e.push_read(ins.c);
            e.effectful = true;
        }
        NewObjectLit => {
            let n = objlit_n(consts, ins.c as usize) as u16;
            e.push_window(ins.b, n);
            e.writes = Some(ins.a);
            e.effectful = true;
        }
        NewArrayLit => {
            e.push_window(ins.b, ins.c as u16);
            e.writes = Some(ins.a);
            e.effectful = true;
        }
    }
    e
}

/// Reads that can be rewritten by copy-prop: plain b/c/a operand regs of
/// non-window ops. Window ops (Call, literal ops) and Skip successors are
/// left untouched.
fn rewritable_reads(op: Op) -> (bool, bool, bool) {
    use Op::*;
    match op {
        Move | Neg | BitNot | Not | TypeOf | Len | GetField | LoadCell => {
            (false, true, false)
        }
        Add | Sub | Mul | Div | Mod | Pow | BitAnd | BitOr | BitXor | Shl | Shr
        | UShr | Eq | Ne | Lt | Le | Gt | Ge | Concat | GetIndex | EqSkip | NeSkip
        | LtSkip | LeSkip | GtSkip | GeSkip => (false, true, true),
        JumpIfFalse | JumpIfTrue | Return => (true, false, false),
        SetField => (false, false, true),
        SetIndex => (false, true, true),
        ArrayPush | StoreCell => (false, true, false),
        SetUpval => (false, true, false),
        _ => (false, false, false),
    }
}

struct P {
    ins: Vec<Instr>,
    spans: Vec<u32>,
    /// Absolute jump target per instruction (jump family only).
    target: Vec<Option<usize>>,
}

/// Optimize one function's bytecode in place.
/// `upval_srcs[i]` are the capture descriptors of child proto i.
pub fn optimize(
    code: &mut Vec<Instr>,
    spans: &mut Vec<u32>,
    consts: &[Const],
    upval_srcs: &[Vec<UpvalSrc>],
    n_regs: u8,
    arity: u8,
) {
    if code.len() < 2 || code.len() != spans.len() {
        return;
    }
    // cheap pre-scan: tiny straight-line functions with few regs have
    // nothing to win — skip the whole pipeline (keeps cold compiles fast)
    let has_move = code.iter().any(|i| i.op == Op::Move);
    let has_back_jump = code
        .iter()
        .any(|i| i.op == Op::Jump && i.sbx() < 0);
    if !has_move && !has_back_jump && n_regs <= 8 {
        return;
    }
    let mut p = P {
        ins: code.clone(),
        spans: spans.clone(),
        target: code
            .iter()
            .enumerate()
            .map(|(pc, i)| {
                is_jump(i.op).then(|| (pc as i64 + i.sbx() as i64 + 1) as usize)
            })
            .collect(),
    };
    // out-of-range target (defensive): bail
    if p.target.iter().flatten().any(|&t| t > p.ins.len()) {
        return;
    }

    copy_prop(&mut p, consts, upval_srcs);
    sink_move_dst(&mut p, consts, upval_srcs, n_regs);
    dead_store_elim(&mut p, consts, upval_srcs, n_regs);
    hoist_loop_consts(&mut p, consts, upval_srcs, n_regs);
    renumber_regs(&mut p, consts, upval_srcs, n_regs, arity);

    // re-encode relative jumps
    for pc in 0..p.ins.len() {
        if let Some(t) = p.target[pc] {
            let sbx = t as i64 - pc as i64 - 1;
            if sbx < -32768 || sbx > 32767 {
                return; // give up: keep original code
            }
            let i = p.ins[pc];
            p.ins[pc] = Instr::asbx(i.op, i.a, sbx as i32);
        }
    }
    *code = p.ins;
    *spans = p.spans;
}

/// Basic-block leaders: 0, jump targets, and instruction after any
/// jump/skip pair.
fn leaders(p: &P) -> Vec<bool> {
    let n = p.ins.len();
    let mut l = vec![false; n + 1];
    l[0] = true;
    for pc in 0..n {
        if let Some(t) = p.target[pc] {
            l[t] = true;
            if pc + 1 <= n {
                l[pc + 1] = true;
            }
        }
        if is_skip(p.ins[pc].op) {
            // both successors (pc+1 executed or skipped) start blocks
            if pc + 1 <= n {
                l[pc + 1] = true;
            }
            if pc + 2 <= n {
                l[pc + 2] = true;
            }
        }
    }
    l
}

/// Pass 1: block-local copy propagation. After `Move a, b`, reads of `a`
/// become reads of `b` until either is redefined (within the block).
fn copy_prop(p: &mut P, consts: &[Const], upval_srcs: &[Vec<UpvalSrc>]) {
    let l = leaders(p);
    // small list of live (dst -> src) aliases; blocks rarely hold many
    let mut aliases: Vec<(u8, u8)> = Vec::with_capacity(8);
    for pc in 0..p.ins.len() {
        if l[pc] {
            aliases.clear();
        }
        let mut i = p.ins[pc];
        let (ra, rb, rc) = rewritable_reads(i.op);
        let resolve = |aliases: &[(u8, u8)], r: u8| {
            aliases
                .iter()
                .rev()
                .find(|&&(d, _)| d == r)
                .map(|&(_, sv)| sv)
                .unwrap_or(r)
        };
        if ra {
            i.a = resolve(&aliases, i.a);
        }
        if rb && !is_jump(i.op) {
            // b/c of jump family are the sbx encoding — never touch
            i.b = resolve(&aliases, i.b);
        }
        if rc && !is_jump(i.op) {
            i.c = resolve(&aliases, i.c);
        }
        p.ins[pc] = i;
        // a Call reuses the caller's register file above its window as the
        // callee frame — every reg > a is clobbered. Drop all aliases.
        if i.op == Op::Call {
            aliases.clear();
        }
        // update alias state from the (rewritten) instruction
        let e = effects(i, consts, upval_srcs);
        if let Some(w) = e.writes {
            // any alias involving w dies
            aliases.retain(|&(d, sv)| d != w && sv != w);
            if i.op == Op::Move && i.b != i.a {
                aliases.push((i.a, i.b));
            }
        }
    }
}

/// Pass 1.5: `OP rT, ...; Move rD, rT` with rT dead after — sink the
/// destination into OP and let DSE drop the Move. This is the `i++` /
/// `x = a op b` shape the emitter produces for every assignment.
fn sink_move_dst(
    p: &mut P,
    consts: &[Const],
    upval_srcs: &[Vec<UpvalSrc>],
    n_regs: u8,
) {
    use Op::*;
    let l = leaders(p);
    let live_in = liveness(p, consts, upval_srcs, n_regs);
    for pc in 0..p.ins.len().saturating_sub(1) {
        let m = p.ins[pc + 1];
        if m.op != Op::Move || l[pc + 1] {
            continue;
        }
        let i = p.ins[pc];
        // ops whose `a` is a pure destination (not also read, not a
        // window base): safe to retarget
        let sinkable = matches!(
            i.op,
            LoadConst | LoadInt | LoadBool | LoadNull | LoadUndef | Move | Add
                | Sub | Mul | Div | Mod | Pow | Neg | BitAnd | BitOr | BitXor
                | Shl | Shr | UShr | BitNot | Eq | Ne | Lt | Le | Gt | Ge | Not
                | Concat | TypeOf | Len | GetField | GetIndex | LoadCell
                | GetUpval | GetGlobal
        );
        if !sinkable || i.a != m.b || m.a == m.b {
            continue;
        }
        // rT must be dead after the Move, and OP must not read rT itself
        // (retargeting would change the read)
        let e = effects(i, consts, upval_srcs);
        let mut reads_a = false;
        e.for_reads(|r| reads_a |= r == i.a);
        if reads_a {
            continue;
        }
        let t = i.a as usize;
        let dead = live_in.get(pc + 2).map_or(false, |s| !ls_get(s, t));
        if !dead {
            continue;
        }
        let mut ni = i;
        ni.a = m.a;
        p.ins[pc] = ni;
        // the Move becomes `Move rD, rD` — a no-op DSE will delete
        p.ins[pc + 1] = Instr::abc(Op::Move, m.a, m.a, 0);
    }
}

/// Pass 2: delete pure defs whose target is never read afterwards.
/// Backward liveness over the CFG; iterates to a fixpoint.
fn dead_store_elim(
    p: &mut P,
    consts: &[Const],
    upval_srcs: &[Vec<UpvalSrc>],
    n_regs: u8,
) {
    for _round in 0..1 {
        let live_in = liveness(p, consts, upval_srcs, n_regs);
        let n = p.ins.len();
        let mut dead = vec![false; n];
        for pc in 0..n {
            let i = p.ins[pc];
            let pure_def = matches!(
                i.op,
                Op::Move | Op::LoadInt | Op::LoadBool | Op::LoadNull | Op::LoadUndef | Op::LoadConst
            );
            if !pure_def {
                continue;
            }
            // successor is pc+1 (pure defs never branch); dead if target
            // not live there. A Skip's successor is pinned: deleting it
            // would change which instruction gets skipped.
            if pc > 0 && is_skip(p.ins[pc - 1].op) {
                continue;
            }
            let a = i.a as usize;
            let self_move = i.op == Op::Move && i.a == i.b;
            let live_after = live_in.get(pc + 1).map_or(true, |s| ls_get(s, a));
            if self_move || !live_after {
                dead[pc] = true;
            }
        }
        if !dead.iter().any(|&d| d) {
            return;
        }
        rebuild(p, |pc| !dead[pc], &[]);
    }
}

/// 256-register live set as four u64 words.
pub(crate) type LiveSet = [u64; 4];

#[inline(always)]
fn ls_get(s: &LiveSet, r: usize) -> bool {
    s[r >> 6] & (1 << (r & 63)) != 0
}
#[inline(always)]
fn ls_set(s: &mut LiveSet, r: usize) {
    s[r >> 6] |= 1 << (r & 63);
}
#[inline(always)]
fn ls_clear(s: &mut LiveSet, r: usize) {
    s[r >> 6] &= !(1 << (r & 63));
}
#[inline(always)]
fn ls_or(a: &mut LiveSet, b: &LiveSet) {
    for i in 0..4 {
        a[i] |= b[i];
    }
}

/// Backward liveness: live_in[pc] bitmask. Iterative to fixpoint.
fn liveness(
    p: &P,
    consts: &[Const],
    upval_srcs: &[Vec<UpvalSrc>],
    _n_regs: u8,
) -> Vec<LiveSet> {
    let n = p.ins.len();
    let mut live_in: Vec<LiveSet> = vec![[0; 4]; n + 1];
    let mut changed = true;
    while changed {
        changed = false;
        for pc in (0..n).rev() {
            let i = p.ins[pc];
            let mut out: LiveSet = [0; 4];
            match i.op {
                Op::Return | Op::Halt => {}
                Op::Jump => {
                    let t = p.target[pc].unwrap_or(pc + 1);
                    ls_or(&mut out, &live_in[t.min(n)]);
                }
                Op::JumpIfFalse | Op::JumpIfTrue => {
                    ls_or(&mut out, &live_in[(pc + 1).min(n)]);
                    let t = p.target[pc].unwrap_or(pc + 1);
                    ls_or(&mut out, &live_in[t.min(n)]);
                }
                op if is_skip(op) => {
                    ls_or(&mut out, &live_in[(pc + 1).min(n)]);
                    ls_or(&mut out, &live_in[(pc + 2).min(n)]);
                }
                _ => ls_or(&mut out, &live_in[(pc + 1).min(n)]),
            }
            let e = effects(i, consts, upval_srcs);
            if let Some(w) = e.writes {
                ls_clear(&mut out, w as usize);
            }
            e.for_reads(|r| ls_set(&mut out, r as usize));
            if out != live_in[pc] {
                live_in[pc] = out;
                changed = true;
            }
        }
    }
    live_in
}

/// Pass 3: hoist single-def loop-invariant constants to a preheader.
/// Conservative: only innermost-style loops whose header is entered by
/// fall-through only (no forward jumps to the header), reg not live into
/// the header and not live after the loop.
fn hoist_loop_consts(
    p: &mut P,
    consts: &[Const],
    upval_srcs: &[Vec<UpvalSrc>],
    n_regs: u8,
) {
    let live_in = liveness(p, consts, upval_srcs, n_regs);
    let n = p.ins.len();
    // (insert_before_pc, def_pc) — collected, then applied in one rebuild
    let mut moves: Vec<(usize, usize)> = Vec::new();
    let mut moved_def = vec![false; n];
    for j in 0..n {
        let (Op::Jump, Some(h)) = (p.ins[j].op, p.target[j]) else { continue };
        if h >= j {
            continue; // not a back edge
        }
        // header entered only by fall-through + this back jump?
        let mut ok = true;
        for pc in 0..n {
            if pc != j && p.target[pc] == Some(h) {
                ok = false;
                break;
            }
        }
        // fall-through entry must not come from a Skip (pinned pair)
        if h == 0 || is_skip(p.ins[h - 1].op) {
            ok = false;
        }
        if !ok {
            continue;
        }
        let body = h..=j;
        for pc in body.clone() {
            let i = p.ins[pc];
            if !matches!(i.op, Op::LoadInt | Op::LoadBool | Op::LoadConst) {
                continue;
            }
            if moved_def[pc] || (pc > 0 && is_skip(p.ins[pc - 1].op)) {
                continue;
            }
            let a = i.a;
            // a Call in the body clobbers every reg above its window
            // (callee frame overlap) — hoisting a def of such a reg out
            // of the loop leaves garbage after the first call
            let call_clobbers = body.clone().any(|q| {
                let bi = p.ins[q];
                bi.op == Op::Call && bi.a <= a
            });
            if call_clobbers {
                continue;
            }
            // sole def of `a` in the body?
            let sole = body.clone().all(|q| {
                q == pc
                    || effects(p.ins[q], consts, upval_srcs).writes != Some(a)
            });
            if !sole {
                continue;
            }
            // `a` must be dead at the header (first-iteration reads before
            // the def would otherwise see the hoisted value early) and dead
            // after the loop (zero-iteration executions clobber it)
            let a_us = a as usize;
            let dead_at_header = live_in.get(h).map_or(false, |s| !ls_get(s, a_us));
            let dead_after = live_in.get(j + 1).map_or(true, |s| !ls_get(s, a_us));
            if dead_at_header && dead_after {
                moves.push((h, pc));
                moved_def[pc] = true;
            }
        }
    }
    if moves.is_empty() {
        return;
    }
    rebuild(p, |pc| !moved_def[pc], &moves);
}

/// Pass 4: renumber registers so the loop-hottest vregs get the lowest
/// ids (Tier-2 keeps vregs 0..8 in FP registers). Gated to functions with
/// no Call/Closure ops: a Call clobbers every reg above its window (the
/// callee frame overlaps), so stack discipline must be preserved there,
/// and child protos' capture descriptors name parent regs by id.
fn renumber_regs(
    p: &mut P,
    consts: &[Const],
    upval_srcs: &[Vec<UpvalSrc>],
    n_regs: u8,
    arity: u8,
) {
    if p.ins.iter().any(|i| matches!(i.op, Op::Call | Op::Closure)) {
        return;
    }
    let nr = n_regs as usize;
    if nr <= 8 || nr > 200 {
        return; // nothing to win, or too close to the encoding limit
    }
    // loop nesting depth per pc (count of enclosing back-edge spans)
    let n = p.ins.len();
    let mut depth = vec![0u32; n];
    for j in 0..n {
        if let (Op::Jump, Some(h)) = (p.ins[j].op, p.target[j]) {
            if h < j {
                for d in depth.iter_mut().take(j + 1).skip(h) {
                    *d += 1;
                }
            }
        }
    }
    // weight per reg
    let mut weight = vec![0u64; nr];
    for pc in 0..n {
        let e = effects(p.ins[pc], consts, upval_srcs);
        let w = 1u64 << (3 * depth[pc].min(6));
        e.for_reads(|r| {
            if (r as usize) < nr {
                weight[r as usize] += w;
            }
        });
        if let Some(r) = e.writes {
            if (r as usize) < nr {
                weight[r as usize] += w;
            }
        }
    }
    // contiguous groups from literal-window ops (kept contiguous, order
    // preserved); args 0..arity pinned in place
    let mut group = (0..nr).collect::<Vec<usize>>(); // group leader per reg
    let unite = |group: &mut Vec<usize>, lo: usize, hi: usize| {
        if hi >= nr {
            return;
        }
        let lead = group[lo];
        for r in lo..=hi {
            let l = group[r];
            for g in group.iter_mut() {
                if *g == l {
                    *g = lead;
                }
            }
        }
    };
    for pc in 0..n {
        let i = p.ins[pc];
        match i.op {
            Op::NewObjectLit => {
                let k = objlit_n(consts, i.c as usize);
                if k > 0 {
                    unite(&mut group, i.b as usize, i.b as usize + k - 1);
                }
            }
            Op::NewArrayLit => {
                if i.c > 0 {
                    unite(&mut group, i.b as usize, i.b as usize + i.c as usize - 1);
                }
            }
            _ => {}
        }
    }
    // collect units: (member regs ascending, weight, pinned)
    use std::collections::BTreeMap;
    let mut units: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for r in 0..nr {
        units.entry(group[r]).or_default().push(r);
    }
    let mut list: Vec<(Vec<usize>, u64, bool)> = units
        .into_values()
        .map(|members| {
            // groups must be contiguous reg ranges; merged unions of
            // overlapping windows are — verify defensively
            let contiguous = members.windows(2).all(|w| w[1] == w[0] + 1);
            let wsum = members.iter().map(|&r| weight[r]).sum::<u64>();
            let pinned =
                !contiguous || members.iter().any(|&r| r < arity as usize);
            (members, wsum, pinned)
        })
        .collect();
    // pinned units keep their ids; the rest are placed by weight into the
    // remaining id space, lowest ids first
    let mut new_id = vec![usize::MAX; nr];
    let mut taken = vec![false; nr];
    for (members, _, pinned) in &list {
        if *pinned {
            for &r in members {
                new_id[r] = r;
                taken[r] = true;
            }
        }
    }
    list.retain(|(_, _, pinned)| !pinned);
    list.sort_by(|a, b| b.1.cmp(&a.1));
    for (members, _, _) in &list {
        let len = members.len();
        // lowest free contiguous span of `len` ids
        let mut base = 0usize;
        'search: loop {
            if base + len > nr {
                return; // no space (shouldn't happen: permutation)
            }
            for o in 0..len {
                if taken[base + o] {
                    base += o + 1;
                    continue 'search;
                }
            }
            break;
        }
        for (o, &r) in members.iter().enumerate() {
            new_id[r] = base + o;
            taken[base + o] = true;
        }
    }
    if new_id.iter().enumerate().all(|(r, &nid)| nid == r) {
        return; // identity
    }
    let map = |r: u8| -> u8 {
        let r = r as usize;
        if r < nr { new_id[r] as u8 } else { r as u8 }
    };
    // rewrite operands
    for pc in 0..n {
        let mut i = p.ins[pc];
        match i.op {
            Op::Jump => {}
            Op::LoadConst | Op::LoadInt | Op::LoadBool | Op::GetGlobal
            | Op::JumpIfFalse | Op::JumpIfTrue => {
                // abx/asbx or a-only: only a is a register
                i.a = map(i.a);
            }
            Op::LoadNull | Op::LoadUndef | Op::Return | Op::Halt | Op::NewObject
            | Op::NewCell | Op::Await | Op::GetUpval => {
                i.a = map(i.a);
            }
            Op::NewArray => {
                i.a = map(i.a); // b is a capacity hint, not a reg
            }
            Op::NewObjectLit => {
                i.a = map(i.a);
                i.b = map(i.b); // window base; group kept contiguous
            }
            Op::NewArrayLit => {
                i.a = map(i.a);
                i.b = map(i.b); // c is the count
            }
            Op::SetUpval => {
                i.b = map(i.b); // a is the upval index
            }
            Op::GetField => {
                i.a = map(i.a);
                i.b = map(i.b); // c is a const index
            }
            Op::SetField => {
                i.a = map(i.a);
                i.c = map(i.c); // b is a const index
            }
            _ => {
                // plain abc register ops (arith, cmp, skips, moves, index,
                // len, push, cells, concat, typeof)
                i.a = map(i.a);
                i.b = map(i.b);
                i.c = map(i.c);
            }
        }
        p.ins[pc] = i;
    }
}

/// Rebuild the program keeping instructions where `keep(pc)`, inserting the
/// removed defs listed in `moves` (insert_before, def_pc) ahead of their
/// insertion point. Jump targets are remapped; targets pointing at a
/// removed instruction move to the next kept one.
fn rebuild(p: &mut P, keep: impl Fn(usize) -> bool, moves: &[(usize, usize)]) {
    let n = p.ins.len();
    let mut new_ins = Vec::with_capacity(n);
    let mut new_spans = Vec::with_capacity(n);
    let mut new_target = Vec::with_capacity(n);
    // old pc -> new pc of the first instruction at-or-after it
    let mut map = vec![0usize; n + 1];
    for pc in 0..n {
        map[pc] = new_ins.len();
        for &(at, def) in moves {
            if at == pc {
                new_ins.push(p.ins[def]);
                new_spans.push(p.spans[def]);
                new_target.push(None);
            }
        }
        // hoisted instructions land before the header, so entry falls
        // through them; map[pc] above points at them for jumps targeting
        // the header — WRONG for back edges. Fix: back-edge targets must
        // skip the inserted block, so remap header to the kept header
        // instruction below (map is recomputed for jump targets there).
        if keep(pc) {
            new_ins.push(p.ins[pc]);
            new_spans.push(p.spans[pc]);
            new_target.push(p.target[pc]);
        }
    }
    map[n] = new_ins.len();
    // header positions after insertion: the kept instruction, not the
    // inserted defs — recompute exact positions
    let mut exact = vec![usize::MAX; n + 1];
    {
        let mut idx = 0usize;
        for pc in 0..n {
            idx += moves.iter().filter(|&&(at, _)| at == pc).count();
            if keep(pc) {
                exact[pc] = idx;
                idx += 1;
            }
        }
        exact[n] = idx;
    }
    // remap targets: prefer the exact kept position; if the target was
    // removed, use the next kept instruction (map-style scan)
    let next_kept = |mut t: usize| -> usize {
        loop {
            if t >= n {
                return exact[n];
            }
            if exact[t] != usize::MAX {
                return exact[t];
            }
            t += 1;
        }
    };
    for t in new_target.iter_mut() {
        if let Some(old) = *t {
            *t = Some(next_kept(old));
        }
    }
    p.ins = new_ins;
    p.spans = new_spans;
    p.target = new_target;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn li(a: u8, k: i32) -> Instr {
        Instr::asbx(Op::LoadInt, a, k)
    }

    #[test]
    fn dead_move_removed() {
        // r1 = 5; r2 = r1 (dead); return r1
        let mut code = vec![
            li(1, 5),
            Instr::abc(Op::Move, 2, 1, 0),
            Instr::abc(Op::Return, 1, 0, 0),
        ];
        let mut spans = vec![0; 3];
        optimize(&mut code, &mut spans, &[], &[], 3, 0);
        assert_eq!(code.len(), 2);
        assert_eq!(code[0].op, Op::LoadInt);
        assert_eq!(code[1].op, Op::Return);
    }

    #[test]
    fn copy_prop_rewrites_use() {
        // r1 = 5; r2 = r1; r3 = r2 + r2; return r3
        let mut code = vec![
            li(1, 5),
            Instr::abc(Op::Move, 2, 1, 0),
            Instr::abc(Op::Add, 3, 2, 2),
            Instr::abc(Op::Return, 3, 0, 0),
        ];
        let mut spans = vec![0; 4];
        optimize(&mut code, &mut spans, &[], &[], 4, 0);
        // Move is dead after rewriting Add to read r1
        assert_eq!(code.len(), 3);
        assert_eq!(code[1].op, Op::Add);
        assert_eq!(code[1].b, 1);
        assert_eq!(code[1].c, 1);
    }

    #[test]
    fn loop_const_hoisted() {
        // pc0: r1 = 0            (counter)
        // pc1: r2 = 7            (loop-invariant, header)
        // pc2: r1 = r1 + r2
        // pc3: LtSkip r1 < r2*?  -- use a JumpIfTrue shape instead:
        // keep it simple: pc3 jump back to pc1
        // pc4: return r1
        let mut code = vec![
            li(1, 0),
            li(2, 7),
            Instr::abc(Op::Add, 1, 1, 2),
            Instr::asbx(Op::Jump, 0, -3),
            Instr::abc(Op::Return, 1, 0, 0),
        ];
        let mut spans = vec![0; 5];
        optimize(&mut code, &mut spans, &[], &[], 3, 0);
        // r2's def hoisted before the loop; back jump still targets Add's
        // block header (the old header slot now holds Add after DSE keeps
        // everything else)
        let hoisted = code
            .iter()
            .take_while(|i| i.op != Op::Add)
            .filter(|i| i.op == Op::LoadInt && i.a == 2)
            .count();
        assert_eq!(hoisted, 1, "invariant load should sit before the loop: {code:?}");
        // program still ends with return
        assert_eq!(code.last().unwrap().op, Op::Return);
    }

    #[test]
    fn skip_pair_untouched() {
        // EqSkip r1,r2 ; Jump +1 ; return r1  — successor of skip pinned
        let mut code = vec![
            li(1, 1),
            li(2, 1),
            Instr::abc(Op::EqSkip, 0, 1, 2),
            Instr::asbx(Op::Jump, 0, 0),
            Instr::abc(Op::Return, 1, 0, 0),
        ];
        let mut spans = vec![0; 5];
        let before = code.clone();
        optimize(&mut code, &mut spans, &[], &[], 3, 0);
        assert_eq!(code.len(), before.len());
        assert_eq!(code[2].op, Op::EqSkip);
        assert_eq!(code[3].op, Op::Jump);
    }
}
