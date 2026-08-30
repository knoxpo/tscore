//! tsr-gc
//!
//! Stop-the-realm mark-sweep for the main realm. Non-moving: the sweep
//! rebuilds per-arena free lists, allocation reuses freed slots. Scratch
//! realms (parallel chunks) never collect — they drop wholesale.
//! Per-realm heaps mean this never synchronizes across threads.

use tsr_memory::{Foreign, Heap, Kind, PromiseState, Ref, Value};

#[derive(Debug, Default, Clone, Copy)]
pub struct GcStats {
    pub minor_collections: u64,
    pub major_collections: u64,
    pub last_freed: usize,
    pub last_live: usize,
    /// Approximate live bytes measured during the last major collection.
    pub last_live_bytes: usize,
    pub last_pause_us: u64,
    pub max_pause_us: u64,
}

impl GcStats {
    pub fn collections(&self) -> u64 {
        self.minor_collections + self.major_collections
    }

    fn record_pause(&mut self, start: std::time::Instant) {
        let us = start.elapsed().as_micros() as u64;
        self.last_pause_us = us;
        self.max_pause_us = self.max_pause_us.max(us);
    }
}

/// Reset a persistent mark vector: zero what exists, extend to arena size.
fn reset_marks(v: &mut Vec<bool>, len: usize) {
    v.clear();
    v.resize(len, false);
}

/// Collect the heap given its roots: the register stack and the globals.
pub fn collect<'a>(
    heap: &mut Heap,
    stack: &[Value],
    globals: impl Iterator<Item = &'a Value>,
    stats: &mut GcStats,
) {
    let t0 = std::time::Instant::now();
    // persistent scratch: no per-collection mark-vector mallocs
    let mut m = std::mem::take(&mut heap.gc_scratch);
    reset_marks(&mut m.strs, heap.strs.len());
    reset_marks(&mut m.objs, heap.objs.len());
    reset_marks(&mut m.arrs, heap.arrs.len());
    reset_marks(&mut m.closures, heap.closures.len());
    reset_marks(&mut m.cells, heap.cells.len());
    reset_marks(&mut m.foreigns, heap.foreigns.len());
    let mut work = std::mem::take(&mut m.work);
    work.clear();
    work.extend_from_slice(stack);
    work.extend(globals.copied());

    while let Some(v) = work.pop() {
        match v.kind() {
            Kind::Str(r) => {
                m.strs[r as usize] = true;
            }
            Kind::Object(r) => {
                if !mark(&mut m.objs, r) {
                    let o = &heap.objs[r as usize];
                    let n = o.vlen().min(tsr_memory::OBJ_INLINE);
                    work.extend_from_slice(&o.inline[..n]);
                    work.extend_from_slice(&o.overflow);
                }
            }
            Kind::Array(r) => {
                if !mark(&mut m.arrs, r) {
                    work.extend_from_slice(&heap.arrs[r as usize]);
                }
            }
            Kind::Closure(r) => {
                if !mark(&mut m.closures, r) {
                    work.extend_from_slice(&heap.closures[r as usize].upvals);
                }
            }
            Kind::Cell(r) => {
                if !mark(&mut m.cells, r) {
                    work.push(heap.cells[r as usize]);
                }
            }
            Kind::Foreign(r) => {
                if !mark(&mut m.foreigns, r) {
                    match &heap.foreigns[r as usize] {
                        Foreign::Promise(p) => {
                            if let PromiseState::Fulfilled(v) = p.state {
                                work.push(v);
                            }
                            work.extend(p.reactions.iter().map(|&c| Value::foreign(c)));
                        }
                        Foreign::Coroutine(co) => {
                            work.extend_from_slice(&co.regs);
                            if let Some(c) = co.closure {
                                work.push(Value::closure(c));
                            }
                            work.push(Value::foreign(co.promise));
                        }
                        Foreign::Handle(..) | Foreign::Free => {}
                    }
                }
            }
            _ => {}
        }
    }

    // one fused pass per arena: rebuild the free list, set old bits for
    // survivors, accumulate live bytes — instead of three separate walks
    let mut freed = 0usize;
    let mut live_bytes = 0usize;
    macro_rules! fused_sweep {
        ($marks:expr, $free:ident, $gen:ident, $bytes:expr, $clear:expr) => {{
            heap.$free.clear();
            heap.$gen.old.clear_all();
            heap.$gen.dirty.clear_all();
            heap.$gen.young.clear();
            for (i, &alive) in $marks.iter().enumerate() {
                let r = i as Ref;
                if alive {
                    heap.$gen.old.set(r);
                    let b: &dyn Fn(&Heap, Ref) -> usize = &$bytes;
                    live_bytes += b(&heap, r);
                } else {
                    let c: &mut dyn FnMut(&mut Heap, Ref) = &mut $clear;
                    c(heap, r);
                    heap.$free.push(r);
                    freed += 1;
                }
            }
        }};
    }
    fused_sweep!(m.strs, free_strs, gen_strs,
        |h: &Heap, r: Ref| 24 + h.strs[r as usize].as_str().len(),
        |h: &mut Heap, r: Ref| {
            if let tsr_memory::HStr::Shared(_) = h.strs[r as usize] {
                h.strs[r as usize] = tsr_memory::HStr::Buf(String::new());
            } else if let tsr_memory::HStr::Buf(b) = &mut h.strs[r as usize] {
                b.clear();
                if b.capacity() > 1024 {
                    b.shrink_to(64); // don't hoard giant buffers
                }
            }
        });
    fused_sweep!(m.objs, free_objs, gen_objs,
        |h: &Heap, r: Ref| 72 + h.objs[r as usize].overflow.len() * 8,
        |h: &mut Heap, r: Ref| {
            let o = &mut h.objs[r as usize];
            o.shape = tsr_memory::empty_shape();
            o.clear_vals(); // keeps overflow capacity
        });
    fused_sweep!(m.arrs, free_arrs, gen_arrs,
        |h: &Heap, r: Ref| 32 + h.arrs[r as usize].len() * 8,
        |h: &mut Heap, r: Ref| {
            let v = &mut h.arrs[r as usize];
            v.clear(); // keep capacity: reuse avoids malloc
            if v.capacity() > 1024 {
                v.shrink_to(64);
            }
        });
    fused_sweep!(m.closures, free_closures, gen_closures,
        |h: &Heap, r: Ref| 32 + h.closures[r as usize].upvals.len() * 8,
        |h: &mut Heap, r: Ref| {
            h.closures[r as usize].upvals.clear();
        });
    fused_sweep!(m.cells, free_cells, gen_cells,
        |_h: &Heap, _r: Ref| 8,
        |h: &mut Heap, r: Ref| {
            h.cells[r as usize] = Value::UNDEFINED;
        });
    fused_sweep!(m.foreigns, free_foreigns, gen_foreigns,
        |_h: &Heap, _r: Ref| 96,
        |h: &mut Heap, r: Ref| {
            // unobserved rejection dies here: surface it before dropping
            if let Foreign::Promise(p) = &h.foreigns[r as usize] {
                if let PromiseState::Rejected(e) = &p.state {
                    if p.reactions.is_empty() && !e.cancelled {
                        eprintln!("warning: unhandled promise rejection: {}", e.msg);
                    }
                }
            }
            h.foreigns[r as usize] = Foreign::Free;
        });

    let total = heap.strs.len()
        + heap.objs.len()
        + heap.arrs.len()
        + heap.closures.len()
        + heap.cells.len()
        + heap.foreigns.len();
    stats.major_collections += 1;
    stats.last_freed = freed;
    stats.last_live = total - freed;
    stats.last_live_bytes = live_bytes;

    heap.remembered.clear();
    heap.promoted_since_major = 0;
    heap.promoted_bytes_since_major = 0;

    heap.allocs_since_gc = 0;
    // heap-proportional trigger: workloads whose live set genuinely grows
    // (gc_churn's history chain) would otherwise pay a full trace every
    // fixed 256k allocations — O(live^2) total GC work
    heap.gc_threshold = (1 << 18).max(stats.last_live / 2);
    m.work = work;
    heap.gc_scratch = m;
    if std::env::var_os("TSC_GC_DEBUG").is_some() {
        eprintln!(
            "[gc] MAJOR freed={} live={} arena={} free_objs={} free_arrs={}",
            stats.last_freed,
            stats.last_live,
            heap.objs.len() + heap.arrs.len(),
            heap.free_objs.len(),
            heap.free_arrs.len()
        );
    }
    stats.record_pause(t0);
}

/// Push v's outgoing edges onto the worklist (shared by minor tracing).
fn push_children(heap: &Heap, v: Value, work: &mut Vec<Value>) {
    match v.kind() {
        Kind::Object(r) => {
            let o = &heap.objs[r as usize];
            let n = o.vlen().min(tsr_memory::OBJ_INLINE);
            work.extend_from_slice(&o.inline[..n]);
            work.extend_from_slice(&o.overflow);
        }
        Kind::Array(r) => work.extend_from_slice(&heap.arrs[r as usize]),
        Kind::Closure(r) => {
            work.extend_from_slice(&heap.closures[r as usize].upvals)
        }
        Kind::Cell(r) => work.push(heap.cells[r as usize]),
        Kind::Foreign(r) => match &heap.foreigns[r as usize] {
            Foreign::Promise(p) => {
                if let PromiseState::Fulfilled(v) = p.state {
                    work.push(v);
                }
                work.extend(p.reactions.iter().map(|&c| Value::foreign(c)));
            }
            Foreign::Coroutine(co) => {
                work.extend_from_slice(&co.regs);
                if let Some(c) = co.closure {
                    work.push(Value::closure(c));
                }
                work.push(Value::foreign(co.promise));
            }
            Foreign::Handle(..) | Foreign::Free => {}
        },
        _ => {}
    }
}

/// Minor collection: trace only the young generation. Old objects stop
/// traversal (their new edges are covered by the remembered set). Sweep
/// walks the young allocation logs only — O(young), not O(heap).
pub fn collect_minor<'a>(
    heap: &mut Heap,
    stack: &[Value],
    globals: impl Iterator<Item = &'a Value>,
    stats: &mut GcStats,
) {
    let t0 = std::time::Instant::now();

    // persistent minor-mark bitmaps + worklist (no per-collection mallocs)
    let mut m = std::mem::take(&mut heap.gc_scratch);
    m.ystrs.clear_all();
    m.yobjs.clear_all();
    m.yarrs.clear_all();
    m.yclosures.clear_all();
    m.ycells.clear_all();
    m.yforeigns.clear_all();

    let mut work = std::mem::take(&mut m.work);
    work.clear();
    work.extend_from_slice(stack);
    work.extend(globals.copied());
    // remembered old containers: trace their children, not themselves
    let remembered = std::mem::take(&mut heap.remembered);
    stats_remembered_peak(heap, remembered.len());
    for &v in &remembered {
        push_children(heap, v, &mut work);
    }

    while let Some(v) = work.pop() {
        let (gen, marks, r) = match v.kind() {
            Kind::Str(r) => (&heap.gen_strs, &mut m.ystrs, r),
            Kind::Object(r) => (&heap.gen_objs, &mut m.yobjs, r),
            Kind::Array(r) => (&heap.gen_arrs, &mut m.yarrs, r),
            Kind::Closure(r) => (&heap.gen_closures, &mut m.yclosures, r),
            Kind::Cell(r) => (&heap.gen_cells, &mut m.ycells, r),
            Kind::Foreign(r) => (&heap.gen_foreigns, &mut m.yforeigns, r),
            _ => continue,
        };
        if gen.old.get(r) || marks.get(r) {
            continue;
        }
        marks.set(r);
        push_children(heap, v, &mut work);
    }

    // sweep young logs only
    let mut freed = 0usize;
    let mut promoted = 0usize;
    let mut promoted_bytes = 0usize;
    macro_rules! sweep_young {
        ($gen:ident, $marks:expr, $free:ident, $clear:expr) => {{
            let young = std::mem::take(&mut heap.$gen.young);
            for r in young {
                if $marks.get(r) {
                    heap.$gen.old.set(r);
                    promoted += 1;
                    promoted_bytes += slot_bytes(heap, stringify!($gen), r);
                } else {
                    let clear: &mut dyn FnMut(&mut Heap, tsr_memory::Ref) = &mut $clear;
                    clear(heap, r);
                    heap.$free.push(r);
                    freed += 1;
                }
            }
        }};
    }
    sweep_young!(gen_strs, m.ystrs, free_strs, |h: &mut Heap, r| {
        if let tsr_memory::HStr::Shared(_) = h.strs[r as usize] {
            h.strs[r as usize] = tsr_memory::HStr::Buf(String::new());
        } else if let tsr_memory::HStr::Buf(b) = &mut h.strs[r as usize] {
            b.clear();
            if b.capacity() > 1024 {
                b.shrink_to(64);
            }
        }
    });
    sweep_young!(gen_objs, m.yobjs, free_objs, |h: &mut Heap, r| {
        let o = &mut h.objs[r as usize];
        o.shape = tsr_memory::empty_shape();
        o.clear_vals();
    });
    sweep_young!(gen_arrs, m.yarrs, free_arrs, |h: &mut Heap, r| {
        let v = &mut h.arrs[r as usize];
        v.clear();
        if v.capacity() > 1024 {
            v.shrink_to(64);
        }
    });
    sweep_young!(gen_closures, m.yclosures, free_closures, |h: &mut Heap, r| {
        h.closures[r as usize].upvals.clear();
    });
    sweep_young!(gen_cells, m.ycells, free_cells, |h: &mut Heap, r| {
        h.cells[r as usize] = Value::UNDEFINED;
    });
    sweep_young!(gen_foreigns, m.yforeigns, free_foreigns, |h: &mut Heap, r| {
        if let Foreign::Promise(p) = &h.foreigns[r as usize] {
            if let PromiseState::Rejected(e) = &p.state {
                if p.reactions.is_empty() && !e.cancelled {
                    eprintln!("warning: unhandled promise rejection: {}", e.msg);
                }
            }
        }
        h.foreigns[r as usize] = Foreign::Free;
    });

    // clear dirty bits for the remembered containers we consumed
    for v in remembered {
        match v.kind() {
            Kind::Object(r) => heap.gen_objs.dirty.clear(r),
            Kind::Array(r) => heap.gen_arrs.dirty.clear(r),
            Kind::Cell(r) => heap.gen_cells.dirty.clear(r),
            Kind::Foreign(r) => heap.gen_foreigns.dirty.clear(r),
            _ => {}
        }
    }

    heap.promoted_since_major += promoted;
    heap.promoted_bytes_since_major += promoted_bytes;
    heap.allocs_since_gc = 0;
    m.work = work;
    heap.gc_scratch = m;
    stats.minor_collections += 1;
    stats.last_freed = freed;
    if std::env::var_os("TSC_GC_DEBUG").is_some() {
        eprintln!(
            "[gc] minor freed={freed} promoted={promoted} arena={} free_objs={} free_arrs={}",
            heap.objs.len() + heap.arrs.len(),
            heap.free_objs.len(),
            heap.free_arrs.len()
        );
    }
    stats.record_pause(t0);
}

fn slot_bytes(heap: &Heap, arena: &str, r: tsr_memory::Ref) -> usize {
    let i = r as usize;
    match arena {
        "gen_strs" => 24 + heap.strs[i].as_str().len(),
        "gen_objs" => 72 + heap.objs[i].overflow.len() * 8,
        "gen_arrs" => 32 + heap.arrs[i].len() * 8,
        "gen_closures" => 32 + heap.closures[i].upvals.len() * 8,
        "gen_cells" => 8,
        _ => 96,
    }
}

fn stats_remembered_peak(heap: &mut Heap, n: usize) {
    heap.remembered_peak = heap.remembered_peak.max(n);
}

fn mark(bits: &mut [bool], r: Ref) -> bool {
    let was = bits[r as usize];
    bits[r as usize] = true;
    was
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn collects_garbage_keeps_live() {
        let mut heap = Heap::new();
        let live_arr = heap.alloc_arr(vec![Value::number(7.0)]);
        for i in 0..1000 {
            heap.alloc_str(Arc::from(format!("garbage {i}").as_str()));
        }
        let live_str = heap.alloc_str(Arc::from("keep"));
        heap.arr_mut(live_arr).push(Value::str_ref(live_str));

        let roots = [Value::array(live_arr)];
        let mut stats = GcStats::default();
        collect(&mut heap, &roots, [].iter(), &mut stats);

        assert_eq!(stats.last_freed, 1000);
        assert_eq!(stats.major_collections, 1);
        assert_eq!(heap.str_at(live_str), "keep");
        // freed slots are reused, arena does not grow
        let len_before = heap.strs.len();
        for _ in 0..500 {
            heap.alloc_str(Arc::from("new"));
        }
        assert_eq!(heap.strs.len(), len_before);
    }

    #[test]
    fn closure_cycle_via_cells_is_collected() {
        let mut heap = Heap::new();
        // cell -> closure -> cell (cycle), unreachable from roots
        let cell = heap.alloc_cell(Value::UNDEFINED);
        let proto = Arc::new(tsc_ir::FunctionProto::default());
        let clo = heap.alloc_closure(tsr_memory::Closure { proto, upvals: vec![Value::cell(cell)] });
        *heap.cell_mut(cell) = Value::closure(clo);

        let mut stats = GcStats::default();
        collect(&mut heap, &[], [].iter(), &mut stats);
        assert_eq!(stats.last_freed, 2);
    }
}
