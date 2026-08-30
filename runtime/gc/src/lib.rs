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
    debug_assert!(
        heap.nursery.objs.is_empty()
            && heap.nursery.arrs.is_empty()
            && heap.nursery.strs.is_empty(),
        "major GC with a non-empty nursery: evacuate first"
    );
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


/// Minor collection: trace only the young generation. Old objects stop
/// traversal (their new edges are covered by the remembered set). Sweep
/// walks the young allocation logs only — O(young), not O(heap).
// Cheney scan-queue kinds (packed (kind << 32) | ref in GcScratch.scan)
const K_OBJ: u64 = 0;
const K_ARR: u64 = 1;
const K_CLOSURE: u64 = 2;
const K_CELL: u64 = 3;
const K_FOREIGN: u64 = 4;

/// Forward one edge: young refs are evacuated into the old arenas (via
/// the forwarding table) and the edge rewritten; old-space young-log
/// survivors are marked and queued. Non-moving kinds (closure/cell/
/// foreign) are marked + queued only.
fn forward(
    v: Value,
    heap: &mut Heap,
    m: &mut tsr_memory::GcScratch,
    nur: &mut tsr_memory::Nursery,
) -> Value {
    use tsr_memory::YOUNG_BIT;
    match v.kind() {
        Kind::Object(r) => {
            if r & YOUNG_BIT != 0 {
                let yi = (r & !YOUNG_BIT) as usize;
                if m.fwd_objs[yi] == u32::MAX {
                    let o = std::mem::take(&mut nur.objs[yi]);
                    let nr = heap.promote_obj(o);
                    m.fwd_objs[yi] = nr;
                    m.scan.push((K_OBJ << 32) | nr as u64);
                }
                Value::object(m.fwd_objs[yi])
            } else {
                if !heap.gen_objs.old.get(r) && !m.yobjs.get(r) {
                    m.yobjs.set(r);
                    m.scan.push((K_OBJ << 32) | r as u64);
                }
                v
            }
        }
        Kind::Array(r) => {
            if r & YOUNG_BIT != 0 {
                let yi = (r & !YOUNG_BIT) as usize;
                if m.fwd_arrs[yi] == u32::MAX {
                    let a = std::mem::take(&mut nur.arrs[yi]);
                    let nr = heap.promote_arr(a);
                    m.fwd_arrs[yi] = nr;
                    m.scan.push((K_ARR << 32) | nr as u64);
                }
                Value::array(m.fwd_arrs[yi])
            } else {
                if !heap.gen_arrs.old.get(r) && !m.yarrs.get(r) {
                    m.yarrs.set(r);
                    m.scan.push((K_ARR << 32) | r as u64);
                }
                v
            }
        }
        Kind::Str(r) => {
            if r & YOUNG_BIT != 0 {
                let yi = (r & !YOUNG_BIT) as usize;
                if m.fwd_strs[yi] == u32::MAX {
                    let s = std::mem::replace(
                        &mut nur.strs[yi],
                        tsr_memory::HStr::Buf(String::new()),
                    );
                    m.fwd_strs[yi] = heap.promote_str(s);
                }
                Value::str_ref(m.fwd_strs[yi])
            } else {
                if !heap.gen_strs.old.get(r) {
                    m.ystrs.set(r);
                }
                v
            }
        }
        Kind::Closure(r) => {
            if !heap.gen_closures.old.get(r) && !m.yclosures.get(r) {
                m.yclosures.set(r);
                m.scan.push((K_CLOSURE << 32) | r as u64);
            }
            v
        }
        Kind::Cell(r) => {
            if !heap.gen_cells.old.get(r) && !m.ycells.get(r) {
                m.ycells.set(r);
                m.scan.push((K_CELL << 32) | r as u64);
            }
            v
        }
        Kind::Foreign(r) => {
            if !heap.gen_foreigns.old.get(r) && !m.yforeigns.get(r) {
                m.yforeigns.set(r);
                m.scan.push((K_FOREIGN << 32) | r as u64);
            }
            v
        }
        _ => v,
    }
}

/// Minor collection: evacuating trace. Young (nursery) survivors move to
/// the old arenas with every reachable edge rewritten; old-space young-log
/// survivors are marked in place and swept as before. Old containers stop
/// traversal (remembered set covers their new edges). Dead nursery slots
/// are never visited beyond a buffer-salvage pass.
pub fn collect_minor<'a>(
    heap: &mut Heap,
    stack: &mut [Value],
    globals: impl Iterator<Item = &'a mut Value>,
    extra: &[Value],
    stats: &mut GcStats,
) {
    let t0 = std::time::Instant::now();

    // persistent minor-mark bitmaps + queues (no per-collection mallocs)
    let mut m = std::mem::take(&mut heap.gc_scratch);
    let mut nur = std::mem::take(&mut heap.nursery);
    m.ystrs.clear_all();
    m.yobjs.clear_all();
    m.yarrs.clear_all();
    m.yclosures.clear_all();
    m.ycells.clear_all();
    m.yforeigns.clear_all();
    m.fwd_objs.clear();
    m.fwd_objs.resize(nur.objs.len(), u32::MAX);
    m.fwd_arrs.clear();
    m.fwd_arrs.resize(nur.arrs.len(), u32::MAX);
    m.fwd_strs.clear();
    m.fwd_strs.resize(nur.strs.len(), u32::MAX);
    m.scan.clear();

    // roots: rewrite in place
    for slot in stack.iter_mut() {
        *slot = forward(*slot, heap, &mut m, &mut nur);
    }
    for g in globals {
        *g = forward(*g, heap, &mut m, &mut nur);
    }
    for &v in extra {
        // pinned/microtask foreigns: non-moving, scan contents only
        let _ = forward(v, heap, &mut m, &mut nur);
    }
    // remembered old containers: fix their edges, not themselves
    let remembered = std::mem::take(&mut heap.remembered);
    stats_remembered_peak(heap, remembered.len());
    for &v in &remembered {
        match v.kind() {
            Kind::Object(r) => m.scan.push((K_OBJ << 32) | r as u64),
            Kind::Array(r) => m.scan.push((K_ARR << 32) | r as u64),
            Kind::Cell(r) => m.scan.push((K_CELL << 32) | r as u64),
            Kind::Foreign(r) => m.scan.push((K_FOREIGN << 32) | r as u64),
            _ => {}
        }
    }

    // transitive fixup: index-based re-borrow per edge (forward() may push
    // into the same arena Vec — no borrow spans a promotion)
    while let Some(item) = m.scan.pop() {
        let r = item as u32 as usize;
        match item >> 32 {
            K_OBJ => {
                let n = heap.objs[r].vlen();
                for i in 0..n {
                    let v = heap.objs[r].val(i);
                    let nv = forward(v, heap, &mut m, &mut nur);
                    heap.objs[r].set_val(i, nv);
                }
            }
            K_ARR => {
                let n = heap.arrs[r].len();
                for i in 0..n {
                    let v = heap.arrs[r][i];
                    let nv = forward(v, heap, &mut m, &mut nur);
                    heap.arrs[r][i] = nv;
                }
            }
            K_CLOSURE => {
                let n = heap.closures[r].upvals.len();
                for i in 0..n {
                    let v = heap.closures[r].upvals[i];
                    let nv = forward(v, heap, &mut m, &mut nur);
                    heap.closures[r].upvals[i] = nv;
                }
            }
            K_CELL => {
                let v = heap.cells[r];
                heap.cells[r] = forward(v, heap, &mut m, &mut nur);
            }
            _ => {
                // K_FOREIGN: take/put-back — cannot hold &mut contents
                // while forward() needs &mut heap
                let mut f = std::mem::replace(&mut heap.foreigns[r], Foreign::Free);
                match &mut f {
                    Foreign::Promise(p) => {
                        if let PromiseState::Fulfilled(v) = &mut p.state {
                            *v = forward(*v, heap, &mut m, &mut nur);
                        }
                        for &c in &p.reactions {
                            let _ = forward(Value::foreign(c), heap, &mut m, &mut nur);
                        }
                    }
                    Foreign::Coroutine(co) => {
                        for i in 0..co.regs.len() {
                            let v = co.regs[i];
                            co.regs[i] = forward(v, heap, &mut m, &mut nur);
                        }
                        if let Some(c) = co.closure {
                            let _ = forward(Value::closure(c), heap, &mut m, &mut nur);
                        }
                        let _ = forward(Value::foreign(co.promise), heap, &mut m, &mut nur);
                    }
                    _ => {}
                }
                heap.foreigns[r] = f;
            }
        }
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
    {
        // manual sweep for foreigns: resume() frees consumed coroutine
        // slots explicitly (free_foreign) while their young-log entry
        // remains — pushing them to the free list again here would hand
        // the same slot to two allocations
        let young = std::mem::take(&mut heap.gen_foreigns.young);
        for r in young {
            if matches!(heap.foreigns[r as usize], Foreign::Free) {
                continue; // already freed explicitly
            }
            if m.yforeigns.get(r) {
                heap.gen_foreigns.old.set(r);
                promoted += 1;
                promoted_bytes += slot_bytes(heap, "gen_foreigns", r);
            } else {
                if let Foreign::Promise(p) = &heap.foreigns[r as usize] {
                    if let PromiseState::Rejected(e) = &p.state {
                        if p.reactions.is_empty() && !e.cancelled {
                            eprintln!(
                                "warning: unhandled promise rejection: {}",
                                e.msg
                            );
                        }
                    }
                }
                heap.foreigns[r as usize] = Foreign::Free;
                heap.free_foreigns.push(r);
                freed += 1;
            }
        }
    }

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

    // harvest backing buffers from dead nursery slots, then reset by
    // truncation — dead objects get only this salvage touch
    let mut evacuated = 0usize;
    for (i, o) in nur.objs.iter_mut().enumerate() {
        if m.fwd_objs[i] != u32::MAX {
            evacuated += 1;
        } else if o.overflow.capacity() > 0 && heap.pool_arr_bufs.len() < 4096 {
            let mut b = std::mem::take(&mut o.overflow);
            b.clear();
            if b.capacity() <= 1024 {
                heap.pool_arr_bufs.push(b);
            }
        }
    }
    for (i, a) in nur.arrs.iter_mut().enumerate() {
        if m.fwd_arrs[i] != u32::MAX {
            evacuated += 1;
        } else if a.capacity() > 0 && heap.pool_arr_bufs.len() < 4096 {
            let mut b = std::mem::take(a);
            b.clear();
            if b.capacity() <= 1024 {
                heap.pool_arr_bufs.push(b);
            }
        }
    }
    for (i, hs) in nur.strs.iter_mut().enumerate() {
        if m.fwd_strs[i] != u32::MAX {
            evacuated += 1;
        } else if let tsr_memory::HStr::Buf(b) = hs {
            if b.capacity() > 0 && heap.pool_str_bufs.len() < 4096 {
                let mut b = std::mem::take(b);
                b.clear();
                if b.capacity() <= 1024 {
                    heap.pool_str_bufs.push(b);
                }
            }
        }
    }
    let nursery_dead =
        nur.objs.len() + nur.arrs.len() + nur.strs.len() - evacuated;
    freed += nursery_dead;
    nur.objs.clear();
    nur.arrs.clear();
    nur.strs.clear();
    nur.bytes = 0;
    heap.nursery = nur;

    heap.promoted_since_major += promoted;
    heap.promoted_bytes_since_major += promoted_bytes;
    heap.allocs_since_gc = 0;
    heap.gc_scratch = m;
    stats.minor_collections += 1;
    stats.last_freed = freed;
    if std::env::var_os("TSC_GC_DEBUG").is_some() {
        eprintln!(
            "[gc] minor freed={freed} promoted={promoted} evac={evacuated} arena={} free_objs={} free_arrs={}",
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
