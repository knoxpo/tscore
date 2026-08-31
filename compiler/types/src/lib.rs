//! tsc-types
//!
//! Forward type dataflow over linear bytecode. Unified Tier-2 compiles
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

/// Analysis result consumed by the Tier-2 compiler.
pub struct TypedProto {
    pub tier2_ok: bool,
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
}

// internal lattice: Num / Bool / Top (strings, objects, etc. all Top —
// codegen only cares about the two primitive fast lanes)
#[derive(Clone, Copy, PartialEq, Eq)]
enum T {
    /// A known integer constant (refines Num; enables constant-divisor
    /// codegen for Mod/Div).
    Int(i64),
    Num,
    Bool,
    Top,
}

impl T {
    fn is_num(self) -> bool {
        matches!(self, T::Num | T::Int(_))
    }
}

fn join(a: T, b: T) -> T {
    if a == b {
        return a;
    }
    if a.is_num() && b.is_num() {
        return T::Num; // two different constants: still numeric
    }
    T::Top
}

pub fn analyze(proto: &FunctionProto) -> TypedProto {
    let reject = |reason: &'static str| TypedProto {
        tier2_ok: false,
        reason,
        num_facts: Vec::new(),
        jumpif: Vec::new(),
        const_ops: Vec::new(),
        arg_guard: Vec::new(),
        loop_spec: Vec::new(),
    };
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

    TypedProto {
        tier2_ok: true,
        reason: "",
        num_facts,
        jumpif,
        const_ops,
        arg_guard,
        loop_spec,
    }
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
        let t = analyze(&p);
        assert!(t.tier2_ok);
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
        let t = analyze(&p);
        assert!(t.tier2_ok);
        assert!(!t.arg_guard[0]);
        assert!(!t.num_facts[1][1]); // NewObject result not Num
    }
}
