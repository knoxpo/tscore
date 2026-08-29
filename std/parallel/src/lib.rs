//! tss-parallel
//!
//! `parallel.*` natives on the work-stealing scheduler. Semantics: clone at
//! spawn (see docs/specifications/concurrency-semantics.md) — the callback's
//! captures and each input chunk are structured-cloned into a fresh scratch
//! realm per chunk; results are cloned back. Deterministic: identical output
//! at any worker count.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tsr_memory::Value;
use tsr_realm::interp::call_value;
use tsr_realm::{Realm, RtError};
use tsr_scheduler::Pool;
use tsr_task::portable::{clone_out, rehydrate};
use tsr_task::PortableValue;

static POOL: OnceLock<Pool> = OnceLock::new();
static CONFIGURED_WORKERS: OnceLock<usize> = OnceLock::new();

/// Record the desired worker count without spawning anything (startup cost:
/// zero). Returns the effective count. First call wins.
pub fn configure(workers: Option<usize>) -> usize {
    *CONFIGURED_WORKERS.get_or_init(|| {
        workers.unwrap_or_else(|| {
            std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
        })
    })
}

/// The process-wide scheduler pool. Built lazily on first parallel /
/// task.scope use — programs that never go parallel never spawn threads.
pub fn shared_pool() -> &'static Pool {
    POOL.get_or_init(|| Pool::new(configure(None)))
}

/// Install `parallel` and `runtime.cpu` globals. First call fixes the
/// worker count for the process.
pub fn install(realm: &mut Realm, workers: Option<usize>) {
    let n_workers = configure(workers);

    let map = realm.add_native(|realm, args| run_parallel(realm, args, true));
    let for_ = realm.add_native(|realm, args| run_parallel(realm, args, false));
    realm.set_global_obj("parallel", vec![("map", map), ("for", for_)]);

    // runtime.cpu.count
    let mut cpu = tsr_memory::Obj::default();
    cpu.set(Arc::from("count"), Value::Number(n_workers as f64));
    let cpu_ref = realm.heap.alloc_obj(cpu);
    realm.set_global_obj("runtime", vec![("cpu", Value::Object(cpu_ref))]);
}


fn run_parallel(
    realm: &mut Realm,
    args: &[Value],
    collect: bool,
) -> Result<Value, RtError> {
    let (arr, f) = match (args.first(), args.get(1)) {
        (Some(&Value::Array(a)), Some(&f @ Value::Closure(_))) => (a, f),
        (Some(&Value::Array(_)), Some(&Value::Native(_))) => {
            return Err(RtError::new(
                "parallel.map: callback must be a TypeScript function",
            ))
        }
        _ => {
            return Err(RtError::new(
                "parallel.map(items, fn): expected array and function",
            ))
        }
    };
    let pool = shared_pool();
    let n_items = realm.heap.arr(arr).len();
    if n_items == 0 {
        return Ok(if collect {
            Value::Array(realm.heap.alloc_arr(Vec::new()))
        } else {
            Value::Undefined
        });
    }

    // clone captures + input out of the caller realm
    let pv_fn = Arc::new(clone_out(&realm.heap, f).map_err(RtError::new)?);
    let items: Vec<PortableValue> = {
        let vals = realm.heap.arr(arr).clone();
        vals.into_iter()
            .map(|v| clone_out(&realm.heap, v))
            .collect::<Result<_, _>>()
            .map_err(RtError::new)?
    };

    // ponytail: static 8×workers chunks + work stealing; lazy binary
    // splitting if benchmarks show skew starving workers. No minimum chunk
    // size: per-chunk overhead (realm setup + clone) is ~µs, so
    // over-chunking small heavy inputs costs less than under-chunking them.
    let chunk_size = n_items.div_ceil(8 * pool.workers()).max(1);

    let results: Arc<Mutex<Vec<Option<PortableValue>>>> =
        Arc::new(Mutex::new(if collect { vec![None; n_items] } else { Vec::new() }));
    let error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let failed = Arc::new(AtomicBool::new(false));

    let mut jobs: Vec<Box<dyn FnOnce(&tsr_scheduler::Ctx) + Send>> = Vec::new();
    let mut items = items.into_iter().enumerate().peekable();
    while items.peek().is_some() {
        let chunk: Vec<(usize, PortableValue)> = items.by_ref().take(chunk_size).collect();
        let pv_fn = pv_fn.clone();
        let results = results.clone();
        let error = error.clone();
        let failed = failed.clone();
        jobs.push(Box::new(move |_ctx| {
            if failed.load(Ordering::Relaxed) {
                return;
            }
            match run_chunk(&pv_fn, &chunk, collect) {
                Ok(out) => {
                    if collect {
                        let mut slots = results.lock().unwrap();
                        for (i, v) in out {
                            slots[i] = Some(v);
                        }
                    }
                }
                Err(e) => {
                    failed.store(true, Ordering::Relaxed);
                    error.lock().unwrap().get_or_insert(e);
                }
            }
        }));
    }
    pool.run_batch(jobs);

    if let Some(e) = error.lock().unwrap().take() {
        return Err(RtError::new(e));
    }
    if !collect {
        return Ok(Value::Undefined);
    }
    let slots = Arc::try_unwrap(results)
        .map_err(|_| RtError::new("internal: result refs leaked"))?
        .into_inner()
        .unwrap();
    let vals: Vec<Value> = slots
        .into_iter()
        .map(|s| rehydrate(&s.expect("chunk skipped without error"), &mut realm.heap))
        .collect();
    Ok(Value::Array(realm.heap.alloc_arr(vals)))
}

/// Execute one chunk in a fresh scratch realm. The realm (arena) drops at
/// the end — region-based memory management, no GC on the parallel path.
fn run_chunk(
    pv_fn: &PortableValue,
    chunk: &[(usize, PortableValue)],
    collect: bool,
) -> Result<Vec<(usize, PortableValue)>, String> {
    let mut realm = Realm::new();
    // scratch realm: never collects (the rehydrated callback lives in a
    // Rust local, unrooted); the whole arena drops when the chunk ends
    realm.gc_enabled = false;
    tsr_io::install(&mut realm);
    install(&mut realm, None); // nested parallel.* shares the pool
    let f = rehydrate(pv_fn, &mut realm.heap);
    let mut out = Vec::with_capacity(if collect { chunk.len() } else { 0 });
    for (i, item) in chunk {
        let v = rehydrate(item, &mut realm.heap);
        let r = call_value(&mut realm, f, &[v, Value::Number(*i as f64)])
            .map_err(|e| e.msg)?;
        if collect {
            out.push((*i, clone_out(&realm.heap, r)?));
        }
    }
    Ok(out)
}
