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

struct Marks {
    strs: Vec<bool>,
    objs: Vec<bool>,
    arrs: Vec<bool>,
    closures: Vec<bool>,
    cells: Vec<bool>,
    foreigns: Vec<bool>,
}

/// Collect the heap given its roots: the register stack and the globals.
pub fn collect<'a>(
    heap: &mut Heap,
    stack: &[Value],
    globals: impl Iterator<Item = &'a Value>,
    stats: &mut GcStats,
) {
    let t0 = std::time::Instant::now();
    let mut m = Marks {
        strs: vec![false; heap.strs.len()],
        objs: vec![false; heap.objs.len()],
        arrs: vec![false; heap.arrs.len()],
        closures: vec![false; heap.closures.len()],
        cells: vec![false; heap.cells.len()],
        foreigns: vec![false; heap.foreigns.len()],
    };
    let mut work: Vec<Value> = Vec::with_capacity(stack.len() + 16);
    work.extend_from_slice(stack);
    work.extend(globals.copied());

    while let Some(v) = work.pop() {
        match v.kind() {
            Kind::Str(r) => {
                m.strs[r as usize] = true;
            }
            Kind::Object(r) => {
                if !mark(&mut m.objs, r) {
                    work.extend_from_slice(&heap.objs[r as usize].values);
                }
            }
            Kind::Array(r) => {
                if !mark(&mut m.arrs, r) {
                    work.extend_from_slice(&heap.arrs[r as usize]);
                }
            }
            Kind::Closure(r) => {
                if !mark(&mut m.closures, r) {
                    work.extend(
                        heap.closures[r as usize].upvals.iter().map(|&c| Value::cell(c)),
                    );
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

    let mut freed = 0usize;
    freed += sweep(&m.strs, &mut heap.free_strs, |r| {
        heap.strs[r as usize] = std::sync::Arc::from("");
    });
    freed += sweep(&m.objs, &mut heap.free_objs, |r| {
        heap.objs[r as usize] = Default::default();
    });
    freed += sweep(&m.arrs, &mut heap.free_arrs, |r| {
        heap.arrs[r as usize] = Vec::new();
    });
    freed += sweep(&m.closures, &mut heap.free_closures, |r| {
        heap.closures[r as usize].upvals = Vec::new();
    });
    freed += sweep(&m.cells, &mut heap.free_cells, |r| {
        heap.cells[r as usize] = Value::UNDEFINED;
    });
    freed += sweep(&m.foreigns, &mut heap.free_foreigns, |r| {
        // unobserved rejection dies here: surface it before dropping
        if let Foreign::Promise(p) = &heap.foreigns[r as usize] {
            if let PromiseState::Rejected(e) = &p.state {
                if p.reactions.is_empty() && !e.cancelled {
                    eprintln!("warning: unhandled promise rejection: {}", e.msg);
                }
            }
        }
        heap.foreigns[r as usize] = Foreign::Free;
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
    stats.last_live_bytes = approx_live_bytes(heap, &m);

    // sticky-generational major reset: everything live becomes old
    set_old_from_marks(&mut heap.gen_strs, &m.strs);
    set_old_from_marks(&mut heap.gen_objs, &m.objs);
    set_old_from_marks(&mut heap.gen_arrs, &m.arrs);
    set_old_from_marks(&mut heap.gen_closures, &m.closures);
    set_old_from_marks(&mut heap.gen_cells, &m.cells);
    set_old_from_marks(&mut heap.gen_foreigns, &m.foreigns);
    heap.remembered.clear();
    heap.promoted_since_major = 0;
    heap.promoted_bytes_since_major = 0;

    heap.allocs_since_gc = 0;
    stats.record_pause(t0);
}

/// Push v's outgoing edges onto the worklist (shared by minor tracing).
fn push_children(heap: &Heap, v: Value, work: &mut Vec<Value>) {
    match v.kind() {
        Kind::Object(r) => work.extend_from_slice(&heap.objs[r as usize].values),
        Kind::Array(r) => work.extend_from_slice(&heap.arrs[r as usize]),
        Kind::Closure(r) => {
            work.extend(heap.closures[r as usize].upvals.iter().map(|&c| Value::cell(c)))
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
    use tsr_memory::Bitmap;
    let t0 = std::time::Instant::now();

    struct YoungMarks {
        strs: Bitmap,
        objs: Bitmap,
        arrs: Bitmap,
        closures: Bitmap,
        cells: Bitmap,
        foreigns: Bitmap,
    }
    let mut m = YoungMarks {
        strs: Bitmap::default(),
        objs: Bitmap::default(),
        arrs: Bitmap::default(),
        closures: Bitmap::default(),
        cells: Bitmap::default(),
        foreigns: Bitmap::default(),
    };

    let mut work: Vec<Value> = Vec::with_capacity(stack.len() + 64);
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
            Kind::Str(r) => (&heap.gen_strs, &mut m.strs, r),
            Kind::Object(r) => (&heap.gen_objs, &mut m.objs, r),
            Kind::Array(r) => (&heap.gen_arrs, &mut m.arrs, r),
            Kind::Closure(r) => (&heap.gen_closures, &mut m.closures, r),
            Kind::Cell(r) => (&heap.gen_cells, &mut m.cells, r),
            Kind::Foreign(r) => (&heap.gen_foreigns, &mut m.foreigns, r),
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
    sweep_young!(gen_strs, m.strs, free_strs, |h: &mut Heap, r| {
        h.strs[r as usize] = std::sync::Arc::from("");
    });
    sweep_young!(gen_objs, m.objs, free_objs, |h: &mut Heap, r| {
        h.objs[r as usize] = Default::default();
    });
    sweep_young!(gen_arrs, m.arrs, free_arrs, |h: &mut Heap, r| {
        h.arrs[r as usize] = Vec::new();
    });
    sweep_young!(gen_closures, m.closures, free_closures, |h: &mut Heap, r| {
        h.closures[r as usize].upvals = Vec::new();
    });
    sweep_young!(gen_cells, m.cells, free_cells, |h: &mut Heap, r| {
        h.cells[r as usize] = Value::UNDEFINED;
    });
    sweep_young!(gen_foreigns, m.foreigns, free_foreigns, |h: &mut Heap, r| {
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
    stats.minor_collections += 1;
    stats.last_freed = freed;
    stats.record_pause(t0);
}

fn slot_bytes(heap: &Heap, arena: &str, r: tsr_memory::Ref) -> usize {
    let i = r as usize;
    match arena {
        "gen_strs" => 24 + heap.strs[i].len(),
        "gen_objs" => 32 + heap.objs[i].values.len() * 8,
        "gen_arrs" => 32 + heap.arrs[i].capacity() * 8,
        "gen_closures" => 32 + heap.closures[i].upvals.len() * 4,
        "gen_cells" => 8,
        _ => 96,
    }
}

fn stats_remembered_peak(heap: &mut Heap, n: usize) {
    heap.remembered_peak = heap.remembered_peak.max(n);
}

fn set_old_from_marks(gen: &mut tsr_memory::GenState, marks: &[bool]) {
    gen.old.clear_all();
    gen.dirty.clear_all();
    for (i, &alive) in marks.iter().enumerate() {
        if alive {
            gen.old.set(i as tsr_memory::Ref);
        }
    }
    gen.young.clear();
}

/// Rough live-byte estimate, computed while everything is already paged
/// in from the mark phase. Documented approximation.
fn approx_live_bytes(heap: &Heap, m: &Marks) -> usize {
    let mut bytes = 0usize;
    for (i, alive) in m.strs.iter().enumerate() {
        if *alive {
            bytes += 24 + heap.strs[i].len();
        }
    }
    for (i, alive) in m.objs.iter().enumerate() {
        if *alive {
            bytes += 32 + heap.objs[i].values.len() * 8;
        }
    }
    for (i, alive) in m.arrs.iter().enumerate() {
        if *alive {
            bytes += 32 + heap.arrs[i].capacity() * 8;
        }
    }
    for (i, alive) in m.closures.iter().enumerate() {
        if *alive {
            bytes += 32 + heap.closures[i].upvals.len() * 4;
        }
    }
    bytes += m.cells.iter().filter(|a| **a).count() * 8;
    bytes += m.foreigns.iter().filter(|a| **a).count() * 96;
    bytes
}

fn mark(bits: &mut [bool], r: Ref) -> bool {
    let was = bits[r as usize];
    bits[r as usize] = true;
    was
}

/// Rebuild one arena's free list from its mark bits; clears dead slots so
/// dropped payloads (strings, arrays) release memory now.
fn sweep(marks: &[bool], free: &mut Vec<Ref>, mut clear: impl FnMut(Ref)) -> usize {
    free.clear();
    let mut freed = 0;
    for (i, &alive) in marks.iter().enumerate() {
        if !alive {
            clear(i as Ref);
            free.push(i as Ref);
            freed += 1;
        }
    }
    freed
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
        assert_eq!(&**heap.str_at(live_str), "keep");
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
        let proto = Arc::new(tsc_ir::FunctionProto {
            name: Arc::from("f"),
            arity: 0,
            is_async: false,
            n_regs: 1,
            code: vec![],
            consts: vec![],
            upvals: vec![],
            protos: vec![],
            spans: vec![],
            arg_types: vec![],
            jit: Default::default(),
        });
        let clo = heap.alloc_closure(tsr_memory::Closure { proto, upvals: vec![cell] });
        *heap.cell_mut(cell) = Value::closure(clo);

        let mut stats = GcStats::default();
        collect(&mut heap, &[], [].iter(), &mut stats);
        assert_eq!(stats.last_freed, 2);
    }
}
