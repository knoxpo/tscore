//! tsr-gc
//!
//! Stop-the-realm collector for the main realm. Objects, arrays, element
//! chunks, closures and cells live in the bump heap (`tsr_memory::cells`);
//! strings and foreigns keep their arenas. Minor = copy nursery
//! survivors into old space and fix every reachable edge; major =
//! mark-sweep over the old chunks, rebuilding the free lists. Scratch
//! realms (parallel chunks) never collect — they drop wholesale.

use tsr_memory::cells::*;
use tsr_memory::{Foreign, Heap, Kind, PromiseState, Ref, Value};

#[derive(Debug, Default, Clone, Copy)]
pub struct GcStats {
    pub minor_collections: u64,
    pub major_collections: u64,
    pub last_freed: usize,
    pub last_live: usize,
    /// Live bytes measured during the last major collection.
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

fn debug_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("TSC_GC_DEBUG").is_some())
}

/// Set the mark bit; true when it was already set.
#[inline(always)]
fn mark_cell(a: Ref) -> bool {
    let m = meta_at(a);
    if m & M_MARK != 0 {
        return true;
    }
    set_meta(a, m | M_MARK);
    false
}

/// Push every value edge of a cell onto the major worklist, marking its
/// element chunk on the way.
fn trace_cell(heap: &Heap, a: Ref, work: &mut Vec<Value>) {
    let m = meta_at(a);
    match kind_of(m) {
        K_OBJ => {
            let o = heap.obj(a);
            let n = o.vlen();
            work.extend_from_slice(&o.inline[..n.min(OBJ_INLINE)]);
            if o.spill != 0 {
                mark_cell(o.spill);
                work.extend_from_slice(elems_slice(o.spill, n - OBJ_INLINE));
            }
        }
        K_ARR | K_ARR_NUM => {
            let e = word(a, 1);
            mark_cell(e);
            work.extend_from_slice(elems_slice(e, len_of(m)));
        }
        // every element is an int: the chunk is live, its contents are
        // not references, so there is nothing to trace inside it
        K_ARR_I32 => {
            mark_cell(word(a, 1));
        }
        K_CLOSURE => work.extend_from_slice(heap.closure(a).upvals()),
        K_CELL => work.push(*heap.cell(a)),
        k => unreachable!("trace of cell kind {k}"),
    }
}

/// Major collection given the roots: the register stack and the globals.
pub fn collect<'a>(
    heap: &mut Heap,
    stack: &[Value],
    globals: impl Iterator<Item = &'a Value>,
    stats: &mut GcStats,
) {
    assert!(
        heap.young.used() == 0 && heap.nursery.strs.is_empty(),
        "major GC with a non-empty nursery: evacuate first (young={} strs={})",
        heap.young.used(),
        heap.nursery.strs.len()
    );
    let t0 = std::time::Instant::now();
    heap.old.sync_top();
    heap.compact_ropes();
    let mut m = std::mem::take(&mut heap.gc_scratch);
    reset_marks(&mut m.strs, heap.strs.len());
    reset_marks(&mut m.foreigns, heap.foreigns.len());
    let mut work = std::mem::take(&mut m.work);
    work.clear();
    work.extend_from_slice(stack);
    work.extend(globals.copied());
    let verify_roots: Vec<Value> = if verify_on() { work.clone() } else { Vec::new() };
    // before the sweep: did this major inherit the corruption?
    verify_no_stale_marks(heap, "pre-major-marks");
    verify_reachable(heap, &verify_roots, "pre-major");

    while let Some(v) = work.pop() {
        match v.kind() {
            Kind::Str(r) => m.strs[r as usize] = true,
            Kind::Object(a) | Kind::Array(a) | Kind::Closure(a) | Kind::Cell(a) => {
                if !mark_cell(a) {
                    trace_cell(heap, a, &mut work);
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

    verify_marked(heap, &verify_roots);
    // sweep the old chunks: live cells lose their mark and dirty bits and
    // become aged; dead runs coalesce into free cells
    let (mut live_cells, mut freed, mut live_bytes) = (0usize, 0usize, 0usize);
    heap.old.clear_free_lists();
    let n_chunks = heap.old.chunks.len();
    for ci in 0..n_chunks {
        let (base, top) = (heap.old.chunks[ci].base, heap.old.chunks[ci].top);
        let mut a = base;
        let mut dead_run: Option<(usize, usize)> = None; // (start, words)
        while a < top {
            let mt = meta_at(a as Ref);
            let words = size_words(mt);
            debug_assert!(words >= 2, "corrupt cell at {a:#x}");
            if mt & M_MARK != 0 {
                set_meta(a as Ref, (mt & !(M_MARK | M_DIRTY)) | M_AGED);
                live_cells += 1;
                live_bytes += words * 8;
                if let Some((s, w)) = dead_run.take() {
                    heap.old.push_free(s as Ref, w);
                }
            } else {
                if kind_of(mt) != K_FREE {
                    freed += 1;
                }
                dead_run = Some(match dead_run {
                    Some((s, w)) => (s, w + words),
                    None => (a, words),
                });
            }
            a += words * 8;
        }
        if let Some((s, w)) = dead_run {
            heap.old.push_free(s as Ref, w);
        }
    }
    heap.born_old.clear();
    heap.born_elems.clear();
    heap.born_buf.len = 0;
    heap.old.end_minor();
    heap.old.live_bytes = live_bytes;

    // arena kinds: one fused pass each — rebuild the free list, set old
    // bits for survivors, accumulate live bytes
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
                    live_cells += 1;
                } else {
                    let c: &mut dyn FnMut(&mut Heap, Ref) = &mut $clear;
                    c(heap, r);
                    heap.$free.push(r as u32);
                    freed += 1;
                }
            }
        }};
    }
    fused_sweep!(m.strs, free_strs, gen_strs,
        |h: &Heap, r: Ref| 24 + h.strs[r as usize].byte_len(),
        |h: &mut Heap, r: Ref| {
            if !matches!(h.strs[r as usize], tsr_memory::HStr::Buf(_)) {
                // Shared arcs and rope trees are dropped here
                h.strs[r as usize] = tsr_memory::HStr::Buf(String::new());
            } else if let tsr_memory::HStr::Buf(b) = &mut h.strs[r as usize] {
                b.clear();
                if b.capacity() > 1024 {
                    b.shrink_to(64); // don't hoard giant buffers
                }
            }
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

    stats.major_collections += 1;
    verify_reachable(heap, &verify_roots, "major");
    stats.last_freed = freed;
    stats.last_live = live_cells;
    stats.last_live_bytes = live_bytes;

    heap.remembered.clear();
    heap.promoted_since_major = 0;
    heap.promoted_bytes_since_major = 0;
    heap.allocs_since_gc = 0;
    // heap-proportional trigger: workloads whose live set genuinely grows
    // would otherwise pay a full trace every fixed 256k allocations
    heap.gc_threshold = (1 << 18).max(stats.last_live / 2);
    // a major that reclaimed at least half the heap says the survivors
    // pretenuring assumed are not the rule any more
    if !heap.pretenure_fixed && freed * 2 >= live_cells {
        heap.pretenure = 0;
    }
    m.work = work;
    heap.gc_scratch = m;
    if debug_on() {
        eprintln!(
            "[gc] MAJOR us={} freed={} live={} live_bytes={} old_bytes={}",
            t0.elapsed().as_micros(),
            stats.last_freed,
            stats.last_live,
            live_bytes,
            heap.old.allocated_bytes()
        );
    }
    stats.record_pause(t0);
}

/// Forward one edge during a minor. Young obj/arr cells are copied into
/// old space (once: the forwarding word remembers where) and the edge is
/// rewritten. Old cells born since the last minor are marked and queued
/// the first time they are reached; aged old cells stop the walk (their
/// new edges are in the remembered set). Strings and foreigns keep their
/// arena logic.
#[inline]
fn forward(v: Value, heap: &mut Heap, m: &mut tsr_memory::GcScratch, nur: &mut tsr_memory::Nursery) -> Value {
    match v.kind() {
        Kind::Object(a) => Value::object(forward_cell(a, heap, m)),
        Kind::Array(a) => Value::array(forward_cell(a, heap, m)),
        Kind::Closure(a) => Value::closure(forward_cell(a, heap, m)),
        Kind::Cell(a) => Value::cell(forward_cell(a, heap, m)),
        Kind::Str(r) => {
            use tsr_memory::YOUNG_BIT;
            if r & YOUNG_BIT != 0 {
                let yi = (r & !YOUNG_BIT) as usize;
                if m.fwd_strs[yi] == u32::MAX {
                    let s = std::mem::replace(&mut nur.strs[yi], tsr_memory::HStr::Buf(String::new()));
                    m.fwd_strs[yi] = heap.promote_str(s) as u32;
                }
                Value::str_ref(m.fwd_strs[yi] as Ref)
            } else {
                if !heap.gen_strs.old.get(r) {
                    m.ystrs.set(r);
                }
                v
            }
        }
        Kind::Foreign(r) => {
            if !heap.gen_foreigns.old.get(r) && !m.yforeigns.get(r) {
                m.yforeigns.set(r);
                m.scan_foreign.push(r);
            }
            v
        }
        _ => v,
    }
}

/// Copy or mark one cell; returns where it lives now.
#[inline]
fn forward_cell(a: Ref, heap: &mut Heap, m: &mut tsr_memory::GcScratch) -> Ref {
    let mt = meta_at(a);
    if heap.young(a) {
        if mt & M_FWD != 0 {
            return mt & !7;
        }
        let words = size_words(mt);
        let (na, _) = heap.old.alloc(words);
        // SAFETY: both cells are `words` long and do not overlap.
        unsafe { std::ptr::copy_nonoverlapping(a as *const u64, na as *mut u64, words) };
        // The copy inherits the source's flag bits, so strip them: an
        // inherited M_MARK would make the next major skip the cell, and
        // an inherited M_DIRTY would tell `ref_store_check` the cell is
        // already remembered and suppress the barrier on it. Neither is
        // reachable today -- young cells are never marked or dirtied --
        // but this is the third bug in this family, so the promotion
        // stops depending on that.
        debug_young_flags(mt, "cell");
        set_meta(na, (mt & !(M_MARK | M_DIRTY)) | M_AGED);
        set_meta(a, na | M_FWD);
        heap.promoted_bytes_since_major += words * 8;
        heap.promoted_since_major += 1;
        m.scan.push(na);
        return na;
    }
    if mt & (M_AGED | M_MARK) == 0 {
        set_meta(a, mt | M_MARK);
        m.scan.push(a);
    }
    a
}

/// Element chunks are not values: forwarded (copied or marked) by their
/// owner's scan, which then walks the values through the new address.
#[inline]
fn forward_elems(e: Ref, heap: &mut Heap) -> Ref {
    let mt = meta_at(e);
    if heap.young(e) {
        if mt & M_FWD != 0 {
            return mt & !7;
        }
        let words = size_words(mt);
        let (ne, _) = heap.old.alloc(words);
        unsafe { std::ptr::copy_nonoverlapping(e as *const u64, ne as *mut u64, words) };
        debug_young_flags(mt, "elems");
        set_meta(ne, (mt & !(M_MARK | M_DIRTY)) | M_AGED);
        set_meta(e, ne | M_FWD);
        heap.promoted_bytes_since_major += words * 8;
        return ne;
    }
    if mt & (M_AGED | M_MARK) == 0 {
        set_meta(e, mt | M_MARK);
    }
    e
}

/// Fix every edge of one cell.
fn scan_cell(a: Ref, heap: &mut Heap, m: &mut tsr_memory::GcScratch, nur: &mut tsr_memory::Nursery) {
    let mt = meta_at(a);
    match kind_of(mt) {
        K_OBJ => {
            let n = len_of(mt);
            let spill = word(a, OBJ_WORDS - 1);
            if spill != 0 {
                let ns = forward_elems(spill, heap);
                set_word(a, OBJ_WORDS - 1, ns);
            }
            for i in 0..n {
                let v = heap.obj(a).val(i);
                let nv = forward(v, heap, m, nur);
                heap.obj_mut(a).set_val(i, nv);
            }
        }
        K_ARR | K_ARR_NUM => {
            let e = forward_elems(word(a, 1), heap);
            set_word(a, 1, e);
            let n = len_of(mt);
            for i in 0..n {
                let v = elems_slice(e, n)[i];
                elems_slice_mut(e, n)[i] = forward(v, heap, m, nur);
            }
        }
        // int elements are not references: the chunk still moves, its
        // contents need no rewriting
        K_ARR_I32 => {
            let e = forward_elems(word(a, 1), heap);
            set_word(a, 1, e);
        }
        K_CLOSURE => {
            let n = len_of(mt);
            for i in 0..n {
                let v = heap.closure(a).upvals()[i];
                heap.closure_mut(a).upvals_mut()[i] = forward(v, heap, m, nur);
            }
        }
        K_CELL => {
            let v = *heap.cell(a);
            *heap.cell_mut(a) = forward(v, heap, m, nur);
        }
        // Reached from a root or a remembered container, so something
        // live points here. Say enough to tell the two causes apart: a
        // cell freed while still reachable (K_FREE, and usually marked
        // by this very trace) versus a walk that lost cell alignment.
        k => {
            let chunk = heap.old.chunks.iter().position(|c| (c.base..c.end).contains(&(a as usize)));
            panic!(
                "scan of cell kind {k} at {a:#x} meta={mt:#x} size={} aged={} dirty={} mark={} \
                 in_born={} chunk={chunk:?} born_chunk={} born_from={:#x}",
                size_words(mt),
                mt & M_AGED != 0,
                mt & M_DIRTY != 0,
                mt & M_MARK != 0,
                heap.old.in_born_range(a),
                heap.old.born_chunk,
                heap.old.born_from
            )
        }
    }
}

/// Ceiling on consecutive untraced minors. Each one lets a generation's
/// dead cells go unnoticed for another cycle, so this bounds both the
/// garbage held and how late a `pretenure` back-off is seen.
/// Ceiling on consecutive untraced minors. Each one lets a generation's
/// dead cells go unnoticed for another cycle, so this bounds both the
/// garbage held and how late a `pretenure` back-off is seen. Measured on
/// gc_churn: 1 (the old behaviour) 51.1ms of GC, 3 -> 37.5, 7 -> 31.4,
/// 15 -> 28.3, 31 -> 26.5. Past 15 the walk is memory-bandwidth bound and
/// the extra lag buys nothing.
const PRETENURE_RUN_MAX: u8 = 15;

/// Minor collection: evacuate nursery survivors, fix every reachable
/// edge, sweep the young logs (old-space born cells, strings, foreigns).
pub fn collect_minor<'a>(
    heap: &mut Heap,
    stack: &mut [Value],
    globals: impl Iterator<Item = &'a mut Value>,
    extra: &[Value],
    stats: &mut GcStats,
) {
    let t0 = std::time::Instant::now();
    if heap.pretenure != 0 && heap.pretenure_skip > 0 {
        heap.pretenure_skip -= 1;
        bulk_promote(heap, stats);
        stats.record_pause(t0);
        return;
    }
    // before the nursery is detached and before evacuation moves any
    // string ref — compaction rewrites indices in place
    heap.old.sync_top();
    heap.compact_ropes();

    let young_used = heap.young.used();
    let young_top = heap.young.top;
    let mut m = std::mem::take(&mut heap.gc_scratch);
    let mut nur = std::mem::take(&mut heap.nursery);
    m.ystrs.clear_all();
    m.yforeigns.clear_all();
    m.fwd_strs.clear();
    m.fwd_strs.resize(nur.strs.len(), u32::MAX);
    m.scan.clear();
    m.scan_foreign.clear();

    // roots: rewrite in place
    for slot in stack.iter_mut() {
        *slot = forward(*slot, heap, &mut m, &mut nur);
    }
    for g in globals {
        *g = forward(*g, heap, &mut m, &mut nur);
    }
    for &v in extra {
        let _ = forward(v, heap, &mut m, &mut nur);
    }
    let t_roots = t0.elapsed();
    // remembered aged containers: fix their edges, not themselves
    let remembered = std::mem::take(&mut heap.remembered);
    heap.remembered_peak = heap.remembered_peak.max(remembered.len());
    for &v in &remembered {
        match v.kind() {
            Kind::Object(a) | Kind::Array(a) | Kind::Cell(a) => m.scan.push(a),
            Kind::Foreign(r) => m.scan_foreign.push(r),
            _ => {}
        }
    }

    loop {
        if let Some(a) = m.scan.pop() {
            scan_cell(a, heap, &mut m, &mut nur);
            continue;
        }
        let Some(r) = m.scan_foreign.pop() else { break };
        // take/put-back — cannot hold &mut contents while forward() needs &mut heap
        let mut f = std::mem::replace(&mut heap.foreigns[r as usize], Foreign::Free);
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
        heap.foreigns[r as usize] = f;
    }
    let t_scan = t0.elapsed();

    // sweep the old space's young log: reached cells age, the rest free
    let (mut freed, mut promoted, mut promoted_bytes) = (0usize, 0usize, 0usize);
    let born = std::mem::take(&mut heap.born_old);
    // the JIT's buffer is swept in place: no copy into the Vec
    let n_buf = heap.born_buf.len;
    heap.born_buf.len = 0;
    let buf_ptr = heap.born_buf.ptr;
    let from_buf = (0..n_buf).map(|i| Value::from_bits(unsafe { *buf_ptr.add(i) }));
    for v in born.into_iter().chain(from_buf) {
        let a = match v.kind() {
            Kind::Object(a) | Kind::Array(a) | Kind::Closure(a) | Kind::Cell(a) => a,
            _ => continue,
        };
        let mt = meta_at(a);
        if mt & M_MARK != 0 {
            set_meta(a, (mt & !M_MARK) | M_AGED);
            promoted += 1;
            promoted_bytes += size_words(mt) * 8;
        } else {
            heap.old.push_free(a, size_words(mt));
            freed += 1;
        }
    }
    let born_elems = std::mem::take(&mut heap.born_elems);
    for e in born_elems {
        let mt = meta_at(e);
        if mt & M_MARK != 0 {
            set_meta(e, (mt & !M_MARK) | M_AGED);
            promoted_bytes += size_words(mt) * 8;
        } else {
            heap.old.push_free(e, size_words(mt));
        }
    }
    // the born range: cells bump-allocated old since the last minor (the
    // JIT's old-space template and Rust's bump path). Promoted copies
    // landed here too and are aged already.
    sweep_born_range(heap, &mut promoted, &mut promoted_bytes, &mut freed);
    // strings: young log of the old arena
    {
        let young = std::mem::take(&mut heap.gen_strs.young);
        for r in young {
            let r = r as Ref;
            if m.ystrs.get(r) {
                heap.gen_strs.old.set(r);
                promoted += 1;
                promoted_bytes += 24 + heap.strs[r as usize].byte_len();
            } else {
                if !matches!(heap.strs[r as usize], tsr_memory::HStr::Buf(_)) {
                    heap.strs[r as usize] = tsr_memory::HStr::Buf(String::new());
                } else if let tsr_memory::HStr::Buf(b) = &mut heap.strs[r as usize] {
                    b.clear();
                    if b.capacity() > 1024 {
                        b.shrink_to(64);
                    }
                }
                heap.free_strs.push(r as u32);
                freed += 1;
            }
        }
    }
    {
        // foreigns: resume() frees consumed coroutine slots explicitly
        // while their young-log entry remains — skip those
        let young = std::mem::take(&mut heap.gen_foreigns.young);
        for r in young {
            let r = r as Ref;
            if matches!(heap.foreigns[r as usize], Foreign::Free) {
                continue;
            }
            if m.yforeigns.get(r) {
                heap.gen_foreigns.old.set(r);
                promoted += 1;
                promoted_bytes += 96;
            } else {
                if let Foreign::Promise(p) = &heap.foreigns[r as usize] {
                    if let PromiseState::Rejected(e) = &p.state {
                        if p.reactions.is_empty() && !e.cancelled {
                            eprintln!("warning: unhandled promise rejection: {}", e.msg);
                        }
                    }
                }
                heap.foreigns[r as usize] = Foreign::Free;
                heap.free_foreigns.push(r as u32);
                freed += 1;
            }
        }
    }
    // clear dirty bits for the remembered containers we consumed
    for v in remembered {
        match v.kind() {
            Kind::Object(a) | Kind::Array(a) | Kind::Cell(a) => {
                set_meta(a, meta_at(a) & !M_DIRTY);
            }
            Kind::Foreign(r) => heap.gen_foreigns.dirty.clear(r),
            _ => {}
        }
    }
    let t_sweep = t0.elapsed();

    // harvest string buffers from dead nursery slots, then truncate
    let mut evacuated = 0usize;
    for (i, hs) in nur.strs.iter_mut().enumerate() {
        if m.fwd_strs[i] != u32::MAX {
            evacuated += 1;
        } else if let tsr_memory::HStr::Buf(b) = hs {
            if b.capacity() > 0 && heap.pool_str_bufs.len() < tsr_memory::BUF_POOL_CAP {
                let mut b = std::mem::take(b);
                b.clear();
                if b.capacity() <= 1024 {
                    heap.pool_str_bufs.push(b);
                }
            }
        }
    }
    let evac_bytes = heap.promoted_bytes_since_major; // running total; delta below
    // Pretenure policy. Copying a nursery that mostly survives is pure
    // memcpy; allocating those cells old and marking them in place through
    // the born log measured cheaper. Flip on when >= 3/4 of a real nursery
    // survived (by bytes), back off when < 1/2 of the born log did —
    // decided here, with the nursery empty, so allocation never mixes
    // modes mid-cycle.
    heap.pretenure_skip = 0;
    if heap.nursery_on && !heap.pretenure_fixed {
        if heap.pretenure == 0 {
            let copied = evac_bytes.saturating_sub(m.evac_base);
            if young_used >= 256 << 10 && copied * 4 >= young_used * 3 {
                heap.pretenure = 1;
            }
        } else {
            let swept = promoted + freed;
            if swept >= 4096 && promoted * 2 < swept {
                heap.pretenure = 0;
                heap.pretenure_run = 0;
            } else if swept >= 4096 && promoted * 10 >= swept * 9 {
                // Nothing died. Grant twice as many untraced minors as
                // last time, so a generation that is entirely live stops
                // paying for the trace that keeps proving it.
                heap.pretenure_run = (heap.pretenure_run.saturating_mul(2) + 1).min(PRETENURE_RUN_MAX);
                heap.pretenure_skip = heap.pretenure_run;
            } else {
                heap.pretenure_run = 0;
            }
        }
    }
    nur.strs.clear();
    nur.bytes = 0;
    heap.nursery = nur;
    if std::env::var_os("TSC_GC_POISON").is_some() {
        heap.young.poison(young_top);
    }
    heap.young.reset();
    heap.old.end_minor();
    heap.promoted_since_major += promoted;
    heap.promoted_bytes_since_major += promoted_bytes;
    m.evac_base = heap.promoted_bytes_since_major;
    heap.allocs_since_gc = 0;
    heap.gc_scratch = m;
    stats.minor_collections += 1;
    stats.last_freed = freed;
    verify_no_stale_marks(heap, "post-full-minor");
    if verify_on() {
        let mut roots: Vec<Value> = stack.to_vec();
        roots.extend_from_slice(extra);
        verify_reachable(heap, &roots, "post-minor");
    }
    if debug_on() {
        eprintln!(
            "[gc] minor us={} roots={} scan={} sweep={} tail={} young_used={young_used} freed={freed} promoted={promoted} evac_strs={evacuated} old_bytes={}",
            t0.elapsed().as_micros(),
            t_roots.as_micros(),
            (t_scan - t_roots).as_micros(),
            (t_sweep - t_scan).as_micros(),
            (t0.elapsed() - t_sweep).as_micros(),
            heap.old.allocated_bytes()
        );
    }
    stats.record_pause(t0);
}

/// Pretenure-mode minor without a trace: every born cell ages as if it
/// had survived. Nothing young exists in this mode, so the remembered
/// set has no young edge to find; its dirty bits are just cleared.
fn bulk_promote(heap: &mut Heap, stats: &mut GcStats) {
    let t0 = std::time::Instant::now();
    let mut promoted = 0usize;
    let mut promoted_bytes = 0usize;
    for bits in heap.born_buf.drain().collect::<Vec<_>>() {
        heap.born_old.push(Value::from_bits(bits));
    }
    let born = std::mem::take(&mut heap.born_old);
    for v in &born {
        if let Kind::Object(a) | Kind::Array(a) | Kind::Closure(a) | Kind::Cell(a) = v.kind() {
            let mt = meta_at(a);
            // clear the mark: a minor's `forward_cell` sets it on a
            // not-yet-aged old cell, and ageing without clearing leaves
            // it set for good. The next major then reads the stale mark
            // as "already visited", never traces the cell, and sweeps
            // everything reachable only through it.
            set_meta(a, (mt & !M_MARK) | M_AGED);
            promoted += 1;
            promoted_bytes += size_words(mt) * 8;
        }
    }
    for e in std::mem::take(&mut heap.born_elems) {
        let mt = meta_at(e);
        // same as the cells above: ageing must clear the mark, or the
        // next major sees a chunk that is already "visited"
        set_meta(e, (mt & !M_MARK) | M_AGED);
        promoted_bytes += size_words(mt) * 8;
    }
    heap.old.sync_top();
    // no trace ran: every born cell ages, marked or not
    let n = heap.old.chunks.len();
    for ci in heap.old.born_chunk..n {
        let (base, top) = (heap.old.chunks[ci].base, heap.old.chunks[ci].top);
        let mut a = if ci == heap.old.born_chunk { heap.old.born_from.max(base) } else { base };
        while a < top {
            let mt = meta_at(a as Ref);
            if mt & M_AGED == 0 && kind_of(mt) != K_FREE {
                set_meta(a as Ref, (mt & !M_MARK) | M_AGED);
                promoted += 1;
                promoted_bytes += size_words(mt) * 8;
            }
            a += size_words(mt) * 8;
        }
    }
    let young = std::mem::take(&mut heap.gen_strs.young);
    for &r in &young {
        heap.gen_strs.old.set(r as Ref);
    }
    promoted += young.len();
    promoted_bytes += young.len() * 32;
    let young = std::mem::take(&mut heap.gen_foreigns.young);
    for &r in &young {
        let r = r as Ref;
        if !matches!(heap.foreigns[r as usize], Foreign::Free) {
            heap.gen_foreigns.old.set(r);
            promoted += 1;
            promoted_bytes += 96;
        }
    }
    for v in std::mem::take(&mut heap.remembered) {
        match v.kind() {
            Kind::Object(a) | Kind::Array(a) | Kind::Cell(a) => set_meta(a, meta_at(a) & !M_DIRTY),
            Kind::Foreign(r) => heap.gen_foreigns.dirty.clear(r),
            _ => {}
        }
    }
    heap.promoted_since_major += promoted;
    heap.promoted_bytes_since_major += promoted_bytes;
    heap.old.end_minor();
    heap.allocs_since_gc = 0;
    verify_no_stale_marks(heap, "post-bulk-promote");
    stats.minor_collections += 1;
    stats.last_freed = 0;
    if debug_on() {
        eprintln!("[gc] minor-bulk us={} promoted={promoted} old_bytes={}", t0.elapsed().as_micros(), heap.old.allocated_bytes());
    }
}

/// Walk the born range: unreached cells are freed, reached ones handed
/// to `age` (which returns their bytes). Returns the number freed.
fn for_born_range(
    heap: &mut Heap,
    mut age: impl FnMut(Ref, u64) -> usize,
    promoted: &mut usize,
    promoted_bytes: &mut usize,
) -> usize {
    let n = heap.old.chunks.len();
    let mut to_free: Vec<(Ref, usize)> = Vec::new();
    for ci in heap.old.born_chunk..n {
        let (base, top) = (heap.old.chunks[ci].base, heap.old.chunks[ci].top);
        let mut a = if ci == heap.old.born_chunk { heap.old.born_from.max(base) } else { base };
        while a < top {
            let mt = meta_at(a as Ref);
            let words = size_words(mt);
            debug_assert!(words >= 2, "corrupt cell at {a:#x}");
            if mt & M_AGED == 0 && kind_of(mt) != K_FREE {
                if mt & M_MARK != 0 {
                    *promoted += 1;
                    *promoted_bytes += age(a as Ref, mt);
                } else {
                    to_free.push((a as Ref, words));
                }
            }
            a += words * 8;
        }
    }
    let freed = to_free.len();
    for (a, words) in to_free {
        heap.old.push_free(a, words);
    }
    freed
}

fn sweep_born_range(heap: &mut Heap, promoted: &mut usize, promoted_bytes: &mut usize, freed: &mut usize) {
    *freed += for_born_range(heap, |a, mt| {
        set_meta(a, (mt & !M_MARK) | M_AGED);
        size_words(mt) * 8
    }, promoted, promoted_bytes);
}

/// `TSC_GC_VERIFY=1`: after a collection, walk everything reachable
/// from the roots and check that none of it is FREE. This is the exact
/// invariant the major's tracer asserts, checked at the collection that
/// broke it rather than at the one that trips over it later.
fn verify_reachable(heap: &Heap, roots: &[Value], phase: &str) {
    if !verify_on() {
        return;
    }
    let mut seen: std::collections::HashSet<Ref> = std::collections::HashSet::new();
    let mut work: Vec<Value> = roots.to_vec();
    let mut bad = 0usize;
    while let Some(v) = work.pop() {
        let a = match v.kind() {
            Kind::Object(a) | Kind::Array(a) | Kind::Closure(a) | Kind::Cell(a) => a,
            _ => continue,
        };
        if !seen.insert(a) {
            continue;
        }
        let mt = meta_at(a);
        if kind_of(mt) == K_FREE {
            eprintln!(
                "[verify/{phase}] reachable FREE cell {a:#x} size={} words, reached as {:?}",
                size_words(mt),
                v.kind()
            );
            bad += 1;
            if bad > 3 {
                std::process::abort();
            }
            continue;
        }
        if matches!(kind_of(mt), K_OBJ | K_ARR | K_ARR_NUM | K_ARR_I32 | K_CLOSURE | K_CELL) {
            trace_cell(heap, a, &mut work);
        }
    }
    if bad > 0 {
        std::process::abort();
    }
}

/// After the major's mark phase: everything reachable must carry
/// M_MARK, or the sweep frees it.
fn verify_marked(heap: &Heap, roots: &[Value]) {
    if !verify_on() {
        return;
    }
    let mut seen: std::collections::HashSet<Ref> = std::collections::HashSet::new();
    let mut work: Vec<(Ref, Value)> = roots.iter().map(|v| (0u64, *v)).collect();
    let mut bad = 0usize;
    while let Some((parent, v)) = work.pop() {
        let a = match v.kind() {
            Kind::Object(a) | Kind::Array(a) | Kind::Closure(a) | Kind::Cell(a) => a,
            _ => continue,
        };
        if !seen.insert(a) {
            continue;
        }
        let mt = meta_at(a);
        if mt & M_MARK == 0 {
            let pm = if parent == 0 { 0 } else { meta_at(parent) };
            eprintln!(
                "[verify/mark] UNMARKED {a:#x} kind={} size={} aged={} in_born={} \
                 <- parent {parent:#x} kind={} marked={} aged={} dirty={} in_born={}",
                kind_of(mt),
                size_words(mt),
                mt & M_AGED != 0,
                heap.old.in_born_range(a),
                if parent == 0 { 99 } else { kind_of(pm) },
                pm & M_MARK != 0,
                pm & M_AGED != 0,
                pm & M_DIRTY != 0,
                if parent == 0 { false } else { heap.old.in_born_range(parent) }
            );
            bad += 1;
            if bad > 3 {
                std::process::abort();
            }
        }
        if matches!(kind_of(mt), K_OBJ | K_ARR | K_ARR_NUM | K_ARR_I32 | K_CLOSURE | K_CELL) {
            let mut kids: Vec<Value> = Vec::new();
            trace_cell(heap, a, &mut kids);
            work.extend(kids.into_iter().map(|k| (a, k)));
        }
    }
    if bad > 0 {
        std::process::abort();
    }
}

/// Before the major marks anything, no old cell may already carry
/// M_MARK: a stale mark makes `mark_cell` report "already visited" and
/// the cell is never traced.
fn verify_no_stale_marks(heap: &Heap, phase: &str) {
    if !verify_on() {
        return;
    }
    let mut bad = 0usize;
    for ci in 0..heap.old.chunks.len() {
        let (base, top) = (heap.old.chunks[ci].base, heap.old.chunks[ci].top);
        let mut a = base;
        while a < top {
            let mt = meta_at(a as Ref);
            let words = size_words(mt);
            if words < 2 {
                break;
            }
            if mt & M_MARK != 0 && kind_of(mt) != K_ELEMS {
                eprintln!(
                    "[verify/{phase}] chunk{ci} base={base:#x} top={top:#x} born_chunk={} born_from={:#x} nchunks={} | STALE MARK {a:#x} kind={} size={} aged={} dirty={} in_born={}",
                    heap.old.born_chunk,
                    heap.old.born_from,
                    heap.old.chunks.len(),
                    kind_of(mt),
                    words,
                    mt & M_AGED != 0,
                    mt & M_DIRTY != 0,
                    heap.old.in_born_range(a as Ref)
                );
                bad += 1;
                if bad > 12 {
                    std::process::abort();
                }
            }
            a += words * 8;
        }
    }
    if bad > 0 {
        std::process::abort();
    }
}

/// A young cell should never carry a mark or a dirty bit; under
/// TSC_GC_VERIFY say so loudly if one ever does, since promotion used
/// to copy those bits straight into the old generation.
#[inline]
fn debug_young_flags(mt: u64, what: &str) {
    if verify_on() && mt & (M_MARK | M_DIRTY) != 0 {
        eprintln!(
            "[verify/promote] young {what} carried flags: mark={} dirty={}",
            mt & M_MARK != 0,
            mt & M_DIRTY != 0
        );
    }
}

fn verify_on() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| std::env::var_os("TSC_GC_VERIFY").is_some())
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

    fn minor(heap: &mut Heap, roots: &mut [Value]) -> GcStats {
        let mut stats = GcStats::default();
        collect_minor(heap, roots, [].iter_mut(), &[], &mut stats);
        stats
    }

    #[test]
    fn collects_garbage_keeps_live() {
        let mut heap = Heap::new();
        heap.nursery_on = false;
        let live_arr = heap.alloc_arr_lit(&[Value::number(7.0)]);
        for i in 0..1000 {
            heap.alloc_str(Arc::from(format!("garbage {i}").as_str()));
        }
        let live_str = heap.alloc_str(Arc::from("keep"));
        heap.arr_push(live_arr, Value::str_ref(live_str));

        let roots = [Value::array(live_arr)];
        let mut stats = GcStats::default();
        collect(&mut heap, &roots, [].iter(), &mut stats);

        assert_eq!(stats.last_freed, 1000);
        assert_eq!(stats.major_collections, 1);
        assert_eq!(heap.str_at(live_str), "keep");
        assert_eq!(heap.arr(live_arr)[0].as_number(), 7.0);
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
        let clo = heap.alloc_closure(&proto, &[Value::cell(cell)]);
        *heap.cell_mut(cell) = Value::closure(clo);

        let mut stats = GcStats::default();
        collect(&mut heap, &[], [].iter(), &mut stats);
        assert_eq!(stats.last_freed, 2);
    }

    /// Young object graph survives a minor with every edge rewritten,
    /// including the spill chunk and a nested array.
    #[test]
    fn minor_copies_reachable_graph() {
        let mut heap = Heap::new();
        let shape = tsr_memory::empty_shape();
        let mut sh = shape;
        for k in ["a", "b", "c", "d", "e", "f"] {
            sh = sh.with_field(Arc::from(k));
        }
        let inner = heap.alloc_arr_lit(&[Value::number(1.0), Value::number(2.0)]);
        let vals: Vec<Value> = (0..6).map(|i| if i == 5 { Value::array(inner) } else { Value::number(i as f64) }).collect();
        let o = heap.alloc_obj_lit(sh, &vals);
        assert!(heap.young(o));
        let mut roots = [Value::object(o), Value::UNDEFINED];
        let _ = heap.alloc_obj_empty(); // garbage
        minor(&mut heap, &mut roots);
        let no = roots[0].as_object().unwrap();
        assert!(!heap.young(no));
        assert_eq!(heap.obj(no).val(4).as_number(), 4.0);
        let na = heap.obj(no).val(5).as_array().unwrap();
        assert!(!heap.young(na));
        assert_eq!(heap.arr(na), &[Value::number(1.0), Value::number(2.0)]);
        assert_eq!(heap.young.used(), 0);
    }

    /// An aged container that gains a young edge is remembered; the edge
    /// is forwarded by the next minor and the container stays valid.
    #[test]
    fn barrier_keeps_old_to_young_edge() {
        let mut heap = Heap::new();
        let arr = heap.alloc_arr_empty(4);
        let mut roots = [Value::array(arr)];
        minor(&mut heap, &mut roots);
        let arr = roots[0].as_array().unwrap();
        let young = heap.alloc_obj_empty();
        heap.barrier_arr(arr);
        heap.arr_push(arr, Value::object(young));
        assert_eq!(heap.remembered.len(), 1);
        minor(&mut heap, &mut roots);
        let o = heap.arr(arr)[0].as_object().unwrap();
        assert!(!heap.young(o));
        assert_eq!(heap.obj(o).vlen(), 0);
    }

    /// Growth across a minor: the array's chunk is replaced, then copied.
    #[test]
    fn push_grows_then_survives() {
        let mut heap = Heap::new();
        let arr = heap.alloc_arr_empty(1);
        for i in 0..100 {
            heap.arr_push(arr, Value::number(i as f64));
        }
        let mut roots = [Value::array(arr)];
        minor(&mut heap, &mut roots);
        let arr = roots[0].as_array().unwrap();
        assert_eq!(heap.arr_len(arr), 100);
        assert_eq!(heap.arr(arr)[99].as_number(), 99.0);
        heap.arr_push(arr, Value::number(100.0)); // old array grows old
        assert_eq!(heap.arr(arr)[100].as_number(), 100.0);
    }

    /// Born-old cells (closures, cells, pretenured objects) that nothing
    /// reaches are freed by the minor; reached ones age.
    #[test]
    fn born_old_swept_by_minor() {
        let mut heap = Heap::new();
        let proto = Arc::new(tsc_ir::FunctionProto::default());
        let live = heap.alloc_closure(&proto, &[Value::number(1.0)]);
        let _dead = heap.alloc_closure(&proto, &[Value::number(2.0)]);
        let mut roots = [Value::closure(live)];
        let stats = minor(&mut heap, &mut roots);
        assert_eq!(stats.last_freed, 1);
        assert_eq!(heap.closure(live).upvals()[0].as_number(), 1.0);
        assert!(meta_at(live) & M_AGED != 0);
        // a closure capturing a young object keeps it alive across the minor
        let young = heap.alloc_obj_empty();
        let c = heap.alloc_closure(&proto, &[Value::object(young)]);
        let mut roots = [Value::closure(c)];
        minor(&mut heap, &mut roots);
        let o = heap.closure(c).upvals()[0].as_object().unwrap();
        assert!(!heap.young(o));
    }

    /// Pretenured allocation lands old and is measured exactly by the
    /// major sweep.
    #[test]
    fn pretenure_and_live_bytes() {
        let mut heap = Heap::new();
        heap.pretenure = 1;
        heap.pretenure_fixed = true;
        let o = heap.alloc_obj_empty();
        assert!(!heap.young(o));
        let mut roots = [Value::object(o)];
        minor(&mut heap, &mut roots);
        let mut stats = GcStats::default();
        collect(&mut heap, &roots, [].iter(), &mut stats);
        assert_eq!(stats.last_live_bytes, OBJ_WORDS * 8);
        assert_eq!(stats.last_live, 1);
    }
}
