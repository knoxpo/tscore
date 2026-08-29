//! tsr-realm
//!
//! A realm owns a heap, a globals table, and a native-function table, and
//! executes bytecode on the register-machine interpreter in [`interp`].

pub mod interp;

use rustc_hash::FxHashMap;
use std::sync::Arc;
use tsr_memory::{Heap, Value};

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
    pub gc_stats: tsr_gc::GcStats,
    /// Cooperative cancellation flag, checked at interpreter safepoints.
    pub cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}

impl Realm {
    pub fn new() -> Self {
        Realm {
            heap: Heap::new(),
            globals: FxHashMap::default(),
            natives: Vec::new(),
            // page-sized reservation: keeps per-thread register stacks off
            // shared cache lines / size-class neighborhoods
            stack: Vec::with_capacity(4096),
            gc_enabled: true,
            gc_stats: tsr_gc::GcStats::default(),
            cancel: None,
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
        Ok(())
    }

    pub fn maybe_gc(&mut self) {
        if self.gc_enabled && self.heap.needs_gc() {
            tsr_gc::collect(
                &mut self.heap,
                &self.stack,
                self.globals.values(),
                &mut self.gc_stats,
            );
        }
    }

    pub fn add_native(
        &mut self,
        f: impl Fn(&mut Realm, &[Value]) -> Result<Value, RtError> + Send + Sync + 'static,
    ) -> Value {
        self.natives.push(Arc::new(f));
        Value::Native((self.natives.len() - 1) as u32)
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
        self.set_global(name, Value::Object(r));
    }

    pub fn alloc_string(&mut self, s: &str) -> Value {
        Value::Str(self.heap.alloc_str(Arc::from(s)))
    }
}

impl Default for Realm {
    fn default() -> Self {
        Self::new()
    }
}
