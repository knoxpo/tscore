//! tsr-gc
//!
//! Stop-the-realm mark-sweep for the main realm. Non-moving: the sweep
//! rebuilds per-arena free lists, allocation reuses freed slots. Scratch
//! realms (parallel chunks) never collect — they drop wholesale.
//! Per-realm heaps mean this never synchronizes across threads.

use tsr_memory::{Heap, Ref, Value};

#[derive(Debug, Default, Clone, Copy)]
pub struct GcStats {
    pub collections: u64,
    pub last_freed: usize,
    pub last_live: usize,
}

struct Marks {
    strs: Vec<bool>,
    objs: Vec<bool>,
    arrs: Vec<bool>,
    closures: Vec<bool>,
    cells: Vec<bool>,
}

/// Collect the heap given its roots: the register stack and the globals.
pub fn collect<'a>(
    heap: &mut Heap,
    stack: &[Value],
    globals: impl Iterator<Item = &'a Value>,
    stats: &mut GcStats,
) {
    let mut m = Marks {
        strs: vec![false; heap.strs.len()],
        objs: vec![false; heap.objs.len()],
        arrs: vec![false; heap.arrs.len()],
        closures: vec![false; heap.closures.len()],
        cells: vec![false; heap.cells.len()],
    };
    let mut work: Vec<Value> = Vec::with_capacity(stack.len() + 16);
    work.extend_from_slice(stack);
    work.extend(globals.copied());

    while let Some(v) = work.pop() {
        match v {
            Value::Str(r) => {
                m.strs[r as usize] = true;
            }
            Value::Object(r) => {
                if !mark(&mut m.objs, r) {
                    work.extend(heap.objs[r as usize].fields.iter().map(|(_, v)| *v));
                }
            }
            Value::Array(r) => {
                if !mark(&mut m.arrs, r) {
                    work.extend_from_slice(&heap.arrs[r as usize]);
                }
            }
            Value::Closure(r) => {
                if !mark(&mut m.closures, r) {
                    work.extend(
                        heap.closures[r as usize].upvals.iter().map(|&c| Value::Cell(c)),
                    );
                }
            }
            Value::Cell(r) => {
                if !mark(&mut m.cells, r) {
                    work.push(heap.cells[r as usize]);
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
        heap.cells[r as usize] = Value::Undefined;
    });

    let total = heap.strs.len()
        + heap.objs.len()
        + heap.arrs.len()
        + heap.closures.len()
        + heap.cells.len();
    stats.collections += 1;
    stats.last_freed = freed;
    stats.last_live = total - freed;
    heap.allocs_since_gc = 0;
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
        let live_arr = heap.alloc_arr(vec![Value::Number(7.0)]);
        for i in 0..1000 {
            heap.alloc_str(Arc::from(format!("garbage {i}").as_str()));
        }
        let live_str = heap.alloc_str(Arc::from("keep"));
        heap.arr_mut(live_arr).push(Value::Str(live_str));

        let roots = [Value::Array(live_arr)];
        let mut stats = GcStats::default();
        collect(&mut heap, &roots, [].iter(), &mut stats);

        assert_eq!(stats.last_freed, 1000);
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
        let cell = heap.alloc_cell(Value::Undefined);
        let proto = Arc::new(tsc_ir::FunctionProto {
            name: Arc::from("f"),
            arity: 0,
            n_regs: 1,
            code: vec![],
            consts: vec![],
            upvals: vec![],
            protos: vec![],
            spans: vec![],
        });
        let clo = heap.alloc_closure(tsr_memory::Closure { proto, upvals: vec![cell] });
        *heap.cell_mut(cell) = Value::Closure(clo);

        let mut stats = GcStats::default();
        collect(&mut heap, &[], [].iter(), &mut stats);
        assert_eq!(stats.last_freed, 2);
    }
}
