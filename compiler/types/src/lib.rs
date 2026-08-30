//! tsc-types
//!
//! Forward type dataflow over linear bytecode: per-register `TypeHint`
//! facts from TS parameter annotations, used to qualify functions for the
//! Tier-2 (unboxed) JIT and to tell it which guards are provably dead.

use tsc_ir::{Const, FunctionProto, Op, TypeHint};

/// Analysis result consumed by the Tier-2 compiler.
pub struct TypedProto {
    pub tier2_ok: bool,
    pub reason: &'static str,
    /// Per-pc: JumpIfFalse/True condition is a proven number (else Bool).
    pub jumpif_num: Vec<bool>,
}

fn join(a: TypeHint, b: TypeHint) -> TypeHint {
    if a == b {
        a
    } else {
        TypeHint::Top
    }
}

/// Ops Tier-2 can compile. Anything else rejects the function.
fn supported(op: Op) -> bool {
    matches!(
        op,
        Op::LoadInt
            | Op::LoadConst
            | Op::LoadBool
            | Op::LoadUndef
            | Op::Move
            | Op::Add
            | Op::Sub
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
            | Op::BitNot
            | Op::Eq
            | Op::Ne
            | Op::Lt
            | Op::Le
            | Op::Gt
            | Op::Ge
            | Op::EqSkip
            | Op::NeSkip
            | Op::LtSkip
            | Op::LeSkip
            | Op::GtSkip
            | Op::GeSkip
            | Op::Jump
            | Op::JumpIfFalse
            | Op::JumpIfTrue
            | Op::Return
    )
}

pub fn analyze(proto: &FunctionProto) -> TypedProto {
    let reject = |reason: &'static str| TypedProto {
        tier2_ok: false,
        reason,
        jumpif_num: Vec::new(),
    };

    if proto.is_async {
        return reject("async");
    }
    if proto.upvals.is_empty() == false {
        return reject("captures upvalues");
    }
    // all params must be annotated number (feedback integration later)
    if proto.arg_types.len() != proto.arity as usize
        || proto.arg_types.iter().any(|t| *t != TypeHint::Num)
    {
        return reject("params not all `: number`");
    }
    for ins in &proto.code {
        if !supported(ins.op) {
            return reject("unsupported op");
        }
        if ins.op == Op::LoadConst {
            if !matches!(proto.consts[ins.bx() as usize], Const::Number(_)) {
                return reject("string constant");
            }
        }
    }

    // forward fixpoint: state = per-register fact at instruction entry
    let n = proto.code.len();
    let nregs = proto.n_regs as usize;
    let mut states: Vec<Option<Vec<TypeHint>>> = vec![None; n + 1];
    let mut entry = vec![TypeHint::Other; nregs];
    for i in 0..proto.arity as usize {
        entry[i] = TypeHint::Num; // entry guards enforce this
    }
    states[0] = Some(entry);
    let mut work = vec![0usize];
    let mut jumpif_num = vec![false; n];

    let merge = |states: &mut Vec<Option<Vec<TypeHint>>>,
                 work: &mut Vec<usize>,
                 target: usize,
                 s: &[TypeHint]| {
        match &mut states[target] {
            None => {
                states[target] = Some(s.to_vec());
                work.push(target);
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
                    work.push(target);
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
        let fact = |s: &[TypeHint], r: u8| s[r as usize];
        let mut next = vec![pc + 1];
        match ins.op {
            Op::LoadInt => s[a] = TypeHint::Num,
            Op::LoadConst => s[a] = TypeHint::Num, // only Number consts pass the gate
            Op::LoadBool => s[a] = TypeHint::Bool,
            Op::LoadUndef => s[a] = TypeHint::Other,
            Op::Move => s[a] = fact(&s, ins.b),
            Op::Add | Op::Sub | Op::Mul | Op::Div | Op::Mod | Op::Pow => {
                if fact(&s, ins.b) != TypeHint::Num || fact(&s, ins.c) != TypeHint::Num {
                    return reject("arithmetic on non-number");
                }
                s[a] = TypeHint::Num;
            }
            Op::Neg => {
                if fact(&s, ins.b) != TypeHint::Num {
                    return reject("negate non-number");
                }
                s[a] = TypeHint::Num;
            }
            Op::BitAnd | Op::BitOr | Op::BitXor | Op::Shl | Op::Shr | Op::UShr => {
                if fact(&s, ins.b) != TypeHint::Num || fact(&s, ins.c) != TypeHint::Num {
                    return reject("bitwise on non-number");
                }
                s[a] = TypeHint::Num;
            }
            Op::BitNot => {
                if fact(&s, ins.b) != TypeHint::Num {
                    return reject("bitwise on non-number");
                }
                s[a] = TypeHint::Num;
            }
            Op::Eq | Op::Ne | Op::Lt | Op::Le | Op::Gt | Op::Ge => {
                if fact(&s, ins.b) != TypeHint::Num || fact(&s, ins.c) != TypeHint::Num {
                    return reject("compare non-numbers");
                }
                s[a] = TypeHint::Bool;
            }
            Op::EqSkip | Op::NeSkip | Op::LtSkip | Op::LeSkip | Op::GtSkip | Op::GeSkip => {
                if fact(&s, ins.b) != TypeHint::Num || fact(&s, ins.c) != TypeHint::Num {
                    return reject("compare non-numbers");
                }
                // successors: fallthrough (pc+1, the Jump) and skip (pc+2)
                next = vec![pc + 1, pc + 2];
            }
            Op::Jump => {
                next = vec![(pc as i64 + ins.sbx() as i64 + 1) as usize];
            }
            Op::JumpIfFalse | Op::JumpIfTrue => {
                let f = fact(&s, ins.a);
                if f != TypeHint::Num && f != TypeHint::Bool {
                    return reject("branch on non-primitive");
                }
                jumpif_num[pc] = f == TypeHint::Num;
                next = vec![pc + 1, (pc as i64 + ins.sbx() as i64 + 1) as usize];
            }
            // Op::Call intentionally unsupported: spill-everything at call
            // sites made call-in-loop functions slower than Tier-1
            // (ponytail: liveness-based spilling when it earns its keep)
            Op::Return => {
                next = vec![];
            }
            _ => return reject("unsupported op"),
        }
        for t in next {
            merge(&mut states, &mut work, t, &s);
        }
    }

    TypedProto { tier2_ok: true, reason: "", jumpif_num }
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
    fn accepts_numeric_loop() {
        // r1 = 0; loop: r1 = r1 + r0; jump back; return r1
        let p = proto(
            1,
            vec![TypeHint::Num],
            vec![
                Instr::asbx(Op::LoadInt, 1, 0),
                Instr::abc(Op::Add, 1, 1, 0),
                Instr::abc(Op::Return, 1, 0, 0),
            ],
        );
        assert!(analyze(&p).tier2_ok);
    }

    #[test]
    fn rejects_unannotated() {
        let p = proto(
            1,
            vec![TypeHint::Top],
            vec![Instr::abc(Op::Return, 0, 0, 0)],
        );
        assert!(!analyze(&p).tier2_ok);
    }
}
