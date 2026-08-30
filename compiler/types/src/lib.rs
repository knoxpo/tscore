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
    /// Per-arg: entry guard proves this param numeric (annotation or
    /// uniform runtime feedback).
    pub arg_guard: Vec<bool>,
}

// internal lattice: Num / Bool / Top (strings, objects, etc. all Top —
// codegen only cares about the two primitive fast lanes)
#[derive(Clone, Copy, PartialEq, Eq)]
enum T {
    Num,
    Bool,
    Top,
}

fn join(a: T, b: T) -> T {
    if a == b {
        a
    } else {
        T::Top
    }
}

pub fn analyze(proto: &FunctionProto) -> TypedProto {
    let reject = |reason: &'static str| TypedProto {
        tier2_ok: false,
        reason,
        num_facts: Vec::new(),
        jumpif: Vec::new(),
        arg_guard: Vec::new(),
    };
    if proto.is_async {
        return reject("async");
    }
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

    let n = proto.code.len();
    let nregs = proto.n_regs as usize;
    let mut states: Vec<Option<Vec<T>>> = vec![None; n + 1];
    let mut entry = vec![T::Top; nregs];
    for (i, &g) in arg_guard.iter().enumerate() {
        if g {
            entry[i] = T::Num;
        }
    }
    states[0] = Some(entry);
    let mut work = vec![0usize];
    let mut num_facts = vec![[false; 3]; n];
    let mut jumpif = vec![CondFact::Other; n];

    let merge = |states: &mut Vec<Option<Vec<T>>>, work: &mut Vec<usize>, t: usize, s: &[T]| {
        match &mut states[t] {
            None => {
                states[t] = Some(s.to_vec());
                work.push(t);
            }
            Some(old) => {
                let mut changed = false;
                for (o, &v) in old.iter_mut().zip(s.iter()) {
                    let j = join(*o, v);
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

    while let Some(pc) = work.pop() {
        if pc >= n {
            continue;
        }
        let mut s = states[pc].clone().unwrap();
        let ins = proto.code[pc];
        let a = ins.a as usize;
        // b/c bytes overlap the sBx payload on Bx-format ops — out-of-range
        // "registers" just read as not-Num
        let fact = |s: &[T], r: u8| s.get(r as usize).copied().unwrap_or(T::Top);
        // facts monotonically narrow across re-visits (lattice join only
        // widens toward Top), so last write is the sound fixpoint value
        num_facts[pc] = [
            fact(&s, ins.a) == T::Num,
            fact(&s, ins.b) == T::Num,
            fact(&s, ins.c) == T::Num,
        ];
        let mut next: Vec<usize> = vec![pc + 1];
        match ins.op {
            Op::LoadInt => s[a] = T::Num,
            Op::LoadConst => {
                s[a] = match proto.consts.get(ins.bx() as usize) {
                    Some(Const::Number(_)) => T::Num,
                    _ => T::Top,
                }
            }
            Op::LoadBool => s[a] = T::Bool,
            Op::LoadNull | Op::LoadUndef => s[a] = T::Top,
            Op::Move => s[a] = fact(&s, ins.b),
            Op::Add => {
                // string concat possible unless both proven Num
                s[a] = if fact(&s, ins.b) == T::Num && fact(&s, ins.c) == T::Num {
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
                    T::Num => CondFact::Num,
                    T::Bool => CondFact::Bool,
                    T::Top => CondFact::Other,
                };
                next = vec![pc + 1, (pc as i64 + ins.sbx() as i64 + 1) as usize];
            }
            Op::Len => s[a] = T::Num,
            Op::Return | Op::Halt => next = vec![],
            Op::Await => return reject("async op"),
            _ => {
                // everything else (Call, heap ops, cells, upvals, globals,
                // Concat, TypeOf, closures) writes an unproven result
                s[a] = T::Top;
            }
        }
        for t in next {
            merge(&mut states, &mut work, t, &s);
        }
    }

    TypedProto {
        tier2_ok: true,
        reason: "",
        num_facts,
        jumpif,
        arg_guard,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tsc_ir::{Instr, Op};

    fn proto(arity: u8, arg_types: Vec<TypeHint>, code: Vec<Instr>) -> FunctionProto {
        FunctionProto {
            name: Arc::from("t"),
            arity,
            is_async: false,
            n_regs: 8,
            spans: vec![0; code.len()],
            code,
            consts: vec![],
            upvals: vec![],
            protos: vec![],
            arg_types,
            jit: Default::default(),
        }
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
