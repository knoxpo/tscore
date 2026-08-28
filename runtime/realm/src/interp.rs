//! Register-machine bytecode interpreter.

use crate::{Realm, RtError};
use std::sync::Arc;
use tsc_ir::{Const, FunctionProto, Op, UpvalSrc};
use tsr_memory::{to_int32, to_uint32, Closure, Value};

/// Call any callable value with the given arguments (entry point for the
/// CLI and for natives like `parallel.map` that re-enter the interpreter).
pub fn call_value(realm: &mut Realm, f: Value, args: &[Value]) -> Result<Value, RtError> {
    let base = realm.stack.len();
    match f {
        Value::Native(i) => {
            let native = realm.natives[i as usize].clone();
            native(realm, args)
        }
        Value::Closure(c) => {
            let proto = realm.heap.closure(c).proto.clone();
            realm.stack.resize(base + proto.n_regs as usize, Value::Undefined);
            let n = (proto.arity as usize).min(args.len());
            realm.stack[base..base + n].copy_from_slice(&args[..n]);
            let result = run_frame(realm, Some(c), &proto, base);
            realm.stack.truncate(base);
            result
        }
        _ => Err(RtError::new(format!("{} is not a function", f.type_of()))),
    }
}

/// Run the top-level chunk in the realm.
pub fn run_main(realm: &mut Realm, main: &Arc<FunctionProto>) -> Result<Value, RtError> {
    let base = realm.stack.len();
    realm.stack.resize(base + main.n_regs as usize, Value::Undefined);
    let result = run_frame(realm, None, main, base);
    realm.stack.truncate(base);
    result
}

macro_rules! num_bin {
    ($realm:ident, $proto:ident, $pc:ident, $ins:ident, $base:ident, $f:expr) => {{
        let b = $realm.stack[$base + $ins.b as usize];
        let c = $realm.stack[$base + $ins.c as usize];
        match (b, c) {
            (Value::Number(x), Value::Number(y)) => {
                $realm.stack[$base + $ins.a as usize] = Value::Number($f(x, y));
            }
            _ => return Err(err($proto, $pc, format!(
                "cannot apply arithmetic to {} and {}", b.type_of(), c.type_of()))),
        }
    }};
}

macro_rules! int_bin {
    ($realm:ident, $ins:ident, $base:ident, $f:expr) => {{
        let x = to_num($realm.stack[$base + $ins.b as usize]);
        let y = to_num($realm.stack[$base + $ins.c as usize]);
        $realm.stack[$base + $ins.a as usize] = Value::Number($f(x, y));
    }};
}

macro_rules! cmp_bin {
    ($realm:ident, $proto:ident, $pc:ident, $ins:ident, $base:ident, $nf:expr, $sf:expr) => {{
        let b = $realm.stack[$base + $ins.b as usize];
        let c = $realm.stack[$base + $ins.c as usize];
        let r = match (b, c) {
            (Value::Number(x), Value::Number(y)) => $nf(&x, &y),
            (Value::Str(x), Value::Str(y)) => {
                $sf(&*$realm.heap.str_at(x).clone(), &*$realm.heap.str_at(y).clone())
            }
            _ => return Err(err($proto, $pc, format!(
                "cannot compare {} and {}", b.type_of(), c.type_of()))),
        };
        $realm.stack[$base + $ins.a as usize] = Value::Bool(r);
    }};
}

fn to_num(v: Value) -> f64 {
    match v {
        Value::Number(n) => n,
        _ => f64::NAN,
    }
}

fn err(proto: &FunctionProto, pc: usize, msg: String) -> RtError {
    RtError { msg, span: proto.spans.get(pc).copied() }
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

fn run_frame(
    realm: &mut Realm,
    closure: Option<tsr_memory::Ref>,
    proto: &Arc<FunctionProto>,
    base: usize,
) -> Result<Value, RtError> {
    let mut pc: usize = 0;
    loop {
        let ins = proto.code[pc];
        let a = base + ins.a as usize;
        match ins.op {
            Op::LoadConst => {
                realm.stack[a] = match &proto.consts[ins.bx() as usize] {
                    Const::Number(n) => Value::Number(*n),
                    Const::Str(s) => {
                        let r = realm.heap.alloc_str(s.clone());
                        Value::Str(r)
                    }
                }
            }
            Op::LoadInt => realm.stack[a] = Value::Number(ins.sbx() as f64),
            Op::LoadBool => realm.stack[a] = Value::Bool(ins.b != 0),
            Op::LoadNull => realm.stack[a] = Value::Null,
            Op::LoadUndef => realm.stack[a] = Value::Undefined,
            Op::Move => realm.stack[a] = realm.stack[base + ins.b as usize],

            Op::Add => {
                let b = realm.stack[base + ins.b as usize];
                let c = realm.stack[base + ins.c as usize];
                realm.stack[a] = match (b, c) {
                    (Value::Number(x), Value::Number(y)) => Value::Number(x + y),
                    (Value::Str(_), _) | (_, Value::Str(_)) => {
                        let s = format!("{}{}", b.display(&realm.heap), c.display(&realm.heap));
                        realm.alloc_string(&s)
                    }
                    _ => {
                        return Err(err(proto, pc, format!(
                            "cannot add {} and {}", b.type_of(), c.type_of())))
                    }
                };
            }
            Op::Sub => num_bin!(realm, proto, pc, ins, base, |x: f64, y: f64| x - y),
            Op::Mul => num_bin!(realm, proto, pc, ins, base, |x: f64, y: f64| x * y),
            Op::Div => num_bin!(realm, proto, pc, ins, base, |x: f64, y: f64| x / y),
            Op::Mod => num_bin!(realm, proto, pc, ins, base, |x: f64, y: f64| x % y),
            Op::Pow => num_bin!(realm, proto, pc, ins, base, |x: f64, y: f64| x.powf(y)),
            Op::Neg => {
                let b = realm.stack[base + ins.b as usize];
                match b {
                    Value::Number(n) => realm.stack[a] = Value::Number(-n),
                    _ => return Err(err(proto, pc, format!("cannot negate {}", b.type_of()))),
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
                let x = to_num(realm.stack[base + ins.b as usize]);
                realm.stack[a] = Value::Number(!to_int32(x) as f64);
            }

            Op::Eq => {
                let b = realm.stack[base + ins.b as usize];
                let c = realm.stack[base + ins.c as usize];
                realm.stack[a] = Value::Bool(b.strict_eq(c, &realm.heap));
            }
            Op::Ne => {
                let b = realm.stack[base + ins.b as usize];
                let c = realm.stack[base + ins.c as usize];
                realm.stack[a] = Value::Bool(!b.strict_eq(c, &realm.heap));
            }
            Op::Lt => cmp_bin!(realm, proto, pc, ins, base, |x, y| f64::lt(x, y), str::lt),
            Op::Le => cmp_bin!(realm, proto, pc, ins, base, |x, y| f64::le(x, y), str::le),
            Op::Gt => cmp_bin!(realm, proto, pc, ins, base, |x, y| f64::gt(x, y), str::gt),
            Op::Ge => cmp_bin!(realm, proto, pc, ins, base, |x, y| f64::ge(x, y), str::ge),
            Op::Not => {
                let b = realm.stack[base + ins.b as usize];
                realm.stack[a] = Value::Bool(!b.truthy(&realm.heap));
            }

            Op::Jump => {
                if ins.sbx() < 0 {
                    // loop back-edge: GC safepoint (roots = stack + globals)
                    realm.maybe_gc();
                }
                pc = (pc as i64 + ins.sbx() as i64) as usize;
            }
            Op::JumpIfFalse => {
                if !realm.stack[a].truthy(&realm.heap) {
                    pc = (pc as i64 + ins.sbx() as i64) as usize;
                }
            }
            Op::JumpIfTrue => {
                if realm.stack[a].truthy(&realm.heap) {
                    pc = (pc as i64 + ins.sbx() as i64) as usize;
                }
            }

            Op::Call => {
                let f = realm.stack[a];
                let argc = ins.b as usize;
                match f {
                    Value::Closure(c) => {
                        realm.maybe_gc(); // call safepoint
                        let callee = realm.heap.closure(c).proto.clone();
                        let new_base = a + 1;
                        let need = new_base + callee.n_regs as usize;
                        if realm.stack.len() < need {
                            realm.stack.resize(need, Value::Undefined);
                        }
                        for r in argc..callee.n_regs as usize {
                            realm.stack[new_base + r] = Value::Undefined;
                        }
                        let result = run_frame(realm, Some(c), &callee, new_base)?;
                        realm.stack[a] = result;
                    }
                    Value::Native(i) => {
                        let native = realm.natives[i as usize].clone();
                        // args on the Rust stack: no per-call Vec alloc
                        let mut buf = [Value::Undefined; 8];
                        let result = if argc <= 8 {
                            buf[..argc]
                                .copy_from_slice(&realm.stack[a + 1..a + 1 + argc]);
                            native(realm, &buf[..argc])
                        } else {
                            let args: Vec<Value> =
                                realm.stack[a + 1..a + 1 + argc].to_vec();
                            native(realm, &args)
                        }
                        .map_err(|e| err(proto, pc, e.msg))?;
                        realm.stack[a] = result;
                    }
                    _ => {
                        return Err(err(proto, pc, format!(
                            "{} is not a function", f.type_of())))
                    }
                }
            }
            Op::Return => return Ok(realm.stack[a]),
            Op::Halt => return Ok(Value::Undefined),

            Op::Closure => {
                let child = proto.protos[ins.bx() as usize].clone();
                let mut upvals = Vec::with_capacity(child.upvals.len());
                for u in &child.upvals {
                    let cell = match *u {
                        UpvalSrc::ParentLocal(reg) => {
                            match realm.stack[base + reg as usize] {
                                Value::Cell(r) => r,
                                v => {
                                    return Err(err(proto, pc, format!(
                                        "internal: captured slot holds {}", v.type_of())))
                                }
                            }
                        }
                        UpvalSrc::ParentUpval(idx) => {
                            let c = closure.expect("upval capture outside closure");
                            realm.heap.closure(c).upvals[idx as usize]
                        }
                    };
                    upvals.push(cell);
                }
                let r = realm.heap.alloc_closure(Closure { proto: child, upvals });
                realm.stack[a] = Value::Closure(r);
            }
            Op::NewCell => {
                let init = realm.stack[a];
                let r = realm.heap.alloc_cell(init);
                realm.stack[a] = Value::Cell(r);
            }
            Op::LoadCell => match realm.stack[base + ins.b as usize] {
                Value::Cell(r) => realm.stack[a] = *realm.heap.cell(r),
                v => return Err(err(proto, pc, format!(
                    "internal: LoadCell on {}", v.type_of()))),
            },
            Op::StoreCell => match realm.stack[a] {
                Value::Cell(r) => {
                    *realm.heap.cell_mut(r) = realm.stack[base + ins.b as usize]
                }
                v => return Err(err(proto, pc, format!(
                    "internal: StoreCell on {}", v.type_of()))),
            },
            Op::GetUpval => {
                let c = closure.expect("GetUpval outside closure");
                let cell = realm.heap.closure(c).upvals[ins.b as usize];
                realm.stack[a] = *realm.heap.cell(cell);
            }
            Op::SetUpval => {
                let c = closure.expect("SetUpval outside closure");
                let cell = realm.heap.closure(c).upvals[ins.a as usize];
                *realm.heap.cell_mut(cell) = realm.stack[base + ins.b as usize];
            }

            Op::NewObject => {
                let r = realm.heap.alloc_obj(Default::default());
                realm.stack[a] = Value::Object(r);
            }
            Op::NewArray => {
                let r = realm.heap.alloc_arr(Vec::with_capacity(ins.b as usize));
                realm.stack[a] = Value::Array(r);
            }
            Op::GetField => {
                let obj = realm.stack[base + ins.b as usize];
                let name = const_str(proto, ins.c as usize);
                realm.stack[a] = get_field(realm, obj, name)
                    .map_err(|e| err(proto, pc, e.msg))?;
            }
            Op::SetField => {
                let name = const_str_arc(proto, ins.b as usize);
                let v = realm.stack[base + ins.c as usize];
                match realm.stack[a] {
                    Value::Object(r) => realm.heap.obj_mut(r).set(name, v),
                    t => return Err(err(proto, pc, format!(
                        "cannot set property on {}", t.type_of()))),
                }
            }
            Op::GetIndex => {
                let obj = realm.stack[base + ins.b as usize];
                let idx = realm.stack[base + ins.c as usize];
                realm.stack[a] = match (obj, idx) {
                    (Value::Array(r), Value::Number(n)) => {
                        realm.heap.arr(r).get(n as usize).copied()
                            .unwrap_or(Value::Undefined)
                    }
                    (Value::Object(_), Value::Str(s)) => {
                        let name = realm.heap.str_at(s).clone();
                        get_field(realm, obj, &name)
                            .map_err(|e| err(proto, pc, e.msg))?
                    }
                    _ => return Err(err(proto, pc, format!(
                        "cannot index {} with {}", obj.type_of(), idx.type_of()))),
                };
            }
            Op::SetIndex => {
                let idx = realm.stack[base + ins.b as usize];
                let v = realm.stack[base + ins.c as usize];
                match (realm.stack[a], idx) {
                    (Value::Array(r), Value::Number(n)) => {
                        let arr = realm.heap.arr_mut(r);
                        let i = n as usize;
                        if i < arr.len() {
                            arr[i] = v;
                        } else if i == arr.len() {
                            arr.push(v);
                        } else {
                            return Err(err(proto, pc,
                                "sparse arrays not supported in M1".into()));
                        }
                    }
                    (Value::Object(r), Value::Str(s)) => {
                        let name = realm.heap.str_at(s).clone();
                        realm.heap.obj_mut(r).set(name, v);
                    }
                    (t, i) => return Err(err(proto, pc, format!(
                        "cannot index-assign {} with {}", t.type_of(), i.type_of()))),
                }
            }
            Op::Len => {
                let b = realm.stack[base + ins.b as usize];
                realm.stack[a] = match b {
                    Value::Array(r) => Value::Number(realm.heap.arr(r).len() as f64),
                    Value::Str(r) => {
                        // ponytail: byte length == char length for ASCII benchmarks;
                        // UTF-16 length when strings grow up
                        Value::Number(realm.heap.str_at(r).chars().count() as f64)
                    }
                    _ => return Err(err(proto, pc, format!(
                        "{} has no length", b.type_of()))),
                };
            }
            Op::ArrayPush => {
                let v = realm.stack[base + ins.b as usize];
                match realm.stack[a] {
                    Value::Array(r) => realm.heap.arr_mut(r).push(v),
                    t => return Err(err(proto, pc, format!(
                        "cannot push onto {}", t.type_of()))),
                }
            }

            Op::GetGlobal => {
                let name = const_str(proto, ins.bx() as usize);
                realm.stack[a] = *realm.globals.get(name).ok_or_else(|| {
                    err(proto, pc, format!("{name} is not defined"))
                })?;
            }

            Op::Concat => {
                let b = realm.stack[base + ins.b as usize];
                let c = realm.stack[base + ins.c as usize];
                let s = format!("{}{}", b.display(&realm.heap), c.display(&realm.heap));
                realm.stack[a] = realm.alloc_string(&s);
            }
            Op::TypeOf => {
                let b = realm.stack[base + ins.b as usize];
                realm.stack[a] = realm.alloc_string(b.type_of());
            }
        }
        pc += 1;
    }
}

fn get_field(realm: &mut Realm, obj: Value, name: &str) -> Result<Value, RtError> {
    match obj {
        Value::Object(r) => Ok(realm.heap.obj(r).get(name).unwrap_or(Value::Undefined)),
        Value::Array(r) if name == "length" => {
            Ok(Value::Number(realm.heap.arr(r).len() as f64))
        }
        Value::Str(r) if name == "length" => {
            Ok(Value::Number(realm.heap.str_at(r).chars().count() as f64))
        }
        _ => Err(RtError::new(format!(
            "cannot read property '{name}' of {}", obj.type_of()))),
    }
}
