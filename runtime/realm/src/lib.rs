//! tsr-realm
//!
//! A realm owns a heap, a globals table, and a native-function table, and
//! executes bytecode on the register-machine interpreter in [`interp`].

pub mod interp;
pub mod jit;
pub mod layout;

use rustc_hash::FxHashMap;
use std::sync::Arc;
use tsr_memory::{Heap, PromiseError, Value};
use tsr_task::PortableValue;

/// Live runtime stats shared with the observability exporter thread.
#[derive(Default)]
pub struct StatsHub {
    pub gc_minor: std::sync::atomic::AtomicU64,
    pub gc_major: std::sync::atomic::AtomicU64,
    pub gc_last_pause_us: std::sync::atomic::AtomicU64,
    pub gc_max_pause_us: std::sync::atomic::AtomicU64,
    pub live_bytes: std::sync::atomic::AtomicU64,
    pub heap_slots: std::sync::atomic::AtomicU64,
}

/// Cross-thread completion event for a pinned promise.
pub struct Wake {
    pub promise: tsr_memory::Ref,
    pub result: Result<PortableValue, PromiseError>,
}

/// Send-able, settle-exactly-once handle for completing a realm promise
/// from another thread. Dropping without settling reports an error (so a
/// lost completer surfaces instead of deadlocking).
pub struct Completer {
    promise: tsr_memory::Ref,
    tx: crossbeam_channel::Sender<Wake>,
    settled: bool,
}

impl Completer {
    pub fn settle(mut self, result: Result<PortableValue, PromiseError>) {
        self.settled = true;
        let _ = self.tx.send(Wake { promise: self.promise, result });
    }

    pub fn fail(self, msg: impl Into<String>) {
        self.settle(Err(PromiseError {
            msg: msg.into(),
            cancelled: false,
            span: None,
        }));
    }
}

impl Drop for Completer {
    fn drop(&mut self) {
        if !self.settled {
            let _ = self.tx.send(Wake {
                promise: self.promise,
                result: Err(PromiseError {
                    msg: "internal: completer dropped without settling".into(),
                    cancelled: false,
                    span: None,
                }),
            });
        }
    }
}

#[derive(Debug, Clone)]
pub struct RtError {
    pub msg: String,
    /// Source byte offset, when known.
    pub span: Option<u32>,
    /// True when this "error" is cooperative cancellation, not a failure.
    pub cancelled: bool,
}

impl RtError {
    pub fn new(msg: impl Into<String>) -> Self {
        RtError { msg: msg.into(), span: None, cancelled: false }
    }

    pub fn cancelled() -> Self {
        RtError { msg: "task cancelled".into(), span: None, cancelled: true }
    }
}

impl std::fmt::Display for RtError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "runtime error: {}", self.msg)
    }
}

pub type NativeFn =
    Arc<dyn Fn(&mut Realm, &[Value]) -> Result<Value, RtError> + Send + Sync>;

pub struct Realm {
    pub heap: Heap,
    pub globals: FxHashMap<Arc<str>, Value>,
    pub natives: Vec<NativeFn>,
    /// Register stack shared by all frames in this realm.
    pub stack: Vec<Value>,
    /// Scratch realms (parallel chunks) set this false: they drop wholesale.
    pub gc_enabled: bool,
    /// Scratch realms disable JIT: per-chunk compile never amortizes and
    /// contends on the shared code-heap lock.
    pub jit_enabled: bool,
    pub gc_stats: tsr_gc::GcStats,
    /// Per-realm memory ceiling (approximate live bytes at major GC).
    pub max_heap_bytes: Option<usize>,
    /// Set when the memory limit was exceeded; raised at the next safepoint.
    oom: Option<String>,
    /// Observability hub, published to after each collection.
    pub stats_hub: Option<Arc<StatsHub>>,
    /// Error slot for JIT helper failures (out-of-band from the ABI return).
    pub jit_error: Option<RtError>,
    /// Reused scratch for Concat (avoids a malloc/free per concat).
    pub concat_buf: String,
    /// Cooperative cancellation flag, checked at interpreter safepoints.
    pub cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// Coroutines (foreign refs) ready to resume.
    pub microtasks: std::collections::VecDeque<tsr_memory::Ref>,
    /// Foreign refs kept alive across GC (main promise, cross-thread
    /// completions in flight).
    pub pinned: rustc_hash::FxHashSet<tsr_memory::Ref>,
    /// Promises with an outstanding cross-thread Completer.
    pub external_pending: usize,
    /// While waiting for wakes, help execute pool work (set by
    /// tss-parallel::install; the pred returns true when a wake arrived).
    /// Without it, `--workers 1` (zero pool threads) would deadlock.
    pub idle_helper: Option<fn(&dyn Fn() -> bool)>,
    pub wake_tx: crossbeam_channel::Sender<Wake>,
    pub wake_rx: crossbeam_channel::Receiver<Wake>,
}

impl Realm {
    pub fn new() -> Self {
        let (wake_tx, wake_rx) = crossbeam_channel::unbounded();
        Realm {
            heap: Heap::new(),
            globals: FxHashMap::default(),
            natives: Vec::new(),
            // page-sized reservation: keeps per-thread register stacks off
            // shared cache lines / size-class neighborhoods
            stack: Vec::with_capacity(4096),
            gc_enabled: true,
            jit_enabled: true,
            gc_stats: tsr_gc::GcStats::default(),
            max_heap_bytes: None,
            oom: None,
            stats_hub: None,
            jit_error: None,
            concat_buf: String::new(),
            cancel: None,
            microtasks: std::collections::VecDeque::new(),
            pinned: rustc_hash::FxHashSet::default(),
            external_pending: 0,
            idle_helper: None,
            wake_tx: wake_tx.clone(),
            wake_rx,
        }
    }

    /// Safepoint: cancellation check, then GC check. Called at loop
    /// back-edges and closure calls.
    pub fn safepoint(&mut self) -> Result<(), RtError> {
        if let Some(c) = &self.cancel {
            if c.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(RtError::cancelled());
            }
        }
        self.maybe_gc();
        if let Some(msg) = self.oom.take() {
            return Err(RtError::new(msg));
        }
        Ok(())
    }

    /// Allocate a pending promise + a Send-able completer for it. The
    /// promise stays pinned (GC root) until its Wake is processed.
    pub fn promise_pair(&mut self) -> (Value, Completer) {
        let promise = self.heap.alloc_promise();
        self.pinned.insert(promise);
        self.external_pending += 1;
        (
            Value::foreign(promise),
            Completer { promise, tx: self.wake_tx.clone(), settled: false },
        )
    }

    pub fn maybe_gc(&mut self) {
        if self.gc_enabled && self.heap.needs_gc() {
            let extra: Vec<Value> = self
                .microtasks
                .iter()
                .chain(self.pinned.iter())
                .map(|&r| Value::foreign(r))
                .collect();
            // major when the old generation grew ~50% since the last major
            // or the remembered set degenerated; minor otherwise
            static MAJOR_ONLY: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
            let major_only =
                *MAJOR_ONLY.get_or_init(|| std::env::var_os("TSCORE_GC_MAJOR_ONLY").is_some());
            let major = major_only
                || self.heap.promoted_since_major
                    > (self.gc_stats.last_live / 2).max(64 * 1024)
                || self.heap.promoted_bytes_since_major
                    > (self.gc_stats.last_live_bytes / 2).max(8 << 20)
                || self.heap.remembered.len() > 32 * 1024;
            if major {
                tsr_gc::collect(
                    &mut self.heap,
                    &self.stack,
                    self.globals.values().chain(extra.iter()),
                    &mut self.gc_stats,
                );
            } else {
                tsr_gc::collect_minor(
                    &mut self.heap,
                    &self.stack,
                    self.globals.values().chain(extra.iter()),
                    &mut self.gc_stats,
                );
            }
            self.check_memory_limit();
            self.publish_stats();
        }
    }

    pub fn publish_stats(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        if let Some(hub) = &self.stats_hub {
            hub.gc_minor.store(self.gc_stats.minor_collections, Relaxed);
            hub.gc_major.store(self.gc_stats.major_collections, Relaxed);
            hub.gc_last_pause_us.store(self.gc_stats.last_pause_us, Relaxed);
            hub.gc_max_pause_us.store(self.gc_stats.max_pause_us, Relaxed);
            hub.live_bytes.store(self.gc_stats.last_live_bytes as u64, Relaxed);
            let slots = self.heap.objs.len()
                + self.heap.arrs.len()
                + self.heap.strs.len()
                + self.heap.closures.len()
                + self.heap.cells.len()
                + self.heap.foreigns.len();
            hub.heap_slots.store(slots as u64, Relaxed);
        }
    }

    fn check_memory_limit(&mut self) {
        // enforced on major-GC numbers (accurate); burst between majors is
        // bounded by the nursery + promotion trigger
        if let Some(limit) = self.max_heap_bytes {
            let live = self.gc_stats.last_live_bytes;
            if live > limit {
                self.oom = Some(format!(
                    "realm memory limit exceeded: {:.1} MB live > {:.1} MB limit",
                    live as f64 / 1e6,
                    limit as f64 / 1e6
                ));
            }
        }
    }

    pub fn add_native(
        &mut self,
        f: impl Fn(&mut Realm, &[Value]) -> Result<Value, RtError> + Send + Sync + 'static,
    ) -> Value {
        self.natives.push(Arc::new(f));
        Value::native((self.natives.len() - 1) as u32)
    }

    pub fn set_global(&mut self, name: &str, v: Value) {
        self.globals.insert(Arc::from(name), v);
    }

    /// Build an object global out of named members, e.g. `console`, `Math`.
    pub fn set_global_obj(&mut self, name: &str, members: Vec<(&str, Value)>) {
        let mut obj = tsr_memory::Obj::default();
        for (k, v) in members {
            obj.set(Arc::from(k), v);
        }
        let r = self.heap.alloc_obj(obj);
        self.set_global(name, Value::object(r));
    }

    pub fn alloc_string(&mut self, s: &str) -> Value {
        // buffer-reusing arena slot: steady-state string churn stops
        // touching malloc
        Value::str_ref(self.heap.alloc_str_copy(s))
    }
}

impl Default for Realm {
    fn default() -> Self {
        Self::new()
    }
}
