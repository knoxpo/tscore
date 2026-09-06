//! tss-parallel
//!
//! `parallel.*` natives on the work-stealing scheduler. Semantics: clone at
//! spawn (see docs/specifications/concurrency-semantics.md) — the callback's
//! captures and each input chunk are structured-cloned into a fresh scratch
//! realm per chunk; results are cloned back. Deterministic: identical output
//! at any worker count.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tsr_memory::{PromiseError, Value};
use tsr_realm::interpreter::{call_value, drive};
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

/// Pool stats without forcing pool creation (observability must never
/// decide the worker count by initializing the pool first — shared_pool()
/// from an observer thread races install() and locks the worker count at
/// available_parallelism before --workers is applied).
pub fn pool_stats_if_started() -> Option<tsr_scheduler::PoolStats> {
    POOL.get().map(|p| p.stats())
}

fn pool_help(done: &dyn Fn() -> bool) {
    shared_pool().help_until(done);
}

fn pool_busy() -> bool {
    // POOL.get(), not shared_pool(): never force pool creation just to ask.
    POOL.get().is_some_and(|p| p.has_pending())
}

/// Install `parallel` and `runtime.cpu` globals. First call fixes the
/// worker count for the process.
pub fn install(realm: &mut Realm, workers: Option<usize>) {
    let n_workers = configure(workers);
    realm.idle_helper = Some(pool_help);
    realm.pool_busy = Some(pool_busy);
    // warm the pool off the critical path: worker threads are up before the
    // first parallel.map instead of being spawned inside its timed region
    // (configure() above already fixed the worker count, so no race).
    // Once-guarded: install() also runs per scratch realm.
    static WARM: std::sync::Once = std::sync::Once::new();
    WARM.call_once(|| {
        if std::env::var_os("TSC_NO_POOL_WARM").is_none() {
            std::thread::spawn(|| {
                let _ = shared_pool();
            });
        }
    });

    let map = realm.add_native(|realm, args| run_parallel(realm, args, true));
    let for_ = realm.add_native(|realm, args| run_parallel(realm, args, false));
    realm.set_global_obj("parallel", vec![("map", map), ("for", for_)]);

    // runtime.cpu.count
    let cpu = realm.heap.alloc_obj_host();
    realm.heap.obj_set(cpu, Arc::from("count"), Value::number(n_workers as f64));
    let cpu_ref = cpu;

    // runtime.gc.{stats, collect}
    let gc_stats = realm.add_native(|realm, _| {
        let st = realm.gc_stats;
        let heap_slots = realm.heap.cell_bytes() / 8
            + realm.heap.strs.len()
            + realm.heap.foreigns.len();
        let o = realm.heap.alloc_obj_host();
        let fields: Vec<(&str, f64)> = vec![
            ("minorCollections", st.minor_collections as f64),
            ("majorCollections", st.major_collections as f64),
            ("lastFreed", st.last_freed as f64),
            ("lastLive", st.last_live as f64),
            ("liveBytes", st.last_live_bytes as f64),
            ("lastPauseUs", st.last_pause_us as f64),
            ("maxPauseUs", st.max_pause_us as f64),
            ("heapSlots", heap_slots as f64),
            ("promotedSinceMajor", realm.heap.promoted_since_major as f64),
            ("rememberedPeak", realm.heap.remembered_peak as f64),
        ];
        for (k, v) in fields {
            realm.heap.obj_set(o, Arc::from(k), Value::number(v));
        }
        Ok(Value::object(o))
    });
    let gc_collect = realm.add_native(|realm, _| {
        let extra: Vec<Value> = realm
            .microtasks
            .iter()
            .chain(realm.pinned.keys())
            .map(|&r| Value::foreign(r))
            .collect();
        if realm.heap.young.used() > 0 || !realm.heap.nursery.strs.is_empty() {
            tsr_gc::collect_minor(
                &mut realm.heap,
                &mut realm.stack,
                realm.globals.values_mut(),
                &extra,
                &mut realm.gc_stats,
            );
        }
        tsr_gc::collect(
            &mut realm.heap,
            &realm.stack,
            realm.globals.values().chain(extra.iter()),
            &mut realm.gc_stats,
        );
        Ok(Value::UNDEFINED)
    });
    let gc = realm.heap.alloc_obj_host();
    realm.heap.obj_set(gc, Arc::from("stats"), gc_stats);
    realm.heap.obj_set(gc, Arc::from("collect"), gc_collect);
    let gc_ref = gc;

    // runtime.stats(): scheduler + actors + gc counters
    let stats = realm.add_native(|realm, _| {
        let ps = shared_pool().stats();
        let o = realm.heap.alloc_obj_host();
        let fields: Vec<(&str, f64)> = vec![
            ("workers", ps.workers as f64),
            ("tasksExecuted", ps.tasks_executed as f64),
            ("steals", ps.steals as f64),
            ("submits", ps.submits as f64),
            ("parks", ps.parks as f64),
            (
                "actors",
                tsr_actor_count() as f64,
            ),
            ("gcMinor", realm.gc_stats.minor_collections as f64),
            ("gcMajor", realm.gc_stats.major_collections as f64),
        ];
        for (k, v) in fields {
            realm.heap.obj_set(o, Arc::from(k), Value::number(v));
        }
        Ok(Value::object(o))
    });

    realm.set_global_obj(
        "runtime",
        vec![
            ("cpu", Value::object(cpu_ref)),
            ("gc", Value::object(gc_ref)),
            ("stats", stats),
        ],
    );
}

/// Actor count without a dependency edge to tsr-actor (set by tsr-actor).
pub static ACTOR_COUNT_HOOK: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

fn tsr_actor_count() -> usize {
    ACTOR_COUNT_HOOK.load(std::sync::atomic::Ordering::Relaxed)
}


fn run_parallel(
    realm: &mut Realm,
    args: tsr_realm::NativeArgs,
    collect: bool,
) -> Result<Value, RtError> {
    let arr = args.get(realm, 0).as_array();
    let f = args.get(realm, 1);
    let (arr, f) = match (arr, f) {
        (Some(a), f) if f.as_closure().is_some() => (a, f),
        (Some(_), _) if f != Value::UNDEFINED => {
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
            Value::array(realm.heap.alloc_arr_host(&[]))
        } else {
            Value::UNDEFINED
        });
    }
    // chunks inherit the calling task's cancel flag: cancelling a scope
    // child cancels its inner parallel work at the next safepoint
    let parent_cancel = realm.cancel.clone();

    // clone captures + input out of the caller realm
    let pv_fn = Arc::new(clone_out(&realm.heap, f).map_err(RtError::new)?);
    let items: Vec<PortableValue> = {
        let vals = realm.heap.arr(arr).to_vec();
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
    let error: Arc<Mutex<Option<PromiseError>>> = Arc::new(Mutex::new(None));
    let failed = Arc::new(AtomicBool::new(false));

    // async: returns a promise settled by the last-finishing chunk
    let (promise, completer) = realm.promise_pair();
    let completer = Arc::new(Mutex::new(Some(completer)));

    let mut jobs: Vec<Box<dyn FnOnce(&tsr_scheduler::Ctx) + Send>> = Vec::new();
    let mut items = items.into_iter().enumerate().peekable();
    let mut chunks: Vec<Vec<(usize, PortableValue)>> = Vec::new();
    while items.peek().is_some() {
        chunks.push(items.by_ref().take(chunk_size).collect());
    }
    let remaining = Arc::new(AtomicUsize::new(chunks.len()));
    for chunk in chunks {
        let pv_fn = pv_fn.clone();
        let results = results.clone();
        let error = error.clone();
        let failed = failed.clone();
        let remaining = remaining.clone();
        let completer = completer.clone();
        let parent_cancel = parent_cancel.clone();
        jobs.push(Box::new(move |_ctx| {
            let cancelled_early = failed.load(Ordering::Relaxed)
                || parent_cancel
                    .as_ref()
                    .is_some_and(|c| c.load(Ordering::Relaxed));
            if !cancelled_early {
                match run_chunk(&pv_fn, &chunk, collect, parent_cancel.clone()) {
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
            } else if let Some(c) = &parent_cancel {
                if c.load(Ordering::Relaxed) {
                    error.lock().unwrap().get_or_insert(PromiseError {
                        msg: "task cancelled".into(),
                        cancelled: true,
                        span: None,
                        source: None,
                    });
                }
            }
            if remaining.fetch_sub(1, Ordering::SeqCst) == 1 {
                // last chunk assembles and settles
                let completer = completer.lock().unwrap().take().unwrap();
                if let Some(e) = error.lock().unwrap().take() {
                    completer.settle(Err(e));
                } else if collect {
                    let slots = std::mem::take(&mut *results.lock().unwrap());
                    let vals: Vec<PortableValue> = slots
                        .into_iter()
                        .map(|s| s.expect("chunk skipped without error"))
                        .collect();
                    completer.settle(Ok(PortableValue::Array(vals)));
                } else {
                    completer.settle(Ok(PortableValue::Undefined));
                }
            }
        }));
    }
    let batch = tsr_scheduler::Batch::new();
    for job in jobs {
        pool.submit(&batch, job);
    }
    Ok(promise)
}

/// Execute one chunk in a fresh scratch realm. The realm (arena) drops at
/// the end — region-based memory management, no GC on the parallel path.
fn run_chunk(
    pv_fn: &PortableValue,
    chunk: &[(usize, PortableValue)],
    collect: bool,
    cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
) -> Result<Vec<(usize, PortableValue)>, PromiseError> {
    let mut realm = Realm::new();
    // scratch realm: never collects (the rehydrated callback lives in a
    // Rust local, unrooted); the whole arena drops when the chunk ends
    realm.gc_enabled = false;
    realm.cancel = cancel;
    tsr_io::install(&mut realm);
    tsr_channel::install(&mut realm);
    install(&mut realm, None); // nested parallel.* shares the pool
    let f = rehydrate(pv_fn, &mut realm.heap);
    let mut out = Vec::with_capacity(if collect { chunk.len() } else { 0 });
    for (i, item) in chunk {
        let v = rehydrate(item, &mut realm.heap);
        let r = call_value(&mut realm, f, &[v, Value::number(*i as f64)])
            .and_then(|r| settle_if_promise(&mut realm, r))
            .map_err(|e| PromiseError {
                msg: e.msg,
                cancelled: e.cancelled,
                span: e.span,
                source: e.source,
            })?;
        if collect {
            out.push((*i, clone_out(&realm.heap, r).map_err(|msg| PromiseError {
                msg,
                cancelled: false,
                span: None,
                source: None,
            })?));
        }
    }
    Ok(out)
}

/// Async callbacks return a promise: drive this realm's event loop until
/// it settles (worker thread blocks meanwhile — it owns this realm).
pub fn settle_if_promise(realm: &mut Realm, v: Value) -> Result<Value, RtError> {
    match v.as_foreign() {
        Some(r) => drive(realm, r),
        None => Ok(v),
    }
}
