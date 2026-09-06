//! Runtime layout discovery for JIT-inlined heap reads. Instead of
//! constraining Rust struct layout with repr(C), we probe the real
//! offsets at startup with sentinel values and bake them into templates.
//! Any probe failure disables the inline paths (helpers still work), so
//! a compiler/std layout change degrades performance, never correctness.

use crate::Realm;
use std::sync::Arc;
use tsr_memory::{Obj, Value};

/// Byte offsets discovered at startup. All reads native code performs:
///   objs data ptr   = *(realm + realm_objs_ptr)
///   obj             = objs_ptr + ref * obj_size
///   shape data      = *(obj + obj_shape_arc) + shape_id_delta -> u32 id
///   values ptr/len  = *(obj + obj_vals_ptr) / *(obj + obj_vals_len)
///   arrs data ptr   = *(realm + realm_arrs_ptr)
///   arr             = arrs_ptr + ref * arr_size; ptr/len via vec offsets
#[derive(Clone, Copy, Debug)]
pub struct HeapLayout {
    pub realm_stack_ptr: u32,
    pub realm_objs_ptr: u32,
    pub realm_arrs_ptr: u32,
    /// Nursery arena data pointers (young generation).
    pub nursery_objs_ptr: u32,
    pub nursery_arrs_ptr: u32,
    /// Offsets of the [old, young] base-pair tables inside Realm.
    pub obj_bases_off: u32,
    pub arr_bases_off: u32,
    /// nursery.objs Vec length / capacity word offsets inside Realm, and
    /// nursery.bytes — for the JIT's inline bump-allocation fast path.
    pub nursery_objs_len: u32,
    pub nursery_objs_cap: u32,
    pub nursery_bytes_off: u32,
    /// nursery.arrs len/cap and the array-buffer pool's Vec words, for the
    /// JIT's inline array-literal path (pop a pooled buffer, bump a slot).
    pub nursery_arrs_len: u32,
    pub nursery_arrs_cap: u32,
    pub pool_arr_ptr: u32,
    pub pool_arr_len: u32,
    pub pool_arr_cap: u32,
    /// heap.pretenure word: nonzero sends the inline literal templates to
    /// the helper (old-space allocation).
    pub pretenure_off: u32,
    /// Old-space allocation (pretenure mode): arena len/cap, free-list
    /// lengths, young-log Vec words, and the allocation counter.
    pub objs_len: u32,
    pub objs_cap: u32,
    pub arrs_len: u32,
    pub arrs_cap: u32,
    pub free_objs_len: u32,
    pub free_arrs_len: u32,
    pub objs_young_ptr: u32,
    pub objs_young_len: u32,
    pub objs_young_cap: u32,
    pub arrs_young_ptr: u32,
    pub arrs_young_len: u32,
    pub arrs_young_cap: u32,
    pub allocs_since_gc_off: u32,
    /// gen_arrs old / dirty bitmap words: an in-bounds SetIndex on an
    /// old array stores inline when the array is already dirty.
    pub arrs_old_ptr: u32,
    pub arrs_old_len: u32,
    pub arrs_dirty_ptr: u32,
    pub arrs_dirty_len: u32,
    /// gen_objs old / dirty bitmap words: a ref-valued SetField on an
    /// old object stores inline when the object is already dirty.
    pub objs_old_ptr: u32,
    pub objs_old_len: u32,
    pub objs_dirty_ptr: u32,
    pub objs_dirty_len: u32,
    /// The three raw words of an empty Vec<Value> (written into a freshly
    /// bumped Obj's overflow field).
    pub empty_vec_words: [u64; 3],
    /// Obj field offsets (repr(C)): vlen and overflow.
    pub obj_vlen: u32,
    pub obj_overflow: u32,
    pub obj_size: u32,
    pub obj_shape_arc: u32,
    pub shape_id_delta: u32,
    /// Offset of the inline value slots inside Obj (repr(C): exact).
    pub obj_inline: u32,
    pub realm_closures_ptr: u32,
    pub realm_cells_ptr: u32,
    pub closure_size: u32,
    /// Offset of the upvals Vec's data pointer inside Closure.
    pub closure_upvals_ptr: u32,
    /// Offset of the stored proto Arc word inside Closure.
    pub closure_proto_off: u32,
    /// Offset of realm.stack's len inside Realm.
    pub realm_stack_len: u32,
    pub arr_size: u32,
    pub vec_ptr: u32,
    pub vec_len: u32,
    /// Capacity word inside a Vec, derived from the other two.
    pub vec_cap: u32,
}

fn find_word(hay: &[u64], needle: u64) -> Option<u32> {
    let hits: Vec<usize> = hay
        .iter()
        .enumerate()
        .filter(|(_, &w)| w == needle)
        .map(|(i, _)| i)
        .collect();
    if hits.len() == 1 {
        Some((hits[0] * 8) as u32)
    } else {
        if hits.len() > 1 && std::env::var_os("TSC_LAYOUT_DEBUG").is_some() {
            eprintln!(
                "[layout] ambiguous needle {needle:#x}: byte offsets {:?}",
                hits.iter().map(|&i| i * 8).collect::<Vec<_>>()
            );
        }
        None // ambiguous or missing: refuse to guess
    }
}

fn as_words<T>(v: &T) -> &[u64] {
    unsafe {
        std::slice::from_raw_parts(v as *const T as *const u64, std::mem::size_of::<T>() / 8)
    }
}

pub fn discover() -> Option<HeapLayout> {
    let dbg = std::env::var_os("TSC_LAYOUT_DEBUG").is_some();
    macro_rules! probe {
        ($name:literal, $e:expr) => {
            match $e {
                Some(v) => v,
                None => {
                    if dbg {
                        eprintln!("[layout] probe failed: {}", $name);
                    }
                    return None;
                }
            }
        };
    }
    // --- Vec<Value> internals (ptr, len) ---
    let mut vv: Vec<Value> = Vec::with_capacity(13);
    for i in 0..5 {
        vv.push(Value::number(i as f64));
    }
    let vec_ptr = probe!("vec_ptr", find_word(as_words(&vv), vv.as_ptr() as u64));
    let vec_len = probe!("vec_len", find_word(as_words(&vv), 5));

    // --- Obj internals ---
    // burn shape ids so the probe shape's id can never equal its field
    // count (a value collision would make the id-offset scan ambiguous)
    {
        let mut burner = Obj::default();
        for i in 0..16 {
            burner.set(Arc::from(format!("burn{i}").as_str()), Value::number(0.0));
        }
    }
    let mut o = Obj::default();
    for i in 0..3 {
        o.set(Arc::from(format!("probe{i}").as_str()), Value::number(i as f64));
    }
    debug_assert!(o.shape.id() as usize != o.shape.fields.len());
    let obj_words = as_words(&o);
    let shape_ref: &tsr_memory::ShapeData = &o.shape;
    // the stored word is the ArcInner pointer; &*arc points at the data
    let obj_shape_arc = {
        let hits: Vec<usize> = obj_words
            .iter()
            .enumerate()
            .filter(|(_, &w)| {
                let delta = (shape_ref as *const _ as u64).wrapping_sub(w);
                w != 0 && delta <= 64 && delta % 8 == 0
            })
            .map(|(i, _)| i)
            .collect();
        if hits.len() != 1 {
            return None;
        }
        (hits[0] * 8) as u32
    };
    let stored = obj_words[(obj_shape_arc / 8) as usize];
    let shape_data_base_delta = (shape_ref as *const _ as u64 - stored) as u32;
    // id offset inside ShapeData: probe by value
    let sd_words = as_words(shape_ref);
    let id_hits: Vec<(usize, u64)> = sd_words
        .iter()
        .enumerate()
        .filter(|(_, &w)| (w as u32) == shape_ref.id() || (w >> 32) as u32 == shape_ref.id())
        .map(|(i, &w)| (i, w))
        .collect();
    if id_hits.len() != 1 {
        if dbg {
            eprintln!("[layout] probe failed: shape_id ambiguous ({} hits)", id_hits.len());
        }
        return None;
    }
    let id_word = id_hits[0];
    let id_off_in_sd = if (id_word.1 & 0xFFFF_FFFF) as u32 == shape_ref.id() {
        (id_word.0 * 8) as u32
    } else {
        (id_word.0 * 8 + 4) as u32
    };
    let _ = &id_hits;
    let shape_id_delta = shape_data_base_delta + id_off_in_sd;
    // Obj is repr(C): inline slots at a compile-time offset
    let obj_inline = std::mem::offset_of!(Obj, inline) as u32;

    // --- Closure internals: upvals Vec data pointer ---
    let cl = tsr_memory::Closure {
        proto: {
            // a dummy proto: only the struct layout matters here
            std::sync::Arc::new(tsc_ir::FunctionProto::default())
        },
        upvals: {
            let mut u = Vec::with_capacity(7);
            u.push(Value::number(9.0));
            u
        },
    };
    let closure_upvals_ptr = probe!(
        "closure_upvals_ptr",
        find_word(as_words(&cl), cl.upvals.as_ptr() as u64)
    );
    // the stored word is the ArcInner pointer; data sits a small header in
    let closure_proto_off = {
        let data = std::sync::Arc::as_ptr(&cl.proto) as u64;
        let words = as_words(&cl);
        let hits: Vec<usize> = words
            .iter()
            .enumerate()
            .filter(|(_, &w)| {
                let delta = data.wrapping_sub(w);
                w != 0 && delta <= 64 && delta % 8 == 0
            })
            .map(|(i, _)| i)
            .collect();
        if hits.len() != 1 {
            if dbg {
                eprintln!("[layout] probe failed: closure_proto_off");
            }
            return None;
        }
        (hits[0] * 8) as u32
    };

    // --- Realm -> heap.objs / heap.arrs data pointers ---
    let mut realm = Realm::new();
    let dummy_proto = std::sync::Arc::new(tsc_ir::FunctionProto::default());
    for i in 0..4 {
        let mut po = Obj::default();
        po.set(Arc::from("k"), Value::number(i as f64));
        realm.heap.alloc_obj(po);
        realm.heap.alloc_arr(vec![Value::number(i as f64); 3]);
        // populate closures/cells so their arena data pointers are real,
        // unique words inside Realm (an empty Vec's dangling ptr is not)
        realm.heap.alloc_closure(tsr_memory::Closure {
            proto: dummy_proto.clone(),
            upvals: vec![Value::number(i as f64)],
        });
        realm.heap.alloc_cell(Value::number(i as f64));
    }
    realm.stack.resize(37, Value::UNDEFINED);
    // populate the nursery so its Vec data pointers are real (and unique)
    realm.heap.nursery.objs.push(Obj::default());
    realm.heap.nursery.arrs.push(vec![Value::UNDEFINED]);
    // sentinel the base-pair tables: their live values equal the Vec data
    // pointers (ambiguous by construction), so probe unique markers
    realm.heap.obj_bases = [0xB0BA_0001_0001, 0xB0BA_0001_0002];
    realm.heap.arr_bases = [0xB0BA_0002_0001, 0xB0BA_0002_0002];
    // Ambiguity happens when a freed block's address survives as a stale
    // word somewhere in Realm and the target Vec reallocates into that
    // block. Growing the target moves its pointer; the true field updates,
    // the stale copy cannot — so grow-and-retry disambiguates.
    macro_rules! probe_vec_ptr {
        ($name:literal, $vec:expr, $grow:expr) => {{
            let mut found = None;
            for _ in 0..4 {
                if let Some(off) = find_word(as_words(&realm), $vec(&realm) as u64) {
                    found = Some(off);
                    break;
                }
                $grow(&mut realm);
            }
            probe!($name, found)
        }};
    }
    let realm_stack_len =
        probe!("realm_stack_len", find_word(as_words(&realm), 37));
    let realm_stack_ptr = probe_vec_ptr!(
        "realm_stack_ptr",
        |r: &Realm| r.stack.as_ptr(),
        |r: &mut Realm| {
            let n = r.stack.len();
            r.stack.reserve(r.stack.capacity() + 8);
            r.stack.resize(n, Value::UNDEFINED);
        }
    );
    let realm_objs_ptr = probe_vec_ptr!(
        "realm_objs_ptr",
        |r: &Realm| r.heap.objs.as_ptr(),
        |r: &mut Realm| {
            let mut po = Obj::default();
            po.set(Arc::from("g"), Value::number(1.0));
            r.heap.alloc_obj(po);
        }
    );
    let realm_arrs_ptr = probe_vec_ptr!(
        "realm_arrs_ptr",
        |r: &Realm| r.heap.arrs.as_ptr(),
        |r: &mut Realm| {
            r.heap.alloc_arr(vec![Value::number(1.0); 3]);
        }
    );
    let realm_closures_ptr = probe_vec_ptr!(
        "realm_closures_ptr",
        |r: &Realm| r.heap.closures.as_ptr(),
        |r: &mut Realm| {
            r.heap.alloc_closure(tsr_memory::Closure {
                proto: dummy_proto.clone(),
                upvals: vec![Value::number(1.0)],
            });
        }
    );
    let realm_cells_ptr = probe_vec_ptr!(
        "realm_cells_ptr",
        |r: &Realm| r.heap.cells.as_ptr(),
        |r: &mut Realm| {
            r.heap.alloc_cell(Value::number(1.0));
        }
    );
    // growing probes above ran refresh_bases(); re-sentinel both tables so
    // the nursery/base probes below see unique markers, not live pointers
    realm.heap.obj_bases = [0xB0BA_0001_0001, 0xB0BA_0001_0002];
    realm.heap.arr_bases = [0xB0BA_0002_0001, 0xB0BA_0002_0002];
    let realm_words = as_words(&realm);
    let nursery_objs_ptr = probe!(
        "nursery_objs_ptr",
        find_word(realm_words, realm.heap.nursery.objs.as_ptr() as u64)
    );
    let nursery_arrs_ptr = probe!(
        "nursery_arrs_ptr",
        find_word(realm_words, realm.heap.nursery.arrs.as_ptr() as u64)
    );
    let obj_bases_off = probe!("obj_bases", find_word(realm_words, 0xB0BA_0001_0001));
    let arr_bases_off = probe!("arr_bases", find_word(realm_words, 0xB0BA_0002_0001));
    realm.heap.refresh_bases();
    // nursery.objs Vec internals: the Vec struct base is where its data
    // pointer sits minus the (already probed) in-Vec ptr offset; len/cap
    // words derive from the generic Vec layout (0+8+16 word triple)
    let vec_cap_off = 24 - vec_ptr - vec_len;
    let nursery_vec_base = nursery_objs_ptr - vec_ptr;
    let nursery_objs_len = nursery_vec_base + vec_len;
    let nursery_objs_cap = nursery_vec_base + vec_cap_off;
    let nursery_arrs_base = nursery_arrs_ptr - vec_ptr;
    let nursery_arrs_len = nursery_arrs_base + vec_len;
    let nursery_arrs_cap = nursery_arrs_base + vec_cap_off;
    // the pool starts empty — a dangling data pointer would false-match —
    // so give it storage first
    realm.heap.pool_arr_bufs.reserve(16);
    let pool_arr_ptr = probe_vec_ptr!(
        "pool_arr_ptr",
        |r: &Realm| r.heap.pool_arr_bufs.as_ptr(),
        |r: &mut Realm| {
            let c = r.heap.pool_arr_bufs.capacity();
            r.heap.pool_arr_bufs.reserve(c + 8);
        }
    );
    let pool_arr_len = pool_arr_ptr - vec_ptr + vec_len;
    let pool_arr_cap = pool_arr_ptr - vec_ptr + vec_cap_off;
    // nursery.bytes: sentinel probe
    realm.heap.nursery.bytes = 0xB0BA_0003_0001;
    let nursery_bytes_off = probe!(
        "nursery_bytes",
        find_word(as_words(&realm), 0xB0BA_0003_0001)
    );
    realm.heap.nursery.bytes = 0;
    let saved = realm.heap.pretenure;
    realm.heap.pretenure = 0xB0BA_0004_0001;
    let pretenure_off = probe!(
        "pretenure",
        find_word(as_words(&realm), 0xB0BA_0004_0001)
    );
    realm.heap.pretenure = saved;
    let objs_len = realm_objs_ptr - vec_ptr + vec_len;
    let objs_cap = realm_objs_ptr - vec_ptr + vec_cap_off;
    let arrs_len = realm_arrs_ptr - vec_ptr + vec_len;
    let arrs_cap = realm_arrs_ptr - vec_ptr + vec_cap_off;
    // vectors that start empty get storage first (dangling ptr false-match)
    realm.heap.free_objs.reserve(16);
    realm.heap.free_arrs.reserve(16);
    realm.heap.gen_objs.young.reserve(16);
    realm.heap.gen_arrs.young.reserve(16);
    macro_rules! vec_words {
        ($name:literal, $get:expr, $grow:expr) => {{
            let p = probe_vec_ptr!($name, $get, $grow);
            (p, p - vec_ptr + vec_len, p - vec_ptr + vec_cap_off)
        }};
    }
    let (_, free_objs_len, _) = vec_words!(
        "free_objs",
        |r: &Realm| r.heap.free_objs.as_ptr(),
        |r: &mut Realm| { let c = r.heap.free_objs.capacity(); r.heap.free_objs.reserve(c + 8); }
    );
    let (_, free_arrs_len, _) = vec_words!(
        "free_arrs",
        |r: &Realm| r.heap.free_arrs.as_ptr(),
        |r: &mut Realm| { let c = r.heap.free_arrs.capacity(); r.heap.free_arrs.reserve(c + 8); }
    );
    let (objs_young_ptr, objs_young_len, objs_young_cap) = vec_words!(
        "objs_young",
        |r: &Realm| r.heap.gen_objs.young.as_ptr(),
        |r: &mut Realm| { let c = r.heap.gen_objs.young.capacity(); r.heap.gen_objs.young.reserve(c + 8); }
    );
    let (arrs_young_ptr, arrs_young_len, arrs_young_cap) = vec_words!(
        "arrs_young",
        |r: &Realm| r.heap.gen_arrs.young.as_ptr(),
        |r: &mut Realm| { let c = r.heap.gen_arrs.young.capacity(); r.heap.gen_arrs.young.reserve(c + 8); }
    );
    realm.heap.gen_arrs.old.words_mut().reserve(16);
    realm.heap.gen_arrs.dirty.words_mut().reserve(16);
    let (arrs_old_ptr, arrs_old_len, _) = vec_words!(
        "arrs_old",
        |r: &Realm| r.heap.gen_arrs.old.words().as_ptr(),
        |r: &mut Realm| { let w = r.heap.gen_arrs.old.words_mut(); let c = w.capacity(); w.reserve(c + 8); }
    );
    let (arrs_dirty_ptr, arrs_dirty_len, _) = vec_words!(
        "arrs_dirty",
        |r: &Realm| r.heap.gen_arrs.dirty.words().as_ptr(),
        |r: &mut Realm| { let w = r.heap.gen_arrs.dirty.words_mut(); let c = w.capacity(); w.reserve(c + 8); }
    );
    realm.heap.gen_objs.old.words_mut().reserve(16);
    realm.heap.gen_objs.dirty.words_mut().reserve(16);
    let (objs_old_ptr, objs_old_len, _) = vec_words!(
        "objs_old",
        |r: &Realm| r.heap.gen_objs.old.words().as_ptr(),
        |r: &mut Realm| { let w = r.heap.gen_objs.old.words_mut(); let c = w.capacity(); w.reserve(c + 8); }
    );
    let (objs_dirty_ptr, objs_dirty_len, _) = vec_words!(
        "objs_dirty",
        |r: &Realm| r.heap.gen_objs.dirty.words().as_ptr(),
        |r: &mut Realm| { let w = r.heap.gen_objs.dirty.words_mut(); let c = w.capacity(); w.reserve(c + 8); }
    );
    let saved = realm.heap.allocs_since_gc;
    realm.heap.allocs_since_gc = 0xB0BA_0005_0001;
    let allocs_since_gc_off = probe!(
        "allocs_since_gc",
        find_word(as_words(&realm), 0xB0BA_0005_0001)
    );
    realm.heap.allocs_since_gc = saved;
    let empty_vec_words: [u64; 3] = {
        let ev: Vec<Value> = Vec::new();
        let w = as_words(&ev);
        [w[0], w[1], w[2]]
    };
    let obj_vlen = std::mem::offset_of!(Obj, vlen) as u32;
    let obj_overflow = std::mem::offset_of!(Obj, overflow) as u32;

    let layout = HeapLayout {
        realm_stack_ptr,
        realm_objs_ptr,
        realm_arrs_ptr,
        nursery_objs_ptr,
        nursery_arrs_ptr,
        obj_bases_off,
        arr_bases_off,
        nursery_objs_len,
        nursery_objs_cap,
        nursery_bytes_off,
        nursery_arrs_len,
        nursery_arrs_cap,
        pool_arr_ptr,
        pool_arr_len,
        pool_arr_cap,
        pretenure_off,
        objs_len,
        objs_cap,
        arrs_len,
        arrs_cap,
        free_objs_len,
        free_arrs_len,
        objs_young_ptr,
        objs_young_len,
        objs_young_cap,
        arrs_young_ptr,
        arrs_young_len,
        arrs_young_cap,
        allocs_since_gc_off,
        arrs_old_ptr,
        arrs_old_len,
        arrs_dirty_ptr,
        arrs_dirty_len,
        objs_old_ptr,
        objs_old_len,
        objs_dirty_ptr,
        objs_dirty_len,
        empty_vec_words,
        obj_vlen,
        obj_overflow,
        obj_size: std::mem::size_of::<Obj>() as u32,
        obj_shape_arc,
        shape_id_delta,
        obj_inline,
        realm_closures_ptr,
        realm_cells_ptr,
        closure_size: std::mem::size_of::<tsr_memory::Closure>() as u32,
        closure_upvals_ptr,
        closure_proto_off,
        realm_stack_len,
        arr_size: std::mem::size_of::<Vec<Value>>() as u32,
        vec_ptr,
        vec_len,
        vec_cap: vec_cap_off,
    };

    if std::env::var_os("TSC_LAYOUT_DEBUG").is_some() {
        eprintln!("[layout] {layout:?}");
        eprintln!("[layout] verify={}", verify(&realm, &layout));
    }
    // self-check: read a known field through the discovered offsets
    verify(&realm, &layout).then_some(layout)
}

fn verify(realm: &Realm, l: &HeapLayout) -> bool {
    unsafe {
        let realm_base = realm as *const Realm as *const u8;
        let objs = *(realm_base.add(l.realm_objs_ptr as usize) as *const *const u8);
        let obj2 = objs.add(2 * l.obj_size as usize);
        let vlen = realm.heap.objs[2].vlen() as u64;
        let v0 = *(obj2.add(l.obj_inline as usize) as *const u64);
        let shape_arc = *(obj2.add(l.obj_shape_arc as usize) as *const *const u8);
        let sid = *(shape_arc.add(l.shape_id_delta as usize) as *const u32);
        let arrs = *(realm_base.add(l.realm_arrs_ptr as usize) as *const *const u8);
        let arr1 = arrs.add(l.arr_size as usize);
        let alen = *(arr1.add(l.vec_len as usize) as *const u64);
        let aptr = *(arr1.add(l.vec_ptr as usize) as *const *const u64);
        let a0 = *aptr;
        if std::env::var_os("TSC_LAYOUT_DEBUG").is_some() {
            eprintln!(
                "[layout] vlen={vlen} v0={v0:x} want={:x} sid={sid} want={} alen={alen} a0={a0:x} want={:x}",
                Value::number(2.0).bits(),
                realm.heap.objs[2].shape.id(),
                Value::number(1.0).bits()
            );
        }
        vlen == 1
            && v0 == Value::number(2.0).bits()
            && sid == realm.heap.objs[2].shape.id()
            && alen == 3
            && a0 == Value::number(1.0).bits()
    }
}

pub fn layout() -> Option<&'static HeapLayout> {
    static L: std::sync::OnceLock<Option<HeapLayout>> = std::sync::OnceLock::new();
    L.get_or_init(|| {
        // ambiguity is address-coincidence luck (two words in Realm happen
        // to hold equal values under this run's allocator layout) — a
        // fresh probe realm rolls new addresses, so retry before giving
        // up and silently running with inline paths disabled (~2x on
        // object-heavy code)
        for attempt in 0..3 {
            if let Some(l) = discover() {
                return Some(l);
            }
            if std::env::var_os("TSC_LAYOUT_DEBUG").is_some() {
                eprintln!("[layout] discovery attempt {} failed, retrying", attempt + 1);
            }
        }
        None
    })
    .as_ref()
}

#[cfg(test)]
mod tests {
    #[test]
    fn probes_succeed() {
        let l = super::layout().expect("layout discovery failed");
        assert!(l.obj_size > 0 && l.arr_size == 24);
    }
}
