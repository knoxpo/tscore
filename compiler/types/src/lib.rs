//! tsc-types
//!
//! Forward type dataflow over linear bytecode. The optimizing compiler compiles
//! every op (guarded templates when types unproven), so analysis never
//! rejects on op kind — facts only pick which sites get unguarded,
//! unboxed FP lanes. Only async functions reject.

use tsc_ir::{Const, FunctionProto, Op, TypeHint};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CondFact {
    /// A double for certain (or any number when small ints are off).
    Num,
    /// An int-tagged value for certain.
    Int,
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
    /// Per-arg representation the entry guard pins when `arg_guard` is
    /// set: 0 any number, 1 int-tagged (an integral double converts in
    /// place), 2 double (an int converts in place).
    pub arg_repr: Vec<u8>,
    /// Loop-header representation pinning: (header pc, vregs) that join
    /// an int fall-in with double back-edges. Inside the loop they flow
    /// as Dbl; the fall-in edge (and OSR entry) converts an int-tagged
    /// value to a double and deopts on a non-number.
    pub dbl_hdrs: Vec<(usize, Vec<u8>)>,
    /// Per back-edge header: every vreg the analysis holds as a double
    /// there. The fall-in edge needs no guard (the same analysis typed
    /// the value on that edge), but an OSR entry does — the interpreter
    /// hands over whatever representation it happened to produce.
    pub dbl_osr: Vec<(usize, Vec<u8>)>,
    /// Loop-header speculation: (header pc, vregs to number-guard on the
    /// preheader edge). Inside the loop these flow as proven Num; the
    /// compiler must guard the fall-in path (and OSR entries) and deopt
    /// to the header on a miss.
    pub loop_spec: Vec<(usize, Vec<u8>)>,
    /// Per back-edge header: every vreg the analysis holds as an integer
    /// there. An OSR entry must make each one int-tagged (an integral
    /// double converts, anything else deopts): the interpreter's frame
    /// may carry a double where compiled code assumes int bits.
    pub int_hdrs: Vec<(usize, Vec<u8>)>,
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
    /// Per-pc (a,b,c): operand is a double for certain (never int-tagged),
    /// so the unguarded FP lane may read its bits as an f64. Without
    /// small ints every number is a double and this equals `num_facts`.
    pub dbl_facts: Vec<[bool; 3]>,
    /// Per NewObjectLit pc: class of each literal value in order —
    /// 0 proven i32 integer, 1 proven number, 2 unknown.
    pub lit_vals: Vec<Vec<u8>>,
}

/// Field representation as the caller reports it per pc, for GetField
/// and SetField sites with a warm monomorphic cache: 0 Int32, 1 Number,
/// 2 Any / unknown.
/// Element kinds a GetIndex site may be specialized to, from the array
/// cell's kind (see `tsr_memory::cells`): what every array that reached
/// the site during warmup was made of.
pub const EK_I32: u8 = 0;
pub const EK_NUM: u8 = 1;
pub const EK_ANY: u8 = 2;

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
    /// A double for certain: a fractional constant, a quotient, or
    /// arithmetic with a double operand. Never an int-tagged value.
    Dbl,
    /// Numeric, representation unknown (int-tagged or double).
    Num,
    Bool,
    Top,
}

impl T {
    fn is_num(self) -> bool {
        matches!(self, T::Num | T::Int(_) | T::IntV | T::Dbl)
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
        return T::Num; // int with double, or two different constants
    }
    T::Top
}

/// Result representation of `+ - *` on two numeric operands.
fn arith(a: T, b: T, int_static: bool) -> T {
    if a == T::Dbl && b.is_num() || b == T::Dbl && a.is_num() {
        T::Dbl
    } else if int_static && a.is_int() && b.is_int() {
        T::IntV // the compiled op deopts on overflow
    } else if a.is_num() && b.is_num() {
        // A withdrawn destination is compiled as double arithmetic (the
        // emitter converts both operands), so it really is a double —
        // but only where every entry into the region converts it, which
        // is why `dbl_hdrs` carries Dbl-typed vregs at each loop header
        // for the fall-in edge and for OSR. Typing this `Dbl` without
        // that guard let compiled code read an int-tagged home as a
        // double and hung `acc += i`.
        if int_static { T::Num } else { T::Dbl }
    } else {
        T::Top
    }
}

/// `smi`: small ints exist at runtime (Len, bitwise ops and integral
/// constants are int-tagged). `int_static`: type `IntV op IntV` as
/// `IntV`, which the compiled op enforces with an overflow deopt;
/// withdrawn (`Num`) after those deopts repeat.
pub fn analyze(proto: &FunctionProto, field_repr: &[u8], smi: bool, no_int: u64) -> TypedProto {
    analyze_with(proto, field_repr, &[], smi, no_int)
}

pub fn analyze_with(
    proto: &FunctionProto,
    field_repr: &[u8],
    elem_kind: &[u8],
    smi: bool,
    no_int: u64,
) -> TypedProto {
    // integer speculation is per value: a vreg whose arithmetic already
    // overflowed i32 at run time is typed `Num` and compiled as doubles,
    // while the counters beside it keep their integer arms
    let int_ok = |a: usize| smi && (no_int >> (a as u32).min(63)) & 1 == 0;
    let reject = |reason: &'static str| TypedProto {
        opt_ok: false,
        reason,
        num_facts: Vec::new(),
        jumpif: Vec::new(),
        const_ops: Vec::new(),
        arg_guard: Vec::new(),
        arg_repr: Vec::new(),
        dbl_hdrs: Vec::new(),
        dbl_osr: Vec::new(),
        loop_spec: Vec::new(),
        int_hdrs: Vec::new(),
        int_spec: Vec::new(),
        int_facts: Vec::new(),
        dbl_facts: Vec::new(),
        lit_vals: Vec::new(),
    };
    let repr_at = |pc: usize| field_repr.get(pc).copied().unwrap_or(REPR_ANY);
    let ekind_at = |pc: usize| elem_kind.get(pc).copied().unwrap_or(EK_ANY);
    if proto.arity > 8 {
        return reject("arity > 8 (unprofiled)");
    }

    // entry: param proven Num by annotation or uniform runtime feedback
    // (entry guard enforces; unproven params flow as Top, no guard)
    // arg_seen classes: 1 int, 4 double, 2 anything else (OR-ed)
    let mut arg_guard = vec![false; proto.arity as usize];
    let mut arg_repr = vec![0u8; proto.arity as usize];
    for (i, g) in arg_guard.iter_mut().enumerate() {
        let annotated = proto.arg_types.get(i) == Some(&TypeHint::Num);
        let seen = proto.jit.arg_seen[i].load(std::sync::atomic::Ordering::Relaxed);
        *g = annotated || matches!(seen, 1 | 4 | 5);
        arg_repr[i] = match seen {
            1 if smi => 1,
            4 => 2,
            _ => 0,
        };
    }

    let body = proto.body();
    let n = body.code.len();
    let nregs = body.n_regs as usize;
    let mut states: Vec<Option<Vec<T>>> = vec![None; n + 1];
    let mut entry = vec![T::Top; nregs];
    for (i, &g) in arg_guard.iter().enumerate() {
        if g {
            entry[i] = match arg_repr[i] {
                1 => T::IntV,
                2 => T::Dbl,
                _ => T::Num,
            };
        }
    }
    states[0] = Some(entry.clone());
    let mut work = vec![0usize];
    let mut num_facts = vec![[false; 3]; n];
    let mut dbl_facts = vec![[false; 3]; n];
    let mut jumpif = vec![CondFact::Other; n];
    let mut const_ops: Vec<[Option<i64>; 2]> = vec![[None; 2]; n];
    // loop-header speculation state (filled between the two passes):
    // pinned[H] = vregs forced to a type at header H's merge: Num (the
    // preheader guard enforces numeric) or Dbl (the preheader converts)
    let mut pinned: std::collections::HashMap<usize, Vec<(u8, T)>> =
        std::collections::HashMap::new();

    let merge = |states: &mut Vec<Option<Vec<T>>>,
                 work: &mut Vec<usize>,
                 pinned: &std::collections::HashMap<usize, Vec<(u8, T)>>,
                 t: usize,
                 s: &[T]| {
        let pins = pinned.get(&t);
        let pin = |r: usize, v: T| -> T {
            match pins.and_then(|p| p.iter().find(|(pr, _)| *pr as usize == r)) {
                Some(&(_, ty)) => ty, // speculated: the preheader enforces this
                None => v,
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
    // headers pinned for them (a third pass if a Dbl pin failed to hold)
    // ----
    for pass in 0..3 {
        if pass == 2 {
            // verify Dbl pins: every back-edge must carry Dbl now
            let mut bad = false;
            for (pc, ins) in body.code.iter().enumerate() {
                if ins.op != Op::Jump || ins.sbx() >= 0 {
                    continue;
                }
                let h = (pc as i64 + ins.sbx() as i64 + 1) as usize;
                if let (Some(vs), Some(es)) = (pinned.get_mut(&h), &states[pc]) {
                    let n0 = vs.len();
                    vs.retain(|&(r, ty)| ty != T::Dbl || es[r as usize] == T::Dbl);
                    bad |= vs.len() != n0;
                }
            }
            if !bad {
                break;
            }
            pinned.retain(|_, vs| !vs.is_empty());
        }
        if pass == 1 {
            // candidates: back-edge target H where states[H][v] joined to
            // Top but every back-edge source carries Num for v (pin Num),
            // or joined to Num while every back-edge carries Dbl (pin Dbl:
            // `let zr = 0` feeding a double recurrence)
            let mut spec: std::collections::HashMap<usize, Vec<(u8, T)>> =
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
                    let ty = if hs[r] == T::Top && es[r].is_num() {
                        T::Num
                    } else if smi && hs[r] == T::Num && es[r] == T::Dbl {
                        T::Dbl
                    } else {
                        continue;
                    };
                    let e = spec.entry(h).or_default();
                    if !e.iter().any(|(pr, _)| *pr as usize == r) {
                        e.push((r as u8, ty));
                    }
                }
            }
            // multiple back-edges to one header: require the type on ALL
            // of them
            for (pc, ins) in body.code.iter().enumerate() {
                if ins.op != Op::Jump || ins.sbx() >= 0 {
                    continue;
                }
                let h = (pc as i64 + ins.sbx() as i64 + 1) as usize;
                if let (Some(vs), Some(es)) = (spec.get_mut(&h), &states[pc]) {
                    vs.retain(|&(r, ty)| match ty {
                        T::Dbl => es[r as usize] == T::Dbl,
                        _ => es[r as usize].is_num(),
                    });
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
                vs.retain(|&(r, _)| {
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
        }
        if pass >= 1 {
            // reset and re-run the fixpoint with pinning
            states = vec![None; n + 1];
            states[0] = Some(entry.clone());
            work = vec![0usize];
            num_facts = vec![[false; 3]; n];
            dbl_facts = vec![[false; 3]; n];
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
        // a double for certain: with small ints off, every number is
        let dbl = |t: T| if smi { t == T::Dbl } else { t.is_num() };
        dbl_facts[pc] = [dbl(fact(&s, ins.a)), dbl(fact(&s, ins.b)), dbl(fact(&s, ins.c))];
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
                    Some(Const::Number(_)) => T::Dbl,
                    _ => T::Top,
                }
            }
            Op::LoadBool => s[a] = T::Bool,
            Op::LoadNull | Op::LoadUndef => s[a] = T::Top,
            Op::Move => s[a] = fact(&s, ins.b),
            // string concat possible unless both proven Num
            Op::Add => s[a] = arith(fact(&s, ins.b), fact(&s, ins.c), int_ok(a)),
            Op::Sub | Op::Mul => {
                let (b, c) = (fact(&s, ins.b), fact(&s, ins.c));
                // numeric result or runtime error — numeric either way
                s[a] = if b.is_num() && c.is_num() { arith(b, c, int_ok(a)) } else { T::Num };
            }
            Op::Div | Op::Pow => s[a] = T::Dbl,
            // -0 and i32::MIN leave the int range; the lane keeps its own
            // rules for the values it holds
            Op::Neg => s[a] = if fact(&s, ins.b) == T::Dbl { T::Dbl } else { T::Num },
            // int % int is an int except for -0 (negative dividend, zero
            // remainder) and x % 0: with speculation on, the compiled op
            // deopts on those and the result is an int; the divisor must
            // have a magic multiplier (|k| >= 2) or be a variable int
            Op::Mod => {
                let (b, c) = (fact(&s, ins.b), fact(&s, ins.c));
                let div_ok = match c {
                    T::Int(k) => k.unsigned_abs() >= 2 && c.is_int(),
                    T::IntV => true,
                    _ => false,
                };
                s[a] = if int_ok(a) && b.is_int() && div_ok { T::IntV } else { T::Num };
            }
            Op::UShr => s[a] = T::Num,
            // ToInt32 results are ints when small ints are on
            Op::BitAnd | Op::BitOr | Op::BitXor | Op::Shl | Op::Shr | Op::BitNot => {
                s[a] = if smi { T::IntV } else { T::Num };
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
                    x if x.is_int() && smi => CondFact::Int,
                    x if x.is_num() && (!smi || x == T::Dbl) => CondFact::Num,
                    T::Bool => CondFact::Bool,
                    _ => CondFact::Other,
                };
                next = vec![pc + 1, (pc as i64 + ins.sbx() as i64 + 1) as usize];
            }
            Op::Len => s[a] = if smi { T::IntV } else { T::Num },
            // a warm monomorphic field read: the shape's representation
            // for that slot is the result type (the site deopts on a miss)
            Op::GetField => {
                s[a] = match repr_at(pc) {
                    REPR_INT32 => T::IntV,
                    REPR_NUMBER => T::Num,
                    _ => T::Top,
                }
            }
            // a warm array site: every array seen here was made of one
            // kind, and the compiled site guards it, so the loaded
            // element carries that type instead of being unknown
            Op::GetIndex => {
                s[a] = match ekind_at(pc) {
                    EK_I32 if smi => T::IntV,
                    EK_I32 | EK_NUM => T::Num,
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

    let split = |ty: T| -> Vec<(usize, Vec<u8>)> {
        let mut v: Vec<(usize, Vec<u8>)> = pinned
            .iter()
            .map(|(h, vs)| (*h, vs.iter().filter(|(_, t)| *t == ty).map(|(r, _)| *r).collect()))
            .filter(|(_, vs): &(usize, Vec<u8>)| !vs.is_empty())
            .collect();
        v.sort_by_key(|(h, _)| *h);
        v
    };
    let loop_spec = split(T::Num);
    let dbl_hdrs = split(T::Dbl);

    // integer lane: loop-carried numeric vregs whose every in-loop write
    // keeps an integer an integer. Derived from the header state rather
    // than from `loop_spec` — a counter that was already proven Num never
    // needed speculation, and it is exactly what we want unboxed.
    //
    // Being a number is not enough to unbox (1.5 is a number), so the
    // compiler still owes an int32 guard at the header. That guard is
    // paid once per loop entry instead of per use.
    let mut int_spec: Vec<(usize, Vec<u8>)> = Vec::new();
    let mut int_hdrs: Vec<(usize, Vec<u8>)> = Vec::new();
    let mut dbl_at: Vec<(usize, Vec<u8>)> = Vec::new();
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
                    // an element of an int-only array
                    Op::GetIndex => ekind_at(pc2) == EK_I32,
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
            .filter(|&r| hs[r].is_num() && cand[r] && int_ok(r))
            .map(|r| r as u8)
            .collect();
        if !keep.is_empty() {
            int_spec.push((h, keep));
        }
        let ints: Vec<u8> = (0..nregs).filter(|&r| hs[r].is_int()).map(|r| r as u8).collect();
        if !ints.is_empty() {
            int_hdrs.push((h, ints));
        }
        // Mirror of the above for doubles: a vreg the analysis holds as
        // a double here must BE one on every edge into this header —
        // the fall-in guard covers the loop entry, this covers OSR,
        // where the interpreter hands over whatever it happened to make.
        let dbls: Vec<u8> = (0..nregs).filter(|&r| hs[r] == T::Dbl).map(|r| r as u8).collect();
        if !dbls.is_empty() {
            dbl_at.push((h, dbls));
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
        if ins.op == Op::NewArrayLit {
            lit_vals[pc] = (0..ins.c as usize)
                .map(|j| match s.get(ins.b as usize + j).copied() {
                    Some(t) if t.is_int() => 0,
                    Some(t) if t.is_num() => 1,
                    _ => 2,
                })
                .collect();
        }
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

    dbl_at.sort_by_key(|(h, _)| *h);

    TypedProto {
        opt_ok: true,
        reason: "",
        num_facts,
        jumpif,
        const_ops,
        arg_guard,
        arg_repr,
        dbl_hdrs,
        dbl_osr: dbl_at,
        loop_spec,
        int_hdrs,
        int_spec,
        int_facts,
        dbl_facts,
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
        let t = analyze(&p, &[], false, false);
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
        let t = analyze(&p, &[], false, false);
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
        let t = analyze(&p, &[], false, false);
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
        let t = analyze(&p, &[], false, false);
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
        let t = analyze(&p, &[], false, false);
        let vs = &t.int_spec.first().expect("a loop was found").1;
        assert!(vs.contains(&1), "counter should be unboxed: {vs:?}");
        assert!(!vs.contains(&0), "the parameter is never written: {vs:?}");
    }
}
