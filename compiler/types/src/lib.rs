//! tsc-types
//!
//! Forward type dataflow over linear bytecode. The optimizing compiler compiles
//! every op (guarded templates when types unproven), so analysis never
//! rejects on op kind — facts only pick which sites get unguarded,
//! unboxed FP lanes. Only async functions reject.

use tsc_ir::{Const, FunctionProto, Op, TypeHint};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CondFact {
    Num,
    Bool,
    Other,
}

/// Analysis result consumed by the optimizing compiler.
pub struct TypedProto {
    pub opt_ok: bool,
    pub reason: &'static str,
    /// Per-pc, per-operand (a,b,c): operand register holds a proven
    /// number at instruction entry.
    pub num_facts: Vec<[bool; 3]>,
    /// Per-pc: JumpIfFalse/True condition fact.
    pub jumpif: Vec<CondFact>,
    /// Per-pc (b, c): operand is a known integer constant.
    pub const_ops: Vec<[Option<i64>; 2]>,
    /// Per-arg: entry guard proves this param numeric (annotation or
    /// uniform runtime feedback).
    pub arg_guard: Vec<bool>,
    /// Loop-header speculation: (header pc, vregs to number-guard on the
    /// preheader edge). Inside the loop these flow as proven Num; the
    /// compiler must guard the fall-in path (and OSR entries) and deopt
    /// to the header on a miss.
    pub loop_spec: Vec<(usize, Vec<u8>)>,
    /// Subset of `loop_spec` worth holding unboxed in an integer register
    /// for the body of the loop: (header pc, vregs).
    ///
    /// Integrality cannot be *proven* forward — `Add` is not closed over
    /// it, since a large enough product is infinity and infinity has no
    /// fractional part to test. Real engines do not try: they speculate
    /// int32 and let the hardware carry the check, so an overflowing add
    /// sets V and branches to deopt. That is only possible on an unboxed
    /// value, which is what this list marks.
    ///
    /// A vreg qualifies when it is numeric at the header and every write
    /// to it inside the loop is an operation that keeps an integer an
    /// integer (given an overflow check). Anything else — a division, a
    /// call, a field read — leaves it out. Being numeric is not enough to
    /// unbox, so the compiler owes an int32 guard at the header.
    pub int_spec: Vec<(usize, Vec<u8>)>,
    /// Per-pc, per-operand (a,b,c): operand is a proven integer in i32
    /// range (a constant, or a field read whose shape says Int32).
    pub int_facts: Vec<[bool; 3]>,
    /// Per NewObjectLit pc: class of each literal value in order —
    /// 0 proven i32 integer, 1 proven number, 2 unknown.
    pub lit_vals: Vec<Vec<u8>>,
}

/// Field representation as the caller reports it per pc, for GetField
/// and SetField sites with a warm monomorphic cache: 0 Int32, 1 Number,
/// 2 Any / unknown.
pub const REPR_INT32: u8 = 0;
pub const REPR_NUMBER: u8 = 1;
pub const REPR_ANY: u8 = 2;

/// Ops whose result stays an integer when their inputs are integers,
/// assuming the overflow check the compiler emits alongside them.
/// `Div` and `Pow` are deliberately absent: 5/2 is not an integer.
fn keeps_int(op: Op) -> bool {
    matches!(
        op,
        Op::LoadInt
            | Op::Move
            | Op::Add
            | Op::Sub
            | Op::Mul
            | Op::Mod
            | Op::Neg
            | Op::BitAnd
            | Op::BitOr
            | Op::BitXor
            | Op::BitNot
            | Op::Shl
            | Op::Shr
            | Op::UShr
    )
}

// internal lattice: Num / Bool / Top (strings, objects, etc. all Top —
// codegen only cares about the two primitive fast lanes)
#[derive(Clone, Copy, PartialEq, Eq)]
enum T {
    /// A known integer constant (refines Num; enables constant-divisor
    /// codegen for Mod/Div).
    Int(i64),
    /// An integer of unknown value in i32 range: a field read the shape
    /// vouches for, or the join of two such constants. Not closed under
    /// arithmetic — that is the lane's business.
    IntV,
    Num,
    Bool,
    Top,
}

impl T {
    fn is_num(self) -> bool {
        matches!(self, T::Num | T::Int(_) | T::IntV)
    }
    fn is_int(self) -> bool {
        match self {
            T::Int(v) => v >= i32::MIN as i64 && v <= i32::MAX as i64,
            T::IntV => true,
            _ => false,
        }
    }
}

fn join(a: T, b: T) -> T {
    if a == b {
        return a;
    }
    if a.is_int() && b.is_int() {
        return T::IntV;
    }
    if a.is_num() && b.is_num() {
        return T::Num; // two different constants: still numeric
    }
    T::Top
}

pub fn analyze(proto: &FunctionProto, field_repr: &[u8]) -> TypedProto {
    let reject = |reason: &'static str| TypedProto {
        opt_ok: false,
        reason,
        num_facts: Vec::new(),
        jumpif: Vec::new(),
        const_ops: Vec::new(),
        arg_guard: Vec::new(),
        loop_spec: Vec::new(),
        int_spec: Vec::new(),
        int_facts: Vec::new(),
        lit_vals: Vec::new(),
    };
    let repr_at = |pc: usize| field_repr.get(pc).copied().unwrap_or(REPR_ANY);
    if proto.arity > 8 {
        return reject("arity > 8 (unprofiled)");
    }

    // entry: param proven Num by annotation or uniform runtime feedback
    // (entry guard enforces; unproven params flow as Top, no guard)
    let mut arg_guard = vec![false; proto.arity as usize];
    for (i, g) in arg_guard.iter_mut().enumerate() {
        let annotated = proto.arg_types.get(i) == Some(&TypeHint::Num);
        let seen = proto.jit.arg_seen[i].load(std::sync::atomic::Ordering::Relaxed);
        *g = annotated || seen == 1;
    }

    let body = proto.body();
    let n = body.code.len();
    let nregs = body.n_regs as usize;
    let mut states: Vec<Option<Vec<T>>> = vec![None; n + 1];
    let mut entry = vec![T::Top; nregs];
    for (i, &g) in arg_guard.iter().enumerate() {
        if g {
            entry[i] = T::Num;
        }
    }
    states[0] = Some(entry.clone());
    let mut work = vec![0usize];
    let mut num_facts = vec![[false; 3]; n];
    let mut jumpif = vec![CondFact::Other; n];
    let mut const_ops: Vec<[Option<i64>; 2]> = vec![[None; 2]; n];
    // loop-header speculation state (filled between the two passes):
    // pinned[H] = set of vregs forced Num at header H's merge
    let mut pinned: std::collections::HashMap<usize, Vec<u8>> =
        std::collections::HashMap::new();

    let merge = |states: &mut Vec<Option<Vec<T>>>,
                 work: &mut Vec<usize>,
                 pinned: &std::collections::HashMap<usize, Vec<u8>>,
                 t: usize,
                 s: &[T]| {
        let pins = pinned.get(&t);
        let pin = |r: usize, v: T| -> T {
            if pins.is_some_and(|p| p.contains(&(r as u8))) {
                T::Num // speculated: the preheader guard enforces this
            } else {
                v
            }
        };
        match &mut states[t] {
            None => {
                let mut v = s.to_vec();
                for (r, slot) in v.iter_mut().enumerate() {
                    *slot = pin(r, *slot);
                }
                states[t] = Some(v);
                work.push(t);
            }
            Some(old) => {
                let mut changed = false;
                for (r, (o, &v)) in old.iter_mut().zip(s.iter()).enumerate() {
                    let j = pin(r, join(*o, v));
                    if j != *o {
                        *o = j;
                        changed = true;
                    }
                }
                if changed {
                    work.push(t);
                }
            }
        }
    };

    // ---- two passes: discover speculation candidates, then re-run with
    // headers pinned Num for them ----
    for pass in 0..2 {
        if pass == 1 {
            // candidates: back-edge target H where states[H][v] joined to
            // Top but every back-edge source carries Num for v
            let mut spec: std::collections::HashMap<usize, Vec<u8>> =
                std::collections::HashMap::new();
            for (pc, ins) in body.code.iter().enumerate() {
                if ins.op != Op::Jump || ins.sbx() >= 0 {
                    continue;
                }
                let h = (pc as i64 + ins.sbx() as i64 + 1) as usize;
                let (Some(hs), Some(es)) = (&states[h], &states[pc]) else {
                    continue;
                };
                for r in 0..nregs {
                    if hs[r] == T::Top && es[r].is_num() {
                        let e = spec.entry(h).or_default();
                        if !e.contains(&(r as u8)) {
                            e.push(r as u8);
                        }
                    }
                }
            }
            // multiple back-edges to one header: require Num on ALL of them
            for (pc, ins) in body.code.iter().enumerate() {
                if ins.op != Op::Jump || ins.sbx() >= 0 {
                    continue;
                }
                let h = (pc as i64 + ins.sbx() as i64 + 1) as usize;
                if let (Some(vs), Some(es)) = (spec.get_mut(&h), &states[pc]) {
                    vs.retain(|&r| es[r as usize].is_num());
                }
            }
            // only speculate vregs LIVE-IN at the header: a dead loop
            // temp is garbage on first entry — its guard would deopt
            // every call (measured: 2.4x regression on primes)
            for (h, vs) in spec.iter_mut() {
                // find this header's furthest back-edge (loop end)
                let end = body
                    .code
                    .iter()
                    .enumerate()
                    .filter(|(pc, i)| {
                        i.op == Op::Jump
                            && i.sbx() < 0
                            && (*pc as i64 + i.sbx() as i64 + 1) as usize == *h
                    })
                    .map(|(pc, _)| pc)
                    .max()
                    .unwrap_or(*h);
                vs.retain(|&r| {
                    for pc in *h..=end {
                        let i = body.code[pc];
                        let reads_a = matches!(
                            i.op,
                            Op::SetField
                                | Op::SetIndex
                                | Op::ArrayPush
                                | Op::StoreCell
                                | Op::SetUpval
                                | Op::Return
                                | Op::JumpIfFalse
                                | Op::JumpIfTrue
                                | Op::NewCell
                                | Op::Await
                                | Op::Call
                        );
                        let bx_form = matches!(
                            i.op,
                            Op::LoadConst
                                | Op::LoadInt
                                | Op::Jump
                                | Op::JumpIfFalse
                                | Op::JumpIfTrue
                                | Op::Closure
                                | Op::GetGlobal
                        );
                        let reads_bc = !bx_form;
                        if reads_a && i.a == r {
                            return true; // read before any write
                        }
                        if reads_bc && (i.b == r || i.c == r) {
                            return true;
                        }
                        // writes: most ops write a
                        let writes_a = !matches!(
                            i.op,
                            Op::SetField
                                | Op::SetIndex
                                | Op::ArrayPush
                                | Op::StoreCell
                                | Op::SetUpval
                                | Op::Jump
                                | Op::JumpIfFalse
                                | Op::JumpIfTrue
                                | Op::Return
                                | Op::Halt
                                | Op::EqSkip
                                | Op::NeSkip
                                | Op::LtSkip
                                | Op::LeSkip
                                | Op::GtSkip
                                | Op::GeSkip
                        );
                        if writes_a && i.a == r {
                            return false; // defined before use
                        }
                    }
                    false // never used in the loop
                });
            }
            spec.retain(|_, vs| !vs.is_empty());
            if spec.is_empty() {
                break; // nothing to speculate: pass-1 results stand
            }
            pinned = spec;
            // reset and re-run the fixpoint with pinning
            states = vec![None; n + 1];
            states[0] = Some(entry.clone());
            work = vec![0usize];
            num_facts = vec![[false; 3]; n];
            jumpif = vec![CondFact::Other; n];
            const_ops = vec![[None; 2]; n];
        }
    while let Some(pc) = work.pop() {
        if pc >= n {
            continue;
        }
        let mut s = states[pc].clone().unwrap();
        let ins = body.code[pc];
        let a = ins.a as usize;
        // b/c bytes overlap the sBx payload on Bx-format ops — out-of-range
        // "registers" just read as not-Num
        let fact = |s: &[T], r: u8| s.get(r as usize).copied().unwrap_or(T::Top);
        // facts monotonically narrow across re-visits (lattice join only
        // widens toward Top), so last write is the sound fixpoint value
        num_facts[pc] = [
            fact(&s, ins.a).is_num(),
            fact(&s, ins.b).is_num(),
            fact(&s, ins.c).is_num(),
        ];
        // constant integer operands (b, c) for constant-divisor codegen
        const_ops[pc] = [
            match fact(&s, ins.b) {
                T::Int(v) => Some(v),
                _ => None,
            },
            match fact(&s, ins.c) {
                T::Int(v) => Some(v),
                _ => None,
            },
        ];
        let mut next: Vec<usize> = vec![pc + 1];
        match ins.op {
            Op::LoadInt => s[a] = T::Int(ins.sbx() as i64),
            Op::LoadConst => {
                s[a] = match body.consts.get(ins.bx() as usize) {
                    Some(Const::Number(n))
                        if n.fract() == 0.0 && n.abs() < 9e15 =>
                    {
                        T::Int(*n as i64)
                    }
                    Some(Const::Number(_)) => T::Num,
                    _ => T::Top,
                }
            }
            Op::LoadBool => s[a] = T::Bool,
            Op::LoadNull | Op::LoadUndef => s[a] = T::Top,
            Op::Move => s[a] = fact(&s, ins.b),
            Op::Add => {
                // string concat possible unless both proven Num
                s[a] = if fact(&s, ins.b).is_num() && fact(&s, ins.c).is_num() {
                    T::Num
                } else {
                    T::Top
                };
            }
            Op::Sub
            | Op::Mul
            | Op::Div
            | Op::Mod
            | Op::Pow
            | Op::Neg
            | Op::BitAnd
            | Op::BitOr
            | Op::BitXor
            | Op::Shl
            | Op::Shr
            | Op::UShr
            | Op::BitNot => {
                // numeric result or runtime error — Num either way
                s[a] = T::Num;
            }
            Op::Eq | Op::Ne | Op::Lt | Op::Le | Op::Gt | Op::Ge | Op::Not => s[a] = T::Bool,
            Op::EqSkip | Op::NeSkip | Op::LtSkip | Op::LeSkip | Op::GtSkip | Op::GeSkip => {
                next = vec![pc + 1, pc + 2];
            }
            Op::Jump => {
                next = vec![(pc as i64 + ins.sbx() as i64 + 1) as usize];
            }
            Op::JumpIfFalse | Op::JumpIfTrue => {
                jumpif[pc] = match fact(&s, ins.a) {
                    x if x.is_num() => CondFact::Num,
                    T::Bool => CondFact::Bool,
                    _ => CondFact::Other,
                };
                next = vec![pc + 1, (pc as i64 + ins.sbx() as i64 + 1) as usize];
            }
            Op::Len => s[a] = T::Num,
            // a warm monomorphic field read: the shape's representation
            // for that slot is the result type (the site deopts on a miss)
            Op::GetField => {
                s[a] = match repr_at(pc) {
                    REPR_INT32 => T::IntV,
                    REPR_NUMBER => T::Num,
                    _ => T::Top,
                }
            }
            Op::Return | Op::Halt => next = vec![],
            Op::Await => s[a] = T::Top,
            _ => {
                // everything else (Call, heap ops, cells, upvals, globals,
                // Concat, TypeOf, closures) writes an unproven result
                s[a] = T::Top;
            }
        }
        for t in next {
            merge(&mut states, &mut work, &pinned, t, &s);
        }
    }
    } // pass loop

    let loop_spec: Vec<(usize, Vec<u8>)> = {
        let mut v: Vec<_> = pinned.into_iter().collect();
        v.sort_by_key(|(h, _)| *h);
        v
    };

    // integer lane: loop-carried numeric vregs whose every in-loop write
    // keeps an integer an integer. Derived from the header state rather
    // than from `loop_spec` — a counter that was already proven Num never
    // needed speculation, and it is exactly what we want unboxed.
    //
    // Being a number is not enough to unbox (1.5 is a number), so the
    // compiler still owes an int32 guard at the header. That guard is
    // paid once per loop entry instead of per use.
    let mut int_spec: Vec<(usize, Vec<u8>)> = Vec::new();
    let mut headers: Vec<usize> = body
        .code
        .iter()
        .enumerate()
        .filter(|(_, i)| i.op == Op::Jump && i.sbx() < 0)
        .map(|(pc, i)| (pc as i64 + i.sbx() as i64 + 1) as usize)
        .collect();
    headers.sort_unstable();
    headers.dedup();
    for h in headers {
        let Some(hs) = &states[h] else { continue };
        let end = loop_end(body, h);

        // Optimistically assume every vreg the loop writes stays integral,
        // then withdraw any whose producer is not integer-preserving or
        // whose operands are not themselves integral. Iterating matters:
        // one float operand has to propagate through everything it feeds.
        //
        // Checking only the operation was not enough. `zr = zr*zr - zi*zi
        // + cr` is nothing but Mul, Sub and Add, so the op test accepted
        // it and every entry to a float loop then failed the header guard
        // and deopted — 2.5x on mandelbrot.
        let mut cand: Vec<bool> = (0..nregs)
            .map(|r| body.code[h..=end].iter().any(|i| writes_a(i.op) && i.a as usize == r))
            .collect();
        loop {
            let mut changed = false;
            for (off, i) in body.code[h..=end].iter().enumerate() {
                let pc2 = h + off;
                if !writes_a(i.op) || !cand[i.a as usize] {
                    continue;
                }
                // an operand is integral if it is a known integer constant
                // at this site, or a vreg still believed integral
                let int_operand = |r: u8, side: usize| {
                    const_ops[pc2][side].is_some()
                        || cand.get(r as usize).copied().unwrap_or(false)
                };
                let ok = match i.op {
                    // loads an integer immediate; b/c are not registers
                    Op::LoadInt => true,
                    // a constant load is integral when the constant is —
                    // the emitter reuses one vreg for a literal and for an
                    // arithmetic result, so missing this withdrew the whole
                    // chain that fed off it
                    Op::LoadConst => matches!(
                        body.consts.get(i.bx() as usize),
                        Some(Const::Number(n)) if n.fract() == 0.0 && n.abs() < 2147483648.0
                    ),
                    Op::Move | Op::Neg | Op::BitNot => int_operand(i.b, 0),
                    // a field the shape vouches for as Int32
                    Op::GetField => repr_at(pc2) == REPR_INT32,
                    _ if keeps_int(i.op) => {
                        int_operand(i.b, 0) && int_operand(i.c, 1)
                    }
                    _ => false,
                };
                if !ok {
                    cand[i.a as usize] = false;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }

        let keep: Vec<u8> = (0..nregs)
            .filter(|&r| hs[r].is_num() && cand[r])
            .map(|r| r as u8)
            .collect();
        if !keep.is_empty() {
            int_spec.push((h, keep));
        }
    }

    // integer facts per operand and literal value classes, from the
    // final per-pc states
    let mut int_facts = vec![[false; 3]; n];
    let mut lit_vals: Vec<Vec<u8>> = vec![Vec::new(); n];
    for (pc, ins) in body.code.iter().enumerate() {
        let Some(s) = &states[pc] else { continue };
        let f = |r: u8| s.get(r as usize).copied().is_some_and(|t| t.is_int());
        int_facts[pc] = [f(ins.a), f(ins.b), f(ins.c)];
        if ins.op == Op::NewObjectLit {
            if let Some(Const::Keys(k)) = body.consts.get(ins.c as usize) {
                lit_vals[pc] = (0..k.len())
                    .map(|j| match s.get(ins.b as usize + j).copied() {
                        Some(t) if t.is_int() => 0,
                        Some(t) if t.is_num() => 1,
                        _ => 2,
                    })
                    .collect();
            }
        }
    }

    TypedProto {
        opt_ok: true,
        reason: "",
        num_facts,
        jumpif,
        const_ops,
        arg_guard,
        loop_spec,
        int_spec,
        int_facts,
        lit_vals,
    }
}

/// Last instruction of the loop whose header is `h`: its furthest back edge.
fn loop_end(body: &tsc_ir::ProtoBody, h: usize) -> usize {
    body.code
        .iter()
        .enumerate()
        .filter(|(pc, i)| {
            i.op == Op::Jump && i.sbx() < 0 && (*pc as i64 + i.sbx() as i64 + 1) as usize == h
        })
        .map(|(pc, _)| pc)
        .max()
        .unwrap_or(h)
}

/// Whether this op writes its `a` operand (the rest read it).
fn writes_a(op: Op) -> bool {
    !matches!(
        op,
        Op::SetField
            | Op::SetIndex
            | Op::ArrayPush
            | Op::StoreCell
            | Op::SetUpval
            | Op::Jump
            | Op::JumpIfFalse
            | Op::JumpIfTrue
            | Op::Return
            | Op::Halt
            | Op::EqSkip
            | Op::NeSkip
            | Op::LtSkip
            | Op::LeSkip
            | Op::GtSkip
            | Op::GeSkip
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tsc_ir::{Instr, Op};

    fn proto(arity: u8, arg_types: Vec<TypeHint>, code: Vec<Instr>) -> FunctionProto {
        FunctionProto::new(
            Arc::from("t"),
            arity,
            false,
            vec![],
            arg_types,
            tsc_ir::ProtoBody {
                n_regs: 8,
                spans: vec![0; code.len()],
                code,
                consts: vec![],
                protos: vec![],
            },
        )
    }

    #[test]
    fn num_facts_on_annotated_loop() {
        // r1 = 0; r1 = r1 + r0; return r1
        let p = proto(
            1,
            vec![TypeHint::Num],
            vec![
                Instr::asbx(Op::LoadInt, 1, 0),
                Instr::abc(Op::Add, 1, 1, 0),
                Instr::abc(Op::Return, 1, 0, 0),
            ],
        );
        let t = analyze(&p, &[]);
        assert!(t.opt_ok);
        assert!(t.arg_guard[0]);
        assert!(t.num_facts[1][1] && t.num_facts[1][2]);
    }

    #[test]
    fn object_code_compiles_without_guard() {
        let p = proto(
            1,
            vec![TypeHint::Top],
            vec![
                Instr::abc(Op::NewObject, 1, 0, 0),
                Instr::abc(Op::Return, 1, 0, 0),
            ],
        );
        let t = analyze(&p, &[]);
        assert!(t.opt_ok);
        assert!(!t.arg_guard[0]);
        assert!(!t.num_facts[1][1]); // NewObject result not Num
    }
}

#[cfg(test)]
mod int_lane {
    use super::*;
    use std::sync::Arc;
    use tsc_ir::{Instr, Op};

    fn loopy(code: Vec<Instr>) -> FunctionProto {
        FunctionProto::new(
            Arc::from("t"),
            1,
            false,
            vec![],
            vec![TypeHint::Num],
            tsc_ir::ProtoBody {
                n_regs: 8,
                spans: vec![0; code.len()],
                code,
                consts: vec![],
                protos: vec![],
            },
        )
    }

    /// `for (i = 0; i < n; i++) acc += i` — both induction variables are
    /// written only by integer-preserving ops, so both take the int lane.
    #[test]
    fn counter_and_accumulator_qualify() {
        let p = loopy(vec![
            Instr::asbx(Op::LoadInt, 1, 0),  // i = 0
            Instr::asbx(Op::LoadInt, 2, 0),  // acc = 0
            Instr::abc(Op::LtSkip, 0, 1, 0), // header: i < n
            Instr::asbx(Op::Jump, 0, 4),
            Instr::abc(Op::Add, 2, 2, 1), // acc += i
            Instr::asbx(Op::LoadInt, 3, 1),
            Instr::abc(Op::Add, 1, 1, 3), // i += 1
            Instr::asbx(Op::Jump, 0, -6),
            Instr::abc(Op::Return, 2, 0, 0),
        ]);
        let t = analyze(&p, &[]);
        let vs = &t.int_spec.first().expect("a loop was speculated").1;
        assert!(vs.contains(&1), "counter should be unboxed: {vs:?}");
        assert!(vs.contains(&2), "accumulator should be unboxed: {vs:?}");
    }

    /// A division in the loop makes the value non-integral, so it must be
    /// left boxed even though it is still a number.
    #[test]
    fn division_disqualifies() {
        let p = loopy(vec![
            Instr::asbx(Op::LoadInt, 1, 0),
            Instr::asbx(Op::LoadInt, 2, 0),
            Instr::abc(Op::LtSkip, 0, 1, 0),
            Instr::asbx(Op::Jump, 0, 4),
            Instr::abc(Op::Div, 2, 2, 1), // acc = acc / i  -- not an integer
            Instr::asbx(Op::LoadInt, 3, 1),
            Instr::abc(Op::Add, 1, 1, 3),
            Instr::asbx(Op::Jump, 0, -6),
            Instr::abc(Op::Return, 2, 0, 0),
        ]);
        let t = analyze(&p, &[]);
        let vs = t.int_spec.first().map(|(_, v)| v.clone()).unwrap_or_default();
        assert!(!vs.contains(&2), "divided value must stay boxed: {vs:?}");
    }

    /// A value the loop never writes is loop-invariant; unboxing it would
    /// pay for a guard and buy nothing back.
    #[test]
    fn loop_invariant_value_is_not_unboxed() {
        let p = loopy(vec![
            Instr::asbx(Op::LoadInt, 1, 0),
            Instr::abc(Op::LtSkip, 0, 1, 0),
            Instr::asbx(Op::Jump, 0, 3),
            Instr::asbx(Op::LoadInt, 3, 1),
            Instr::abc(Op::Add, 1, 1, 3),
            Instr::asbx(Op::Jump, 0, -4),
            Instr::abc(Op::Return, 1, 0, 0),
        ]);
        let t = analyze(&p, &[]);
        let vs = &t.int_spec.first().expect("a loop was found").1;
        assert!(vs.contains(&1), "counter should be unboxed: {vs:?}");
        assert!(!vs.contains(&0), "the parameter is never written: {vs:?}");
    }
}
