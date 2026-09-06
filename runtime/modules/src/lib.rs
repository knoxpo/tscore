//! tsr-modules
//!
//! Instantiate and evaluate a compiled module graph. Namespace objects
//! live in `realm.globals` under reserved "\0mod:<id>" keys (NUL prefix —
//! unspellable from TS), so existing GetGlobal/GetField/LoadCell machinery
//! and GC tracing cover everything.

use std::collections::HashMap;
use std::sync::Arc;
use tsc_ir::{Program, ReExport};
use tsr_memory::Value;
use tsr_realm::{interpreter, Realm, RtError};
use tsr_memory::{self};

fn module_key(id: &str) -> Arc<str> {
    Arc::from(format!("\0mod:{id}"))
}

#[derive(Clone, Copy, PartialEq)]
enum State {
    Instantiated,
    Evaluating,
    Evaluated,
}

/// Run a module graph: instantiate every namespace first (cycle safety),
/// then evaluate post-order from the entry. Returns the entry module's
/// completion value.
pub fn run_program(realm: &mut Realm, program: &Program) -> Result<Value, RtError> {
    run_graph(realm, program)
}

/// Like `run_program`, but modules already present in the realm (their
/// "\0mod:" key exists) are left untouched — dynamic import() grafts new
/// subgraphs onto a live registry.
pub fn run_graph(realm: &mut Realm, program: &Program) -> Result<Value, RtError> {
    // ---- instantiate: ns objects, undefined values, shared cells ----
    let mut preexisting = vec![false; program.modules.len()];
    for (i, m) in program.modules.iter().enumerate() {
        if realm.globals.contains_key(&module_key(&m.id)) {
            preexisting[i] = true;
            continue;
        }
        let obj = realm.heap.alloc_obj_host();
        // three hidden pads push every export into overflow slots: the
        // JIT's inline GetField serves slots < OBJ_INLINE only, so ns
        // reads always take the helper — which derefs export cells
        for pad in ["\0p0", "\0p1", "\0p2"] {
            realm.heap.obj_set(obj, Arc::from(pad), Value::UNDEFINED);
        }
        // export fields start as the TDZ sentinel; each module publishes
        // values (or its live cells) as it evaluates — a cyclic read
        // before initialization errors cleanly instead of yielding
        // undefined
        let tdz = Value::from_bits(tsr_memory::TDZ_SENTINEL);
        for e in &m.exports {
            if e.from.is_some() {
                continue; // re-exports materialize just before evaluation
            }
            realm.heap.obj_set(obj, e.name.clone(), tdz);
        }
        if !m.exports.iter().any(|e| e.name.as_ref() == "default") {
            realm.heap.obj_set(obj, Arc::from("default"), tdz);
        }
        let r = obj; // old space: ns never moves
        realm.globals.insert(module_key(&m.id), Value::object(r));
    }

    // ---- evaluate: post-order DFS with cycle marks ----
    let index: HashMap<Arc<str>, usize> = program
        .modules
        .iter()
        .enumerate()
        .map(|(i, m)| (m.id.clone(), i))
        .collect();
    let mut state: Vec<State> = preexisting
        .iter()
        .map(|&p| if p { State::Evaluated } else { State::Instantiated })
        .collect();
    let mut result = Value::UNDEFINED;
    eval_module(realm, program, &index, &mut state, program.entry, &mut result)?;
    Ok(result)
}

fn eval_module(
    realm: &mut Realm,
    program: &Program,
    index: &HashMap<Arc<str>, usize>,
    state: &mut Vec<State>,
    i: usize,
    entry_result: &mut Value,
) -> Result<(), RtError> {
    match state[i] {
        State::Evaluated => return Ok(()),
        State::Evaluating => return Ok(()), // cycle back-edge: partial ns
        State::Instantiated => {}
    }
    state[i] = State::Evaluating;
    let deps: Vec<usize> = program.modules[i]
        .deps
        .iter()
        .filter_map(|d| index.get(d).copied())
        .collect();
    for d in deps {
        eval_module(realm, program, index, state, d, entry_result)?;
    }
    materialize_reexports(realm, program, i);
    // run the module body to completion (drives top-level await)
    let v = interpreter::run_main(realm, &program.modules[i].main)?;
    if i == program.entry {
        *entry_result = v;
    }
    state[i] = State::Evaluated;
    // This module's exports are real now. Any module that re-exports one
    // of them copied the TDZ sentinel instead if it materialized while we
    // were mid-cycle, so repair those namespaces. Idempotent and TDZ-only,
    // so non-cyclic graphs pay one field lookup per re-export.
    // ponytail: O(modules) scan per module finish. Build a
    // dep -> dependents index in compile_graph if a graph ever gets big
    // enough for the O(n^2) to show up.
    let id = program.modules[i].id.clone();
    let dependents: Vec<usize> = (0..program.modules.len())
        .filter(|&j| {
            j != i
                && program.modules[j]
                    .exports
                    .iter()
                    .any(|e| e.from.as_ref().is_some_and(|(d, _)| *d == id))
        })
        .collect();
    for j in dependents {
        materialize_reexports(realm, program, j);
    }
    Ok(())
}

/// Copy re-exported fields from each dep's namespace into this module's.
/// Idempotent, and skips fields already holding a real value — so the
/// second (post-evaluation) pass only repairs TDZ left by a cycle.
fn materialize_reexports(realm: &mut Realm, program: &Program, i: usize) {
    let tdz = Value::from_bits(tsr_memory::TDZ_SENTINEL);
    let m = &program.modules[i];
    let ns_v = realm.globals[&module_key(&m.id)];
    for e in &m.exports {
        let Some((dep, re)) = &e.from else { continue };
        let dep_v = realm.globals[&module_key(dep)];
        let (Some(ns), Some(dep_ns)) = (ns_v.as_object(), dep_v.as_object()) else {
            continue;
        };
        match re {
            ReExport::Namespace => {
                realm.heap.barrier_obj(ns);
                realm.heap.obj_set(ns, e.name.clone(), dep_v);
            }
            ReExport::Named(orig) => {
                let cur = realm.heap.obj(ns).get(&e.name);
                if cur.is_some_and(|c| c.bits() != tdz.bits()) {
                    continue; // already materialized with a real value
                }
                let v = realm
                    .heap
                    .obj(dep_ns)
                    .get(orig)
                    .unwrap_or(Value::UNDEFINED);
                realm.heap.barrier_obj(ns);
                realm.heap.obj_set(ns, e.name.clone(), v);
            }
            ReExport::Star => {
                let fields: Vec<(Arc<str>, Value)> = realm
                    .heap
                    .obj(dep_ns)
                    .entries()
                    .filter(|(k, _)| k.as_ref() != "default")
                    .map(|(k, v)| (k.clone(), v))
                    .collect();
                realm.heap.barrier_obj(ns);
                for (k, v) in fields {
                    let cur = realm.heap.obj(ns).get(&k);
                    if cur.is_none_or(|c| c.bits() == tdz.bits()) {
                        realm.heap.obj_set(ns, k, v);
                    }
                }
            }
        }
    }
}

/// Install the dynamic-import host: `__import(spec, importerId)` resolves
/// against the live registry, compiling and grafting new subgraphs on
/// demand. Returns an already-settled promise either way.
pub fn install(realm: &mut Realm) {
    let imp = realm.add_native(|realm, args| {
        let spec_v = args.get(realm, 0);
        let Some(spec_r) = spec_v.as_str_ref() else {
            return Err(RtError::new("import(): specifier must be a string"));
        };
        let spec = realm.heap.str_at(spec_r).to_string();
        let importer_v = args.get(realm, 1);
        let importer = importer_v
            .as_str_ref()
            .map(|r| realm.heap.str_at(r).to_string())
            .unwrap_or_default();
        let outcome = dynamic_import(realm, &spec, &importer);
        let p = match outcome {
            Ok(ns) => realm.heap.alloc_foreign(tsr_memory::Foreign::Promise(
                tsr_memory::Promise {
                    state: tsr_memory::PromiseState::Fulfilled(ns),
                    reactions: Vec::new(),
                },
            )),
            Err(e) => realm.heap.alloc_foreign(tsr_memory::Foreign::Promise(
                tsr_memory::Promise {
                    state: tsr_memory::PromiseState::Rejected(
                        tsr_memory::PromiseError {
                            msg: e.msg,
                            cancelled: false,
                            span: e.span,
                            source: e.source,
                        },
                    ),
                    reactions: Vec::new(),
                },
            )),
        };
        Ok(Value::foreign(p))
    });
    realm.set_global("__import", imp);
}

fn dynamic_import(realm: &mut Realm, spec: &str, importer: &str) -> Result<Value, RtError> {
    let importer_dir = std::path::Path::new(importer)
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .to_path_buf();
    let cfg = tsc_modules::ResolveConfig::discover(std::path::Path::new(importer));
    let path = tsc_modules::resolve_specifier(spec, &importer_dir, &cfg)
        .map_err(RtError::new)?;
    let id = path.display().to_string();
    let key = module_key(&id);
    if let Some(&ns) = realm.globals.get(&key) {
        return Ok(ns); // cache hit: same namespace object identity
    }
    let program = tsc_modules::compile_graph(&path).map_err(|e| {
        RtError::new(format!("{}: {}", e.path, e.msg))
    })?;
    run_graph(realm, &program)?;
    realm
        .globals
        .get(&key)
        .copied()
        .ok_or_else(|| RtError::new("internal: imported module missing after evaluation"))
}
