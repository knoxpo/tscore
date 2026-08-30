//! Register-machine bytecode interpreter over NaN-boxed values.

use crate::{Realm, RtError};
use std::sync::Arc;
use tsc_ir::{Const, FunctionProto, Op, UpvalSrc};
use tsr_memory::{
    to_int32, to_uint32, Coroutine, Foreign, Kind, PromiseError, PromiseState,
    Value,
};

/// Call any callable value with the given arguments (entry point for the
/// CLI and for natives like `parallel.map` that re-enter the interpreter).
pub fn call_value(realm: &mut Realm, f: Value, args: &[Value]) -> Result<Value, RtError> {
    let base = realm.stack.len();
    match f.kind() {
        Kind::Native(i) => {
            let native = realm.natives[i as usize].clone();
            native(realm, args)
        }
        Kind::Closure(c) => {
            let proto = realm.heap.closure(c).proto.clone();
            realm.stack.resize(base + proto.n_regs as usize, Value::UNDEFINED);
            let n = (proto.arity as usize).min(args.len());
            realm.stack[base..base + n].copy_from_slice(&args[..n]);
            if proto.is_async {
                let p = start_async(realm, Some(c), proto, base);
                realm.stack.truncate(base);
                return Ok(Value::foreign(p));
            }
            let result = run_one(realm, Some(c), &proto, base, 0);
            realm.stack.truncate(base);
            result
        }
        _ => Err(RtError::new(format!("{} is not a function", f.type_of()))),
    }
}

/// Run the top-level chunk in the realm.
/// Run the main chunk to completion: starts it as a coroutine (main is
/// compiled async for top-level await), then drives the event loop until
/// its promise settles.
pub fn run_main(realm: &mut Realm, main: &Arc<FunctionProto>) -> Result<Value, RtError> {
    let base = realm.stack.len();
    realm.stack.resize(base + main.n_regs as usize, Value::UNDEFINED);
    let main_promise = start_async(realm, None, main.clone(), base);
    realm.stack.truncate(base);
    realm.pinned.insert(main_promise);
    let result = drive(realm, main_promise);
    realm.pinned.remove(&main_promise);
    realm.publish_stats();
    result
}

/// Event loop: drain microtasks and cross-thread wakes until the given
/// promise settles.
pub fn drive(realm: &mut Realm, main_promise: tsr_memory::Ref) -> Result<Value, RtError> {
    let rx = realm.wake_rx.clone();
    loop {
        while let Some(co) = realm.microtasks.pop_front() {
            resume(realm, co);
        }
        while let Ok(wake) = rx.try_recv() {
            process_wake(realm, wake);
        }
        if !realm.microtasks.is_empty() {
            continue;
        }
        match &realm.heap.promise(main_promise).state {
            PromiseState::Fulfilled(v) => return Ok(*v),
            PromiseState::Rejected(e) => {
                return Err(RtError {
                    msg: e.msg.clone(),
                    span: e.span,
                    cancelled: e.cancelled,
                })
            }
            PromiseState::Pending => {
                if realm.external_pending == 0 {
                    return Err(RtError::new(
                        "deadlock: pending promises that nothing can settle",
                    ));
                }
                // help the pool while waiting (a zero-thread pool would
                // otherwise never run the jobs we're waiting on)
                if let Some(help) = realm.idle_helper {
                    help(&|| !rx.is_empty());
                    while let Ok(wake) = rx.try_recv() {
                        process_wake(realm, wake);
                    }
                } else {
                    match rx.recv() {
                        Ok(wake) => process_wake(realm, wake),
                        Err(_) => {
                            return Err(RtError::new("internal: wake channel closed"))
                        }
                    }
                }
            }
        }
    }
}

fn process_wake(realm: &mut Realm, wake: crate::Wake) {
    realm.external_pending = realm.external_pending.saturating_sub(1);
    let result = match wake.result {
        Ok(pv) => Ok(tsr_task::portable::rehydrate(&pv, &mut realm.heap)),
        Err(e) => Err(e),
    };
    settle(realm, wake.promise, result);
    realm.pinned.remove(&wake.promise);
}

/// Start an async function whose argument window is already populated at
/// `base`. Runs the body synchronously to its first await; returns the
/// promise (foreign ref) it will settle.
#[cold]
pub fn start_async(
    realm: &mut Realm,
    closure: Option<tsr_memory::Ref>,
    proto: Arc<FunctionProto>,
    base: usize,
) -> tsr_memory::Ref {
    let promise = realm.heap.alloc_promise();
    // the promise is otherwise unreachable while the body runs — a GC at
    // any safepoint inside would free it
    realm.pinned.insert(promise);
    match run_frame(realm, closure, &proto, base, 0, 0) {
        Ok(FrameResult::Return(v)) => settle(realm, promise, Ok(v)),
        Err(e) => settle(
            realm,
            promise,
            Err(PromiseError { msg: e.msg, cancelled: e.cancelled, span: e.span }),
        ),
        Ok(FrameResult::Await { awaited, dst, resume_pc }) => {
            let regs = realm.stack[base..base + proto.n_regs as usize].to_vec();
            let co = realm.heap.alloc_foreign(Foreign::Coroutine(Coroutine {
                closure,
                proto,
                regs,
                resume_pc,
                dst,
                promise,
            }));
            realm.heap.barrier_foreign(awaited);
            realm.heap.promise_mut(awaited).reactions.push(co);
        }
    }
    realm.pinned.remove(&promise);
    promise
}

/// Resume a suspended coroutine (its awaited value is already in
/// `regs[dst]`); settle or re-suspend.
fn resume(realm: &mut Realm, co_ref: tsr_memory::Ref) {
    let Foreign::Coroutine(mut co) =
        std::mem::replace(realm.heap.foreign_mut(co_ref), Foreign::Free)
    else {
        return; // already consumed
    };
    realm.heap.free_foreign(co_ref);
    // the coroutine left the arena: its promise is only reachable through
    // this Rust local until we settle/re-suspend — pin it across execution
    realm.pinned.insert(co.promise);
    let base = realm.stack.len();
    realm.stack.extend_from_slice(&co.regs);
    let result = run_frame(realm, co.closure, &co.proto.clone(), base, 0, co.resume_pc);
    match result {
        Ok(FrameResult::Return(v)) => {
            realm.stack.truncate(base);
            settle(realm, co.promise, Ok(v));
            realm.pinned.remove(&co.promise);
        }
        Err(e) => {
            realm.stack.truncate(base);
            settle(
                realm,
                co.promise,
                Err(PromiseError { msg: e.msg, cancelled: e.cancelled, span: e.span }),
            );
            realm.pinned.remove(&co.promise);
        }
        Ok(FrameResult::Await { awaited, dst, resume_pc }) => {
            let n = co.regs.len();
            co.regs.copy_from_slice(&realm.stack[base..base + n]);
            realm.stack.truncate(base);
            co.resume_pc = resume_pc;
            co.dst = dst;
            let promise = co.promise;
            let new_ref = realm.heap.alloc_foreign(Foreign::Coroutine(co));
            realm.heap.barrier_foreign(awaited);
            realm.heap.promise_mut(awaited).reactions.push(new_ref);
            realm.pinned.remove(&promise);
            return;
        }
    }
}

/// Settle a promise; fulfilled waiters get the value written into their
/// resume register and join the microtask queue, rejected waiters cascade.
pub fn settle(
    realm: &mut Realm,
    promise: tsr_memory::Ref,
    result: Result<Value, PromiseError>,
) {
    realm.heap.barrier_foreign(promise);
    let reactions = {
        let p = realm.heap.promise_mut(promise);
        debug_assert!(matches!(p.state, PromiseState::Pending));
        p.state = match &result {
            Ok(v) => PromiseState::Fulfilled(*v),
            Err(e) => PromiseState::Rejected(e.clone()),
        };
        std::mem::take(&mut p.reactions)
    };
    for co_ref in reactions {
        match &result {
            Ok(v) => {
                realm.heap.barrier_foreign(co_ref);
                if let Foreign::Coroutine(co) = realm.heap.foreign_mut(co_ref) {
                    let dst = co.dst as usize;
                    co.regs[dst] = *v;
                }
                realm.microtasks.push_back(co_ref);
            }
            Err(e) => {
                // cascade the rejection without executing the waiter
                let waiter_promise = match std::mem::replace(
                    realm.heap.foreign_mut(co_ref),
                    Foreign::Free,
                ) {
                    Foreign::Coroutine(co) => {
                        realm.heap.free_foreign(co_ref);
                        co.promise
                    }
                    other => {
                        *realm.heap.foreign_mut(co_ref) = other;
                        continue;
                    }
                };
                settle(realm, waiter_promise, Err(e.clone()));
            }
        }
    }
}

// Register access: indices are emitter-guaranteed < base + n_regs and the
// stack is sized to base + n_regs at frame entry (and only grows). Checked
// in debug builds, unchecked in release — bounds checks are the biggest
// per-instruction cost otherwise.
macro_rules! reg {
    ($realm:expr, $i:expr) => {{
        let i = $i;
        debug_assert!(i < $realm.stack.len());
        unsafe { *$realm.stack.get_unchecked(i) }
    }};
}
macro_rules! set_reg {
    ($realm:expr, $i:expr, $v:expr) => {{
        let i = $i;
        let v = $v;
        debug_assert!(i < $realm.stack.len());
        unsafe { *$realm.stack.get_unchecked_mut(i) = v }
    }};
}

macro_rules! num_bin {
    ($realm:ident, $proto:ident, $pc:ident, $ins:ident, $base:ident, $f:expr) => {{
        let b = reg!($realm, $base + $ins.b as usize);
        let c = reg!($realm, $base + $ins.c as usize);
        if b.is_number() && c.is_number() {
            set_reg!(
                $realm,
                $base + $ins.a as usize,
                Value::number($f(b.as_number(), c.as_number()))
            );
        } else {
            return Err(err($proto, $pc, format!(
                "cannot apply arithmetic to {} and {}", b.type_of(), c.type_of())));
        }
    }};
}

macro_rules! int_bin {
    ($realm:ident, $ins:ident, $base:ident, $f:expr) => {{
        let x = to_num(reg!($realm, $base + $ins.b as usize));
        let y = to_num(reg!($realm, $base + $ins.c as usize));
        // int-derived results are never NaN
        set_reg!($realm, $base + $ins.a as usize, Value::number_unchecked($f(x, y)));
    }};
}

/// Evaluate a comparison; errors on non-number/non-string operand mixes.
macro_rules! cmp_eval {
    ($realm:ident, $proto:ident, $pc:ident, $b:ident, $c:ident, $nf:expr, $sf:expr) => {{
        if $b.is_number() && $c.is_number() {
            $nf(&$b.as_number(), &$c.as_number())
        } else if let (Some(x), Some(y)) = ($b.as_str_ref(), $c.as_str_ref()) {
            $sf(&*$realm.heap.str_at(x).clone(), &*$realm.heap.str_at(y).clone())
        } else {
            return Err(err($proto, $pc, format!(
                "cannot compare {} and {}", $b.type_of(), $c.type_of())));
        }
    }};
}

macro_rules! cmp_bin {
    ($realm:ident, $proto:ident, $pc:ident, $ins:ident, $base:ident, $nf:expr, $sf:expr) => {{
        let b = reg!($realm, $base + $ins.b as usize);
        let c = reg!($realm, $base + $ins.c as usize);
        let r = cmp_eval!($realm, $proto, $pc, b, c, $nf, $sf);
        set_reg!($realm, $base + $ins.a as usize, Value::bool(r));
    }};
}

macro_rules! cmp_skip {
    ($realm:ident, $proto:ident, $pc:ident, $ins:ident, $base:ident, $nf:expr, $sf:expr) => {{
        let b = reg!($realm, $base + $ins.b as usize);
        let c = reg!($realm, $base + $ins.c as usize);
        if cmp_eval!($realm, $proto, $pc, b, c, $nf, $sf) {
            $pc += 1;
        }
    }};
}

#[inline(always)]
fn to_num(v: Value) -> f64 {
    if v.is_number() {
        v.as_number()
    } else {
        f64::NAN
    }
}

fn err(proto: &FunctionProto, pc: usize, msg: String) -> RtError {
    RtError { msg, span: proto.spans.get(pc).copied(), cancelled: false }
}

/// Attach a source span to an existing error, keeping its cancelled flag.
fn at(mut e: RtError, proto: &FunctionProto, pc: usize) -> RtError {
    if e.span.is_none() {
        e.span = proto.spans.get(pc).copied();
    }
    e
}

/// Borrow a name constant — no Arc clone on the hot path (shared-refcount
/// traffic across worker threads kills scaling).
fn const_str(proto: &FunctionProto, idx: usize) -> &str {
    match &proto.consts[idx] {
        Const::Str(s) => s,
        Const::Number(_) => "",
    }
}

fn const_str_arc(proto: &FunctionProto, idx: usize) -> Arc<str> {
    match &proto.consts[idx] {
        Const::Str(s) => s.clone(),
        Const::Number(n) => Arc::from(tsr_memory::fmt_number(*n)),
    }
}

/// Matches Node's ~10k default; also keeps Rust stack use bounded on
/// worker/actor threads (which get 16MB stacks).
pub const MAX_CALL_DEPTH: u32 = 10_000;

/// Run a non-async closure frame to completion (JIT if hot, else
/// interpreter). Arguments already placed at `base`.
pub fn run_one(
    realm: &mut Realm,
    closure: Option<tsr_memory::Ref>,
    proto: &Arc<FunctionProto>,
    base: usize,
    depth: u32,
) -> Result<Value, RtError> {
    if realm.jit_enabled {
        // type feedback while cold: which arg tags does this fn really see?
        if proto.jit.tier.load(std::sync::atomic::Ordering::Relaxed) == tsc_ir::TIER_COLD {
            for i in 0..(proto.arity as usize).min(8) {
                let class = if realm.stack[base + i].is_number() { 1 } else { 2 };
                proto.jit.arg_seen[i].fetch_or(class, std::sync::atomic::Ordering::Relaxed);
            }
        }
        if let Some(f) = crate::jit::tier_up(proto) {
            return crate::jit::enter_jit(realm, f, proto, base, closure, depth);
        }
    }
    match run_frame(realm, closure, proto, base, depth, 0)? {
        FrameResult::Return(v) => Ok(v),
        FrameResult::Await { .. } => unreachable!("await in sync frame"),
    }
}

/// Resume a frame at an arbitrary pc (Tier-2 deopt continuation).
pub fn run_frame_pub(
    realm: &mut Realm,
    closure: Option<tsr_memory::Ref>,
    proto: &Arc<FunctionProto>,
    base: usize,
    depth: u32,
    start_pc: usize,
) -> Result<FrameResult, RtError> {
    run_frame(realm, closure, proto, base, depth, start_pc)
}

/// Outcome of one frame execution: normal return, or suspension at an
/// `await` on a pending promise (async frames only).
pub enum FrameResult {
    Return(Value),
    Await {
        /// The pending promise (foreign ref) being awaited.
        awaited: tsr_memory::Ref,
        dst: u8,
        resume_pc: usize,
    },
}

fn run_frame(
    realm: &mut Realm,
    closure: Option<tsr_memory::Ref>,
    proto: &Arc<FunctionProto>,
    base: usize,
    depth: u32,
    start_pc: usize,
) -> Result<FrameResult, RtError> {
    let mut pc: usize = start_pc;
    loop {
        debug_assert!(pc < proto.code.len());
        let ins = unsafe { *proto.code.get_unchecked(pc) };
        let a = base + ins.a as usize;
        match ins.op {
            Op::LoadConst => {
                let v = match &proto.consts[ins.bx() as usize] {
                    Const::Number(n) => Value::number(*n),
                    Const::Str(s) => {
                        let s = s.clone();
                        Value::str_ref(realm.heap.alloc_str(s))
                    }
                };
                set_reg!(realm, a, v);
            }
            Op::LoadInt => set_reg!(realm, a, Value::number_unchecked(ins.sbx() as f64)),
            Op::LoadBool => set_reg!(realm, a, Value::bool(ins.b != 0)),
            Op::LoadNull => set_reg!(realm, a, Value::NULL),
            Op::LoadUndef => set_reg!(realm, a, Value::UNDEFINED),
            Op::Move => set_reg!(realm, a, reg!(realm, base + ins.b as usize)),

            Op::Add => {
                let b = reg!(realm, base + ins.b as usize);
                let c = reg!(realm, base + ins.c as usize);
                if b.is_number() && c.is_number() {
                    set_reg!(realm, a, Value::number(b.as_number() + c.as_number()));
                } else if b.as_str_ref().is_some() || c.as_str_ref().is_some() {
                    let s = format!("{}{}", b.display(&realm.heap), c.display(&realm.heap));
                    let v = realm.alloc_string(&s);
                    set_reg!(realm, a, v);
                } else {
                    return Err(err(proto, pc, format!(
                        "cannot add {} and {}", b.type_of(), c.type_of())));
                }
            }
            Op::Sub => num_bin!(realm, proto, pc, ins, base, |x: f64, y: f64| x - y),
            Op::Mul => num_bin!(realm, proto, pc, ins, base, |x: f64, y: f64| x * y),
            Op::Div => num_bin!(realm, proto, pc, ins, base, |x: f64, y: f64| x / y),
            Op::Mod => num_bin!(realm, proto, pc, ins, base, |x: f64, y: f64| x % y),
            Op::Pow => num_bin!(realm, proto, pc, ins, base, |x: f64, y: f64| x.powf(y)),
            Op::Neg => {
                let b = reg!(realm, base + ins.b as usize);
                if b.is_number() {
                    set_reg!(realm, a, Value::number(-b.as_number()));
                } else {
                    return Err(err(proto, pc, format!("cannot negate {}", b.type_of())));
                }
            }

            Op::BitAnd => int_bin!(realm, ins, base, |x, y| (to_int32(x) & to_int32(y)) as f64),
            Op::BitOr => int_bin!(realm, ins, base, |x, y| (to_int32(x) | to_int32(y)) as f64),
            Op::BitXor => int_bin!(realm, ins, base, |x, y| (to_int32(x) ^ to_int32(y)) as f64),
            Op::Shl => int_bin!(realm, ins, base, |x, y| {
                (to_int32(x) << (to_uint32(y) & 31)) as f64
            }),
            Op::Shr => int_bin!(realm, ins, base, |x, y| {
                (to_int32(x) >> (to_uint32(y) & 31)) as f64
            }),
            Op::UShr => int_bin!(realm, ins, base, |x, y| {
                (to_uint32(x) >> (to_uint32(y) & 31)) as f64
            }),
            Op::BitNot => {
                let x = to_num(reg!(realm, base + ins.b as usize));
                set_reg!(realm, a, Value::number_unchecked(!to_int32(x) as f64));
            }

            Op::Eq => {
                let b = reg!(realm, base + ins.b as usize);
                let c = reg!(realm, base + ins.c as usize);
                set_reg!(realm, a, Value::bool(b.strict_eq(c, &realm.heap)));
            }
            Op::Ne => {
                let b = reg!(realm, base + ins.b as usize);
                let c = reg!(realm, base + ins.c as usize);
                set_reg!(realm, a, Value::bool(!b.strict_eq(c, &realm.heap)));
            }
            Op::EqSkip => {
                let b = reg!(realm, base + ins.b as usize);
                let c = reg!(realm, base + ins.c as usize);
                if b.strict_eq(c, &realm.heap) {
                    pc += 1;
                }
            }
            Op::NeSkip => {
                let b = reg!(realm, base + ins.b as usize);
                let c = reg!(realm, base + ins.c as usize);
                if !b.strict_eq(c, &realm.heap) {
                    pc += 1;
                }
            }
            Op::LtSkip => cmp_skip!(realm, proto, pc, ins, base, |x, y| f64::lt(x, y), str::lt),
            Op::LeSkip => cmp_skip!(realm, proto, pc, ins, base, |x, y| f64::le(x, y), str::le),
            Op::GtSkip => cmp_skip!(realm, proto, pc, ins, base, |x, y| f64::gt(x, y), str::gt),
            Op::GeSkip => cmp_skip!(realm, proto, pc, ins, base, |x, y| f64::ge(x, y), str::ge),
            Op::Lt => cmp_bin!(realm, proto, pc, ins, base, |x, y| f64::lt(x, y), str::lt),
            Op::Le => cmp_bin!(realm, proto, pc, ins, base, |x, y| f64::le(x, y), str::le),
            Op::Gt => cmp_bin!(realm, proto, pc, ins, base, |x, y| f64::gt(x, y), str::gt),
            Op::Ge => cmp_bin!(realm, proto, pc, ins, base, |x, y| f64::ge(x, y), str::ge),
            Op::Not => {
                let b = reg!(realm, base + ins.b as usize);
                set_reg!(realm, a, Value::bool(!b.truthy(&realm.heap)));
            }

            Op::Jump => {
                if ins.sbx() < 0 {
                    // loop back-edge: cancellation + GC safepoint
                    realm.safepoint().map_err(|e| at(e, proto, pc))?;
                    // OSR: hand a hot interpreted loop to Tier-1 native code
                    // (slot-resident registers make any header enterable).
                    // Fast path is one load + one store; u32::MAX marks
                    // permanently rejected protos.
                    if realm.jit_enabled && !proto.is_async {
                        use std::sync::atomic::Ordering::Relaxed;
                        let be = proto.jit.backedges.load(Relaxed);
                        if be != u32::MAX {
                            proto.jit.backedges.store(be.wrapping_add(1), Relaxed);
                            if be >= crate::jit::osr_threshold_pub() {
                                if let Some(f) = crate::jit::osr_slow(proto) {
                                    let target =
                                        (pc as i64 + ins.sbx() as i64 + 1) as usize;
                                    let v = crate::jit::enter_osr(
                                        realm, f, proto, base, closure, depth, target,
                                    )?;
                                    return Ok(FrameResult::Return(v));
                                }
                            }
                        }
                    }
                }
                pc = (pc as i64 + ins.sbx() as i64) as usize;
            }
            Op::JumpIfFalse => {
                if !reg!(realm, a).truthy(&realm.heap) {
                    pc = (pc as i64 + ins.sbx() as i64) as usize;
                }
            }
            Op::JumpIfTrue => {
                if reg!(realm, a).truthy(&realm.heap) {
                    pc = (pc as i64 + ins.sbx() as i64) as usize;
                }
            }

            Op::Call => {
                let f = reg!(realm, a);
                let argc = ins.b as usize;
                if let Some(c) = f.as_closure() {
                    realm.safepoint().map_err(|e| at(e, proto, pc))?;
                    if depth >= MAX_CALL_DEPTH {
                        return Err(err(proto, pc,
                            "stack overflow: maximum call depth exceeded".into()));
                    }
                    let callee = realm.heap.closure(c).proto.clone();
                    let new_base = a + 1;
                    let need = new_base + callee.n_regs as usize;
                    if realm.stack.len() < need {
                        realm.stack.resize(need, Value::UNDEFINED);
                    }
                    // missing args become undefined; other registers are
                    // def-before-use by the emitter (stale values only
                    // over-retain during GC, never misread)
                    for r in argc..callee.arity as usize {
                        set_reg!(realm, new_base + r, Value::UNDEFINED);
                    }
                    let result = if callee.is_async {
                        Value::foreign(start_async(realm, Some(c), callee, new_base))
                    } else {
                        // run_one tiers up to JIT when the callee is hot
                        run_one(realm, Some(c), &callee, new_base, depth + 1)?
                    };
                    set_reg!(realm, a, result);
                } else if let Kind::Native(i) = f.kind() {
                    let native = realm.natives[i as usize].clone();
                    // args on the Rust stack: no per-call Vec alloc
                    let mut buf = [Value::UNDEFINED; 8];
                    let result = if argc <= 8 {
                        buf[..argc].copy_from_slice(&realm.stack[a + 1..a + 1 + argc]);
                        native(realm, &buf[..argc])
                    } else {
                        let args: Vec<Value> = realm.stack[a + 1..a + 1 + argc].to_vec();
                        native(realm, &args)
                    }
                    .map_err(|e| err(proto, pc, e.msg))?;
                    set_reg!(realm, a, result);
                } else {
                    return Err(err(proto, pc, format!(
                        "{} is not a function", f.type_of())));
                }
            }
            Op::Return => return Ok(FrameResult::Return(reg!(realm, a))),
            Op::Halt => return Ok(FrameResult::Return(Value::UNDEFINED)),

            Op::Await => {
                let v = reg!(realm, a);
                if let Some(r) = v.as_foreign() {
                    if let Foreign::Promise(p) = realm.heap.foreign(r) {
                        match &p.state {
                            PromiseState::Fulfilled(val) => {
                                let val = *val;
                                set_reg!(realm, a, val);
                            }
                            PromiseState::Rejected(e) => {
                                return Err(RtError {
                                    msg: e.msg.clone(),
                                    span: e.span.or_else(|| proto.spans.get(pc).copied()),
                                    cancelled: e.cancelled,
                                })
                            }
                            PromiseState::Pending => {
                                return Ok(FrameResult::Await {
                                    awaited: r,
                                    dst: ins.a,
                                    resume_pc: pc + 1,
                                })
                            }
                        }
                    }
                }
                // await of a non-promise is identity (documented divergence:
                // no microtask tick for settled/plain values)
            }

            Op::Closure => {
                let child = proto.protos[ins.bx() as usize].clone();
                let mut upvals_buf = [Value::UNDEFINED; 8];
                let mut upvals_vec;
                let n_up = child.upvals.len();
                let upvals: &mut [Value] = if n_up <= 8 {
                    &mut upvals_buf[..n_up]
                } else {
                    upvals_vec = vec![Value::UNDEFINED; n_up];
                    &mut upvals_vec
                };
                let mut up_i = 0;
                for u in &child.upvals {
                    let entry = match *u {
                        UpvalSrc::ParentLocal(reg) => {
                            let v = reg!(realm, base + reg as usize);
                            if v.as_cell().is_none() {
                                return Err(err(proto, pc,
                                    "internal: captured slot is not a cell".into()));
                            }
                            v
                        }
                        UpvalSrc::ParentLocalValue(reg) => {
                            // immutable capture: copy the value, no cell
                            reg!(realm, base + reg as usize)
                        }
                        UpvalSrc::ParentUpval(idx) => {
                            let c = closure.expect("upval capture outside closure");
                            realm.heap.closure(c).upvals[idx as usize]
                        }
                    };
                    upvals[up_i] = entry;
                    up_i += 1;
                }
                let r = realm.heap.alloc_closure_reuse(child, upvals);
                set_reg!(realm, a, Value::closure(r));
            }
            Op::NewCell => {
                let init = reg!(realm, a);
                let r = realm.heap.alloc_cell(init);
                set_reg!(realm, a, Value::cell(r));
            }
            Op::LoadCell => match reg!(realm, base + ins.b as usize).as_cell() {
                Some(r) => set_reg!(realm, a, *realm.heap.cell(r)),
                None => return Err(err(proto, pc, "internal: LoadCell on non-cell".into())),
            },
            Op::StoreCell => match reg!(realm, a).as_cell() {
                Some(r) => {
                    realm.heap.barrier_cell(r);
                    *realm.heap.cell_mut(r) = reg!(realm, base + ins.b as usize)
                }
                None => return Err(err(proto, pc, "internal: StoreCell on non-cell".into())),
            },
            Op::GetUpval => {
                let c = closure.expect("GetUpval outside closure");
                let entry = realm.heap.closure(c).upvals[ins.b as usize];
                let v = match entry.as_cell() {
                    Some(r) => *realm.heap.cell(r),
                    None => entry, // immutable value capture
                };
                set_reg!(realm, a, v);
            }
            Op::SetUpval => {
                let c = closure.expect("SetUpval outside closure");
                let entry = realm.heap.closure(c).upvals[ins.a as usize];
                let cell = entry.as_cell().expect("SetUpval on value capture");
                realm.heap.barrier_cell(cell);
                *realm.heap.cell_mut(cell) = reg!(realm, base + ins.b as usize);
            }

            Op::NewObject => {
                let r = realm.heap.alloc_obj_empty();
                set_reg!(realm, a, Value::object(r));
            }
            Op::NewArray => {
                let r = realm.heap.alloc_arr_empty(ins.b as usize);
                set_reg!(realm, a, Value::array(r));
            }
            Op::GetField => {
                let obj = reg!(realm, base + ins.b as usize);
                if let Some(o) = obj.as_object() {
                    let objref = &realm.heap.objs[o as usize];
                    let sid = objref.shape.id;
                    let ic = proto.jit.ic_load(proto.code.len(), pc);
                    let v = if ic != 0 && (ic >> 32) as u32 == sid {
                        objref.values[(ic & 0xFFFF_FFFF) as usize - 1]
                    } else {
                        let name = const_str(proto, ins.c as usize);
                        match objref.shape.slot_of(name) {
                            Some(i) => {
                                proto.jit.ic_store(proto.code.len(), pc, sid, i);
                                objref.values[i]
                            }
                            None => Value::UNDEFINED,
                        }
                    };
                    set_reg!(realm, a, v);
                } else {
                    let name = const_str(proto, ins.c as usize);
                    let v = get_field(realm, obj, name).map_err(|e| err(proto, pc, e.msg))?;
                    set_reg!(realm, a, v);
                }
            }
            Op::SetField => {
                let v = reg!(realm, base + ins.c as usize);
                match reg!(realm, a).as_object() {
                    Some(r) => {
                        realm.heap.barrier_obj(r);
                        let objref = &mut realm.heap.objs[r as usize];
                        let sid = objref.shape.id;
                        let ic = proto.jit.ic_load(proto.code.len(), pc);
                        if ic != 0 && (ic >> 32) as u32 == sid {
                            objref.values[(ic & 0xFFFF_FFFF) as usize - 1] = v;
                        } else {
                            let name = const_str(proto, ins.b as usize);
                            match objref.shape.slot_of(name) {
                                Some(i) => {
                                    proto.jit.ic_store(proto.code.len(), pc, sid, i);
                                    objref.values[i] = v;
                                }
                                None => {
                                    let name = const_str_arc(proto, ins.b as usize);
                                    objref.shape = objref.shape.with_field(name);
                                    objref.values.push(v);
                                }
                            }
                        }
                    }
                    None => return Err(err(proto, pc, format!(
                        "cannot set property on {}", reg!(realm, a).type_of()))),
                }
            }
            Op::GetIndex => {
                let obj = reg!(realm, base + ins.b as usize);
                let idx = reg!(realm, base + ins.c as usize);
                let v = if idx.is_number() {
                    match obj.as_array() {
                        Some(r) => tsr_memory::array_index(idx.as_number())
                            .and_then(|i| realm.heap.arr(r).get(i).copied())
                            .unwrap_or(Value::UNDEFINED),
                        None => return Err(err(proto, pc, format!(
                            "cannot index {} with number", obj.type_of()))),
                    }
                } else if let (Some(_), Some(s)) = (obj.as_object(), idx.as_str_ref()) {
                    let name = realm.heap.str_arc(s);
                    get_field(realm, obj, &name).map_err(|e| err(proto, pc, e.msg))?
                } else {
                    return Err(err(proto, pc, format!(
                        "cannot index {} with {}", obj.type_of(), idx.type_of())));
                };
                set_reg!(realm, a, v);
            }
            Op::SetIndex => {
                let idx = reg!(realm, base + ins.b as usize);
                let v = reg!(realm, base + ins.c as usize);
                let target = reg!(realm, a);
                if idx.is_number() {
                    match target.as_array() {
                        Some(r) => {
                            realm.heap.barrier_arr(r);
                            let arr = realm.heap.arr_mut(r);
                            // non-index number keys (negative/fractional):
                            // array expando properties unsupported — ignored
                            if let Some(i) = tsr_memory::array_index(idx.as_number()) {
                                if i < arr.len() {
                                    arr[i] = v;
                                } else if i == arr.len() {
                                    arr.push(v);
                                    realm.heap.allocs_since_gc += 1;
                                } else {
                                    return Err(err(proto, pc,
                                        "sparse arrays not supported in M1".into()));
                                }
                            }
                        }
                        None => return Err(err(proto, pc, format!(
                            "cannot index-assign {} with number", target.type_of()))),
                    }
                } else if let (Some(r), Some(s)) = (target.as_object(), idx.as_str_ref()) {
                    let name = realm.heap.str_arc(s);
                    realm.heap.barrier_obj(r);
                    realm.heap.obj_mut(r).set(name, v);
                } else {
                    return Err(err(proto, pc, format!(
                        "cannot index-assign {} with {}", target.type_of(), idx.type_of())));
                }
            }
            Op::Len => {
                let b = reg!(realm, base + ins.b as usize);
                let v = if let Some(r) = b.as_array() {
                    Value::number(realm.heap.arr(r).len() as f64)
                } else if let Some(r) = b.as_str_ref() {
                    // ponytail: char count; UTF-16 length when strings grow up
                    Value::number(realm.heap.str_at(r).chars().count() as f64)
                } else {
                    return Err(err(proto, pc, format!("{} has no length", b.type_of())));
                };
                set_reg!(realm, a, v);
            }
            Op::ArrayPush => {
                let v = reg!(realm, base + ins.b as usize);
                match reg!(realm, a).as_array() {
                    Some(r) => {
                        // element growth is allocation pressure too
                        realm.heap.allocs_since_gc += 1;
                        realm.heap.barrier_arr(r);
                        realm.heap.arr_mut(r).push(v)
                    }
                    None => return Err(err(proto, pc, format!(
                        "cannot push onto {}", reg!(realm, a).type_of()))),
                }
            }

            Op::GetGlobal => {
                let name = const_str(proto, ins.bx() as usize);
                let v = *realm.globals.get(name).ok_or_else(|| {
                    err(proto, pc, format!("{name} is not defined"))
                })?;
                set_reg!(realm, a, v);
            }

            Op::Concat => {
                let b = reg!(realm, base + ins.b as usize);
                let c = reg!(realm, base + ins.c as usize);
                let mut s = String::with_capacity(16);
                if !tsr_memory::display_into(&mut s, b, &realm.heap)
                    || !tsr_memory::display_into(&mut s, c, &realm.heap)
                {
                    s.clear();
                    s.push_str(&b.display(&realm.heap));
                    s.push_str(&c.display(&realm.heap));
                }
                let v = realm.alloc_string(&s);
                set_reg!(realm, a, v);
            }
            Op::TypeOf => {
                let b = reg!(realm, base + ins.b as usize);
                let v = realm.alloc_string(b.type_of());
                set_reg!(realm, a, v);
            }
        }
        pc += 1;
    }
}

pub fn get_field_pub(realm: &mut Realm, obj: Value, name: &str) -> Result<Value, RtError> {
    get_field(realm, obj, name)
}

fn get_field(realm: &mut Realm, obj: Value, name: &str) -> Result<Value, RtError> {
    match obj.kind() {
        Kind::Object(r) => Ok(realm.heap.obj(r).get(name).unwrap_or(Value::UNDEFINED)),
        Kind::Array(r) if name == "length" => {
            Ok(Value::number(realm.heap.arr(r).len() as f64))
        }
        Kind::Str(r) if name == "length" => {
            Ok(Value::number(realm.heap.str_at(r).chars().count() as f64))
        }
        _ => Err(RtError::new(format!(
            "cannot read property '{name}' of {}", obj.type_of()))),
    }
}
