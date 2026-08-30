//! JIT bridge: helper shims the compiled code calls, tier management, and
//! `enter_jit`. Helper semantics deliberately mirror `interp::run_frame`
//! arm-for-arm — the differential test suite (interpreter vs JIT output on
//! every golden program) is the drift detector.

use crate::interp::{get_field_pub as get_field, start_async};
use crate::{Realm, RtError};
use std::sync::atomic::Ordering::{AcqRel, Acquire, Relaxed, Release};
use std::sync::Arc;
use tsc_ir::{Const, FunctionProto, Op, UpvalSrc, TIER_BASELINE, TIER_COLD, TIER_COMPILING, TIER_OPT, TIER_REJECTED};
use tsr_jit::tier1::{CompiledFn, Helpers, JitRet};
use tsr_memory::{to_int32, to_uint32, Closure, Kind, Value, JIT_ERR_SENTINEL};

fn realm<'a>(p: *mut core::ffi::c_void) -> &'a mut Realm {
    unsafe { &mut *(p as *mut Realm) }
}

fn proto<'a>(p: *const FunctionProto) -> &'a FunctionProto {
    unsafe { &*p }
}

fn ok(realm: &mut Realm, ctrl: u64) -> JitRet {
    JitRet { val: ctrl, stack: realm.stack.as_mut_ptr() as u64 }
}

fn fail(realm: &mut Realm, e: RtError) -> JitRet {
    realm.jit_error = Some(e);
    JitRet { val: JIT_ERR_SENTINEL, stack: realm.stack.as_mut_ptr() as u64 }
}

fn err_at(proto: &FunctionProto, pc: usize, msg: String) -> RtError {
    RtError { msg, span: proto.spans.get(pc).copied(), cancelled: false }
}

extern "C" fn h_stack_ptr(p: *mut core::ffi::c_void) -> JitRet {
    let r = realm(p);
    JitRet { val: 0, stack: r.stack.as_mut_ptr() as u64 }
}

extern "C" fn h_safepoint(p: *mut core::ffi::c_void) -> JitRet {
    let r = realm(p);
    match r.safepoint() {
        Ok(()) => ok(r, 0),
        Err(e) => fail(r, e),
    }
}

extern "C" fn h_call(
    p: *mut core::ffi::c_void,
    pp: *const FunctionProto,
    pc: u64,
    base_bytes: u64,
    a: u64,
    argc: u64,
    depth: u32,
) -> JitRet {
    let r = realm(p);
    let pr = proto(pp);
    let pc = pc as usize;
    let base = (base_bytes / 8) as usize;
    let abs_a = base + a as usize;
    let argc = argc as usize;
    let f = r.stack[abs_a];
    let result = (|| -> Result<Value, RtError> {
        if let Some(c) = f.as_closure() {
            r.safepoint()?;
            if depth >= crate::interp::MAX_CALL_DEPTH {
                return Err(err_at(pr, pc, "stack overflow: maximum call depth exceeded".into()));
            }
            let callee = r.heap.closure(c).proto.clone();
            let new_base = abs_a + 1;
            let need = new_base + callee.n_regs as usize;
            if r.stack.len() < need {
                r.stack.resize(need, Value::UNDEFINED);
            }
            for reg in argc..callee.arity as usize {
                r.stack[new_base + reg] = Value::UNDEFINED;
            }
            if callee.is_async {
                let pr_ref = start_async(r, Some(c), callee, new_base);
                return Ok(Value::foreign(pr_ref));
            }
            crate::interp::run_one(r, Some(c), &callee, new_base, depth + 1)
        } else if let Kind::Native(i) = f.kind() {
            let native = r.natives[i as usize].clone();
            // args on the Rust stack: no per-call heap alloc (hot path for
            // Math.* inside compiled loops)
            let mut buf = [Value::UNDEFINED; 8];
            let args_vec;
            let args: &[Value] = if argc <= 8 {
                buf[..argc].copy_from_slice(&r.stack[abs_a + 1..abs_a + 1 + argc]);
                &buf[..argc]
            } else {
                args_vec = r.stack[abs_a + 1..abs_a + 1 + argc].to_vec();
                &args_vec
            };
            native(r, args).map_err(|mut e| {
                if e.span.is_none() {
                    e.span = pr.spans.get(pc).copied();
                }
                e
            })
        } else {
            Err(err_at(pr, pc, format!("{} is not a function", f.type_of())))
        }
    })();
    match result {
        Ok(v) => {
            r.stack[abs_a] = v;
            ok(r, 0)
        }
        Err(e) => fail(r, e),
    }
}

/// Execute one instruction with interpreter semantics. Returns ctrl=1 for
/// "branch/skip taken" on control ops, 0 otherwise.
extern "C" fn h_step(
    p: *mut core::ffi::c_void,
    pp: *const FunctionProto,
    pc: u64,
    base_bytes: u64,
    closure: u32,
) -> JitRet {
    let r = realm(p);
    let pr = proto(pp);
    let pc = pc as usize;
    let base = (base_bytes / 8) as usize;
    let closure = if closure == u32::MAX { None } else { Some(closure) };
    match step(r, pr, pc, base, closure) {
        Ok(ctrl) => ok(r, ctrl),
        Err(e) => fail(r, e),
    }
}

fn step(
    realm: &mut Realm,
    proto: &FunctionProto,
    pc: usize,
    base: usize,
    closure: Option<u32>,
) -> Result<u64, RtError> {
    let ins = proto.code[pc];
    let a = base + ins.a as usize;
    let rb = |realm: &Realm| realm.stack[base + ins.b as usize];
    let rc = |realm: &Realm| realm.stack[base + ins.c as usize];
    let e = |msg: String| err_at(proto, pc, msg);

    macro_rules! num2 {
        ($f:expr) => {{
            let b = rb(realm);
            let c = rc(realm);
            if b.is_number() && c.is_number() {
                realm.stack[a] = Value::number($f(b.as_number(), c.as_number()));
                Ok(0)
            } else {
                Err(e(format!(
                    "cannot apply arithmetic to {} and {}",
                    b.type_of(),
                    c.type_of()
                )))
            }
        }};
    }
    macro_rules! int2 {
        ($f:expr) => {{
            let x = if rb(realm).is_number() { rb(realm).as_number() } else { f64::NAN };
            let y = if rc(realm).is_number() { rc(realm).as_number() } else { f64::NAN };
            realm.stack[a] = Value::number($f(x, y));
            Ok(0)
        }};
    }
    macro_rules! cmp2 {
        ($nf:expr, $sf:expr) => {{
            let b = rb(realm);
            let c = rc(realm);
            let r = if b.is_number() && c.is_number() {
                $nf(&b.as_number(), &c.as_number())
            } else if let (Some(x), Some(y)) = (b.as_str_ref(), c.as_str_ref()) {
                $sf(&*realm.heap.str_at(x).clone(), &*realm.heap.str_at(y).clone())
            } else {
                return Err(e(format!(
                    "cannot compare {} and {}",
                    b.type_of(),
                    c.type_of()
                )));
            };
            r
        }};
    }

    match ins.op {
        Op::LoadConst => {
            let v = match &proto.consts[ins.bx() as usize] {
                Const::Number(n) => Value::number(*n),
                Const::Str(s) => {
                    let s = s.clone();
                    Value::str_ref(realm.heap.alloc_str(s))
                }
            };
            realm.stack[a] = v;
            Ok(0)
        }
        Op::Add => {
            let b = rb(realm);
            let c = rc(realm);
            if b.is_number() && c.is_number() {
                realm.stack[a] = Value::number(b.as_number() + c.as_number());
                Ok(0)
            } else if b.as_str_ref().is_some() || c.as_str_ref().is_some() {
                let s = format!("{}{}", b.display(&realm.heap), c.display(&realm.heap));
                let v = realm.alloc_string(&s);
                realm.stack[a] = v;
                Ok(0)
            } else {
                Err(e(format!("cannot add {} and {}", b.type_of(), c.type_of())))
            }
        }
        Op::Sub => num2!(|x: f64, y: f64| x - y),
        Op::Mul => num2!(|x: f64, y: f64| x * y),
        Op::Div => num2!(|x: f64, y: f64| x / y),
        Op::Mod => num2!(|x: f64, y: f64| x % y),
        Op::Pow => num2!(|x: f64, y: f64| x.powf(y)),
        Op::Neg => {
            let b = rb(realm);
            if b.is_number() {
                realm.stack[a] = Value::number(-b.as_number());
                Ok(0)
            } else {
                Err(e(format!("cannot negate {}", b.type_of())))
            }
        }
        Op::BitAnd => int2!(|x, y| (to_int32(x) & to_int32(y)) as f64),
        Op::BitOr => int2!(|x, y| (to_int32(x) | to_int32(y)) as f64),
        Op::BitXor => int2!(|x, y| (to_int32(x) ^ to_int32(y)) as f64),
        Op::Shl => int2!(|x, y| (to_int32(x) << (to_uint32(y) & 31)) as f64),
        Op::Shr => int2!(|x, y| (to_int32(x) >> (to_uint32(y) & 31)) as f64),
        Op::UShr => int2!(|x, y| (to_uint32(x) >> (to_uint32(y) & 31)) as f64),
        Op::BitNot => {
            let x = if rb(realm).is_number() { rb(realm).as_number() } else { f64::NAN };
            realm.stack[a] = Value::number(!to_int32(x) as f64);
            Ok(0)
        }
        Op::Eq | Op::Ne => {
            let b = rb(realm);
            let c = rc(realm);
            let mut r = b.strict_eq(c, &realm.heap);
            if ins.op == Op::Ne {
                r = !r;
            }
            realm.stack[a] = Value::bool(r);
            Ok(0)
        }
        Op::EqSkip | Op::NeSkip => {
            let b = rb(realm);
            let c = rc(realm);
            let mut r = b.strict_eq(c, &realm.heap);
            if ins.op == Op::NeSkip {
                r = !r;
            }
            Ok(r as u64)
        }
        Op::Lt => {
            let r = cmp2!(|x, y| f64::lt(x, y), str::lt);
            realm.stack[a] = Value::bool(r);
            Ok(0)
        }
        Op::Le => {
            let r = cmp2!(|x, y| f64::le(x, y), str::le);
            realm.stack[a] = Value::bool(r);
            Ok(0)
        }
        Op::Gt => {
            let r = cmp2!(|x, y| f64::gt(x, y), str::gt);
            realm.stack[a] = Value::bool(r);
            Ok(0)
        }
        Op::Ge => {
            let r = cmp2!(|x, y| f64::ge(x, y), str::ge);
            realm.stack[a] = Value::bool(r);
            Ok(0)
        }
        Op::LtSkip => Ok(cmp2!(|x, y| f64::lt(x, y), str::lt) as u64),
        Op::LeSkip => Ok(cmp2!(|x, y| f64::le(x, y), str::le) as u64),
        Op::GtSkip => Ok(cmp2!(|x, y| f64::gt(x, y), str::gt) as u64),
        Op::GeSkip => Ok(cmp2!(|x, y| f64::ge(x, y), str::ge) as u64),
        Op::Not => {
            let b = rb(realm);
            realm.stack[a] = Value::bool(!b.truthy(&realm.heap));
            Ok(0)
        }
        Op::JumpIfFalse => {
            let taken = !realm.stack[a].truthy(&realm.heap);
            Ok(taken as u64)
        }
        Op::JumpIfTrue => {
            let taken = realm.stack[a].truthy(&realm.heap);
            Ok(taken as u64)
        }

        Op::Closure => {
            let child = proto.protos[ins.bx() as usize].clone();
            let mut upvals = Vec::with_capacity(child.upvals.len());
            for u in &child.upvals {
                let entry = match *u {
                    UpvalSrc::ParentLocal(reg) => {
                        let v = realm.stack[base + reg as usize];
                        if v.as_cell().is_none() {
                            return Err(e("internal: captured slot is not a cell".into()));
                        }
                        v
                    }
                    UpvalSrc::ParentLocalValue(reg) => realm.stack[base + reg as usize],
                    UpvalSrc::ParentUpval(idx) => {
                        let c = closure.expect("upval capture outside closure");
                        realm.heap.closure(c).upvals[idx as usize]
                    }
                };
                upvals.push(entry);
            }
            let r = realm.heap.alloc_closure(Closure { proto: child, upvals });
            realm.stack[a] = Value::closure(r);
            Ok(0)
        }
        Op::NewCell => {
            let init = realm.stack[a];
            let r = realm.heap.alloc_cell(init);
            realm.stack[a] = Value::cell(r);
            Ok(0)
        }
        Op::LoadCell => match rb(realm).as_cell() {
            Some(r) => {
                realm.stack[a] = *realm.heap.cell(r);
                Ok(0)
            }
            None => Err(e("internal: LoadCell on non-cell".into())),
        },
        Op::StoreCell => match realm.stack[a].as_cell() {
            Some(r) => {
                realm.heap.barrier_cell(r);
                *realm.heap.cell_mut(r) = rb(realm);
                Ok(0)
            }
            None => Err(e("internal: StoreCell on non-cell".into())),
        },
        Op::GetUpval => {
            let c = closure.expect("GetUpval outside closure");
            let entry = realm.heap.closure(c).upvals[ins.b as usize];
            realm.stack[a] = match entry.as_cell() {
                Some(r) => *realm.heap.cell(r),
                None => entry,
            };
            Ok(0)
        }
        Op::SetUpval => {
            let c = closure.expect("SetUpval outside closure");
            let entry = realm.heap.closure(c).upvals[ins.a as usize];
            let cell = entry.as_cell().expect("SetUpval on value capture");
            realm.heap.barrier_cell(cell);
            *realm.heap.cell_mut(cell) = rb(realm);
            Ok(0)
        }
        Op::NewObject => {
            let r = realm.heap.alloc_obj_empty();
            realm.stack[a] = Value::object(r);
            Ok(0)
        }
        Op::NewArray => {
            let r = realm.heap.alloc_arr_empty(ins.b as usize);
            realm.stack[a] = Value::array(r);
            Ok(0)
        }
        Op::GetField => {
            let obj = rb(realm);
            let name = match &proto.consts[ins.c as usize] {
                Const::Str(s) => s.clone(),
                Const::Number(n) => Arc::from(tsr_memory::fmt_number(*n)),
            };
            let v = get_field(realm, obj, &name).map_err(|er| e(er.msg))?;
            realm.stack[a] = v;
            Ok(0)
        }
        Op::SetField => {
            let name = match &proto.consts[ins.b as usize] {
                Const::Str(s) => s.clone(),
                Const::Number(n) => Arc::from(tsr_memory::fmt_number(*n)),
            };
            let v = rc(realm);
            match realm.stack[a].as_object() {
                Some(r) => {
                    realm.heap.barrier_obj(r);
                    realm.heap.obj_mut(r).set(name, v);
                    Ok(0)
                }
                None => Err(e(format!(
                    "cannot set property on {}",
                    realm.stack[a].type_of()
                ))),
            }
        }
        Op::GetIndex => {
            let obj = rb(realm);
            let idx = rc(realm);
            let v = if idx.is_number() {
                match obj.as_array() {
                    Some(r) => realm
                        .heap
                        .arr(r)
                        .get(idx.as_number() as usize)
                        .copied()
                        .unwrap_or(Value::UNDEFINED),
                    None => {
                        return Err(e(format!("cannot index {} with number", obj.type_of())))
                    }
                }
            } else if let (Some(_), Some(s)) = (obj.as_object(), idx.as_str_ref()) {
                let name = realm.heap.str_at(s).clone();
                get_field(realm, obj, &name).map_err(|er| e(er.msg))?
            } else {
                return Err(e(format!(
                    "cannot index {} with {}",
                    obj.type_of(),
                    idx.type_of()
                )));
            };
            realm.stack[a] = v;
            Ok(0)
        }
        Op::SetIndex => {
            let idx = rb(realm);
            let v = rc(realm);
            let target = realm.stack[a];
            if idx.is_number() {
                match target.as_array() {
                    Some(r) => {
                        realm.heap.allocs_since_gc += 1;
                        realm.heap.barrier_arr(r);
                        let arr = realm.heap.arr_mut(r);
                        let Some(i) = tsr_memory::array_index(idx.as_number()) else {
                            // non-index number key: array expando
                            // properties unsupported — ignored
                            return Ok(0);
                        };
                        if i < arr.len() {
                            arr[i] = v;
                        } else if i == arr.len() {
                            arr.push(v);
                        } else {
                            return Err(e("sparse arrays not supported in M1".into()));
                        }
                        Ok(0)
                    }
                    None => Err(e(format!(
                        "cannot index-assign {} with number",
                        target.type_of()
                    ))),
                }
            } else if let (Some(r), Some(s)) = (target.as_object(), idx.as_str_ref()) {
                let name = realm.heap.str_at(s).clone();
                realm.heap.barrier_obj(r);
                realm.heap.obj_mut(r).set(name, v);
                Ok(0)
            } else {
                Err(e(format!(
                    "cannot index-assign {} with {}",
                    target.type_of(),
                    idx.type_of()
                )))
            }
        }
        Op::Len => {
            let b = rb(realm);
            let v = if let Some(r) = b.as_array() {
                Value::number(realm.heap.arr(r).len() as f64)
            } else if let Some(r) = b.as_str_ref() {
                Value::number(realm.heap.str_at(r).chars().count() as f64)
            } else {
                return Err(e(format!("{} has no length", b.type_of())));
            };
            realm.stack[a] = v;
            Ok(0)
        }
        Op::ArrayPush => {
            let v = rb(realm);
            match realm.stack[a].as_array() {
                Some(r) => {
                    realm.heap.allocs_since_gc += 1;
                    realm.heap.barrier_arr(r);
                    realm.heap.arr_mut(r).push(v);
                    Ok(0)
                }
                None => Err(e(format!("cannot push onto {}", realm.stack[a].type_of()))),
            }
        }
        Op::GetGlobal => {
            let name = match &proto.consts[ins.bx() as usize] {
                Const::Str(s) => s.clone(),
                Const::Number(n) => Arc::from(tsr_memory::fmt_number(*n)),
            };
            let v = *realm
                .globals
                .get(&*name)
                .ok_or_else(|| e(format!("{name} is not defined")))?;
            realm.stack[a] = v;
            Ok(0)
        }
        Op::Concat => {
            let b = rb(realm);
            let c = rc(realm);
            let s = format!("{}{}", b.display(&realm.heap), c.display(&realm.heap));
            let v = realm.alloc_string(&s);
            realm.stack[a] = v;
            Ok(0)
        }
        Op::TypeOf => {
            let b = rb(realm);
            let v = realm.alloc_string(b.type_of());
            realm.stack[a] = v;
            Ok(0)
        }
        // control ops with inline templates never reach step
        Op::LoadInt | Op::LoadBool | Op::LoadNull | Op::LoadUndef | Op::Move | Op::Jump
        | Op::Call | Op::Return | Op::Halt | Op::Await => {
            Err(e(format!("internal: h_step on {:?}", ins.op)))
        }
    }
}

fn name_const<'a>(pr: &'a FunctionProto, idx: usize) -> &'a str {
    match &pr.consts[idx] {
        Const::Str(s) => s,
        Const::Number(_) => "",
    }
}

extern "C" fn h_get_field(
    p: *mut core::ffi::c_void,
    pp: *const FunctionProto,
    pc: u64,
    obj_bits: u64,
    cidx: u64,
) -> JitRet {
    let r = realm(p);
    let pr = proto(pp);
    let obj = Value::from_bits(obj_bits);
    // IC fast path (same per-pc caches the interpreter fills)
    if let Some(o) = obj.as_object() {
        let objref = &r.heap.objs[o as usize];
        let sid = objref.shape.id;
        let ic = pr.jit.ic_load(pr.code.len(), pc as usize);
        let v = if ic != 0 && (ic >> 32) as u32 == sid {
            objref.values[(ic & 0xFFFF_FFFF) as usize - 1]
        } else {
            let name = name_const(pr, cidx as usize);
            match objref.shape.slot_of(name) {
                Some(i) => {
                    pr.jit.ic_store(pr.code.len(), pc as usize, sid, i);
                    objref.values[i]
                }
                None => Value::UNDEFINED,
            }
        };
        return JitRet { val: v.bits(), stack: r.stack.as_mut_ptr() as u64 };
    }
    let name = name_const(pr, cidx as usize).to_string();
    match get_field(r, obj, &name) {
        Ok(v) => JitRet { val: v.bits(), stack: r.stack.as_mut_ptr() as u64 },
        Err(mut e) => {
            e.span = pr.spans.get(pc as usize).copied();
            fail(r, e)
        }
    }
}

extern "C" fn h_set_field(
    p: *mut core::ffi::c_void,
    pp: *const FunctionProto,
    pc: u64,
    obj_bits: u64,
    cidx: u64,
    v_bits: u64,
) -> JitRet {
    let r = realm(p);
    let pr = proto(pp);
    let obj = Value::from_bits(obj_bits);
    let v = Value::from_bits(v_bits);
    match obj.as_object() {
        Some(o) => {
            r.heap.barrier_obj(o);
            let obj = &mut r.heap.objs[o as usize];
            let sid = obj.shape.id;
            let ic = pr.jit.ic_load(pr.code.len(), pc as usize);
            if ic != 0 && (ic >> 32) as u32 == sid {
                obj.values[(ic & 0xFFFF_FFFF) as usize - 1] = v;
            } else {
                let name = name_const(pr, cidx as usize);
                match obj.shape.slot_of(name) {
                    Some(i) => {
                        pr.jit.ic_store(pr.code.len(), pc as usize, sid, i);
                        obj.values[i] = v;
                    }
                    None => {
                        let arc = match &pr.consts[cidx as usize] {
                            Const::Str(s) => s.clone(),
                            Const::Number(n) => {
                                std::sync::Arc::from(tsr_memory::fmt_number(*n))
                            }
                        };
                        obj.shape = obj.shape.with_field(arc);
                        obj.values.push(v);
                    }
                }
            }
            ok(r, 0)
        }
        None => fail(
            r,
            err_at(pr, pc as usize, format!("cannot set property on {}", obj.type_of())),
        ),
    }
}

extern "C" fn h_get_index(
    p: *mut core::ffi::c_void,
    pp: *const FunctionProto,
    pc: u64,
    obj_bits: u64,
    idx_bits: u64,
) -> JitRet {
    let r = realm(p);
    let pr = proto(pp);
    let obj = Value::from_bits(obj_bits);
    let idx = Value::from_bits(idx_bits);
    if idx.is_number() {
        if let Some(ar) = obj.as_array() {
            let v = tsr_memory::array_index(idx.as_number())
                .and_then(|i| r.heap.arr(ar).get(i).copied())
                .unwrap_or(Value::UNDEFINED);
            return JitRet { val: v.bits(), stack: r.stack.as_mut_ptr() as u64 };
        }
        return fail(
            r,
            err_at(pr, pc as usize, format!("cannot index {} with number", obj.type_of())),
        );
    }
    if let (Some(_), Some(sref)) = (obj.as_object(), idx.as_str_ref()) {
        let name = r.heap.str_at(sref).clone();
        return match get_field(r, obj, &name) {
            Ok(v) => JitRet { val: v.bits(), stack: r.stack.as_mut_ptr() as u64 },
            Err(mut e) => {
                e.span = pr.spans.get(pc as usize).copied();
                fail(r, e)
            }
        };
    }
    fail(
        r,
        err_at(
            pr,
            pc as usize,
            format!("cannot index {} with {}", obj.type_of(), idx.type_of()),
        ),
    )
}

extern "C" fn h_set_index(
    p: *mut core::ffi::c_void,
    pp: *const FunctionProto,
    pc: u64,
    target_bits: u64,
    idx_bits: u64,
    v_bits: u64,
) -> JitRet {
    let r = realm(p);
    let pr = proto(pp);
    let target = Value::from_bits(target_bits);
    let idx = Value::from_bits(idx_bits);
    let v = Value::from_bits(v_bits);
    if idx.is_number() {
        if let Some(ar) = target.as_array() {
            r.heap.allocs_since_gc += 1;
            r.heap.barrier_arr(ar);
            let arr = r.heap.arr_mut(ar);
            let Some(i) = tsr_memory::array_index(idx.as_number()) else {
                // non-index number key: array expando properties
                // unsupported — silently ignored
                return ok(r, 0);
            };
            if i < arr.len() {
                arr[i] = v;
            } else if i == arr.len() {
                arr.push(v);
            } else {
                return fail(
                    r,
                    err_at(pr, pc as usize, "sparse arrays not supported in M1".into()),
                );
            }
            return ok(r, 0);
        }
        return fail(
            r,
            err_at(
                pr,
                pc as usize,
                format!("cannot index-assign {} with number", target.type_of()),
            ),
        );
    }
    if let (Some(o), Some(sref)) = (target.as_object(), idx.as_str_ref()) {
        let name = r.heap.str_at(sref).clone();
        r.heap.barrier_obj(o);
        r.heap.obj_mut(o).set(name, v);
        return ok(r, 0);
    }
    fail(
        r,
        err_at(
            pr,
            pc as usize,
            format!(
                "cannot index-assign {} with {}",
                target.type_of(),
                idx.type_of()
            ),
        ),
    )
}

extern "C" fn h_len(
    p: *mut core::ffi::c_void,
    pp: *const FunctionProto,
    pc: u64,
    v_bits: u64,
) -> JitRet {
    let r = realm(p);
    let pr = proto(pp);
    let v = Value::from_bits(v_bits);
    if let Some(ar) = v.as_array() {
        let n = r.heap.arr(ar).len() as f64;
        return JitRet {
            val: Value::number(n).bits(),
            stack: r.stack.as_mut_ptr() as u64,
        };
    }
    if let Some(sr) = v.as_str_ref() {
        let n = r.heap.str_at(sr).chars().count() as f64;
        return JitRet {
            val: Value::number(n).bits(),
            stack: r.stack.as_mut_ptr() as u64,
        };
    }
    fail(
        r,
        err_at(pr, pc as usize, format!("{} has no length", v.type_of())),
    )
}

extern "C" fn h_push(
    p: *mut core::ffi::c_void,
    pp: *const FunctionProto,
    pc: u64,
    arr_bits: u64,
    v_bits: u64,
) -> JitRet {
    let r = realm(p);
    let pr = proto(pp);
    let arrv = Value::from_bits(arr_bits);
    match arrv.as_array() {
        Some(ar) => {
            r.heap.allocs_since_gc += 1;
            r.heap.barrier_arr(ar);
            r.heap.arr_mut(ar).push(Value::from_bits(v_bits));
            ok(r, 0)
        }
        None => fail(
            r,
            err_at(pr, pc as usize, format!("cannot push onto {}", arrv.type_of())),
        ),
    }
}

extern "C" fn h_get_global(
    p: *mut core::ffi::c_void,
    pp: *const FunctionProto,
    pc: u64,
    cidx: u64,
) -> JitRet {
    let r = realm(p);
    let pr = proto(pp);
    let name = name_const(pr, cidx as usize);
    match r.globals.get(name) {
        Some(v) => JitRet { val: v.bits(), stack: r.stack.as_mut_ptr() as u64 },
        None => {
            let msg = format!("{name} is not defined");
            fail(r, err_at(pr, pc as usize, msg))
        }
    }
}

fn heap_offsets() -> Option<tsr_jit::tier1::HeapOffsets> {
    crate::layout::layout().map(|l| tsr_jit::tier1::HeapOffsets {
        realm_objs_ptr: l.realm_objs_ptr,
        realm_arrs_ptr: l.realm_arrs_ptr,
        obj_size: l.obj_size,
        obj_shape_arc: l.obj_shape_arc,
        shape_id_delta: l.shape_id_delta,
        obj_vals_ptr: l.obj_vals_ptr,
        obj_vals_len: l.obj_vals_len,
        arr_size: l.arr_size,
        vec_ptr: l.vec_ptr,
        vec_len: l.vec_len,
    })
}

fn helpers() -> Helpers {
    Helpers {
        stack_ptr: h_stack_ptr as *const () as usize,
        step: h_step as *const () as usize,
        call: h_call as *const () as usize,
        safepoint: h_safepoint as *const () as usize,
        get_field: h_get_field as *const () as usize,
        set_field: h_set_field as *const () as usize,
        get_index: h_get_index as *const () as usize,
        set_index: h_set_index as *const () as usize,
        len: h_len as *const () as usize,
        push: h_push as *const () as usize,
        get_global: h_get_global as *const () as usize,
    }
}

/// Call threshold before Tier-1 compilation (overridable for testing).
fn threshold() -> u32 {
    static T: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *T.get_or_init(|| {
        std::env::var("TSC_JIT_THRESHOLD")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(50)
    })
}

pub fn jit_enabled() -> bool {
    static E: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *E.get_or_init(|| std::env::var_os("TSC_NO_JIT").is_none())
}

/// Count a call; compile when hot. Returns the code pointer when available.
#[inline]
pub fn tier_up(proto: &Arc<FunctionProto>) -> Option<CompiledFn> {
    let jit = &proto.jit;
    let code = jit.code.load(Acquire);
    if !code.is_null() {
        return Some(unsafe { std::mem::transmute::<*mut u8, CompiledFn>(code) });
    }
    if !jit_enabled() || jit.tier.load(Relaxed) == TIER_REJECTED {
        return None;
    }
    let n = jit.counter.fetch_add(1, Relaxed);
    if n == threshold()
        && jit
            .tier
            .compare_exchange(TIER_COLD, TIER_COMPILING, AcqRel, Acquire)
            .is_ok()
    {
        compile_now(proto);
        let code = jit.code.load(Acquire);
        if !code.is_null() {
            return Some(unsafe { std::mem::transmute::<*mut u8, CompiledFn>(code) });
        }
    }
    None
}

extern "C" {
    fn fmod(x: f64, y: f64) -> f64;
    fn pow(x: f64, y: f64) -> f64;
}

fn tier2_enabled() -> bool {
    static E: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *E.get_or_init(|| std::env::var_os("TSC_NO_TIER2").is_none())
}

/// OSR trigger threshold (interpreter back-edges in one function).
fn osr_threshold() -> u32 {
    static T: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *T.get_or_init(|| {
        std::env::var("TSC_OSR_THRESHOLD")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(10_000)
    })
}

pub fn osr_threshold_pub() -> u32 {
    osr_threshold()
}

/// OSR slow path: reached only after the back-edge counter crossed the
/// threshold (interpreter keeps a one-load fast path). Compiles once,
/// returns the code, or marks the proto permanently un-OSR-able
/// (backedges = u32::MAX) so the fast path never returns here.
pub fn osr_slow(proto: &Arc<FunctionProto>) -> Option<CompiledFn> {
    let jit = &proto.jit;
    let code = jit.osr_code.load(Acquire);
    if !code.is_null() {
        return Some(unsafe { std::mem::transmute::<*mut u8, CompiledFn>(code) });
    }
    if !jit_enabled() {
        jit.backedges.store(u32::MAX, Relaxed);
        return None;
    }
    let ics = proto.jit.ics_base(proto.code.len()) as u64;
    match tsr_jit::tier1::compile(proto, helpers(), true, heap_offsets(), ics) {
        Some(code) => {
            let ptr = tsr_jit::heap::publish(&code) as *mut u8;
            jit.osr_code.store(ptr, Release);
            Some(unsafe { std::mem::transmute::<*mut u8, CompiledFn>(ptr) })
        }
        None => {
            jit.backedges.store(u32::MAX, Relaxed);
            None
        }
    }
}

/// Enter Tier-1 code mid-function at a loop header (state = the slots).
pub fn enter_osr(
    realm: &mut Realm,
    f: CompiledFn,
    proto: &Arc<FunctionProto>,
    base: usize,
    closure: Option<u32>,
    depth: u32,
    target_pc: usize,
) -> Result<Value, RtError> {
    let ret = f(
        realm as *mut Realm as *mut core::ffi::c_void,
        proto.as_ref() as *const FunctionProto,
        (base * 8) as u64,
        closure.unwrap_or(u32::MAX),
        depth,
        target_pc as u64,
    );
    match ret.val {
        0 => Ok(Value::from_bits(ret.stack)),
        _ => Err(realm
            .jit_error
            .take()
            .unwrap_or_else(|| RtError::new("internal: jit error missing"))),
    }
}

pub fn compile_now(proto: &Arc<FunctionProto>) {
    if tier2_enabled() {
        let typed = tsc_types::analyze(proto);
        if typed.tier2_ok {
            let code = tsr_jit::tier2::compile(
                proto,
                helpers(),
                &typed.jumpif_num,
                fmod as *const () as usize,
                pow as *const () as usize,
            );
            let ptr = tsr_jit::heap::publish(&code) as *mut u8;
            proto.jit.code.store(ptr, Release);
            proto.jit.tier.store(TIER_OPT, Release);
            return;
        }
    }
    compile_tier1(proto)
}

fn compile_tier1(proto: &Arc<FunctionProto>) {
    let ics = proto.jit.ics_base(proto.code.len()) as u64;
    match tsr_jit::tier1::compile(proto, helpers(), false, heap_offsets(), ics) {
        Some(code) => {
            let ptr = tsr_jit::heap::publish(&code) as *mut u8;
            proto.jit.code.store(ptr, Release);
            proto.jit.tier.store(TIER_BASELINE, Release);
        }
        None => {
            proto.jit.tier.store(TIER_REJECTED, Release);
        }
    }
}

/// Run a compiled function whose argument window is prepared at `base`.
pub fn enter_jit(
    realm: &mut Realm,
    f: CompiledFn,
    proto: &Arc<FunctionProto>,
    base: usize,
    closure: Option<u32>,
    depth: u32,
) -> Result<Value, RtError> {
    let ret = f(
        realm as *mut Realm as *mut core::ffi::c_void,
        proto.as_ref() as *const FunctionProto,
        (base * 8) as u64,
        closure.unwrap_or(u32::MAX),
        depth,
        0,
    );
    match ret.val {
        0 => Ok(Value::from_bits(ret.stack)),
        2 => {
            // deopt: state fully materialized in slots; resume in the
            // interpreter. Repeated deopts demote the proto to Tier-1.
            let deopts = proto.jit.deopts.fetch_add(1, Relaxed) + 1;
            if deopts >= 10 && proto.jit.tier.load(Relaxed) == TIER_OPT {
                compile_tier1(proto);
            }
            let resume_pc = ret.stack as usize;
            match crate::interp::run_frame_pub(realm, closure, proto, base, depth, resume_pc)? {
                crate::interp::FrameResult::Return(v) => Ok(v),
                crate::interp::FrameResult::Await { .. } => {
                    unreachable!("await in sync frame")
                }
            }
        }
        _ => Err(realm
            .jit_error
            .take()
            .unwrap_or_else(|| RtError::new("internal: jit error missing"))),
    }
}
