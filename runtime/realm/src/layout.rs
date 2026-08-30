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
    pub realm_objs_ptr: u32,
    pub realm_arrs_ptr: u32,
    pub obj_size: u32,
    pub obj_shape_arc: u32,
    pub shape_id_delta: u32,
    pub obj_vals_ptr: u32,
    pub obj_vals_len: u32,
    pub arr_size: u32,
    pub vec_ptr: u32,
    pub vec_len: u32,
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
    debug_assert!(o.shape.id as usize != o.shape.fields.len());
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
        .filter(|(_, &w)| (w as u32) == shape_ref.id || (w >> 32) as u32 == shape_ref.id)
        .map(|(i, &w)| (i, w))
        .collect();
    if id_hits.len() != 1 {
        if dbg {
            eprintln!("[layout] probe failed: shape_id ambiguous ({} hits)", id_hits.len());
        }
        return None;
    }
    let id_word = id_hits[0];
    let id_off_in_sd = if (id_word.1 & 0xFFFF_FFFF) as u32 == shape_ref.id {
        (id_word.0 * 8) as u32
    } else {
        (id_word.0 * 8 + 4) as u32
    };
    let _ = &id_hits;
    let shape_id_delta = shape_data_base_delta + id_off_in_sd;
    let obj_vals_ptr = probe!("obj_vals_ptr", find_word(obj_words, o.values.as_ptr() as u64));
    let obj_vals_len = probe!("obj_vals_len", find_word(obj_words, 3));

    // --- Realm -> heap.objs / heap.arrs data pointers ---
    let mut realm = Realm::new();
    for i in 0..4 {
        let mut po = Obj::default();
        po.set(Arc::from("k"), Value::number(i as f64));
        realm.heap.alloc_obj(po);
        realm.heap.alloc_arr(vec![Value::number(i as f64); 3]);
    }
    let realm_words = as_words(&realm);
    let realm_objs_ptr = probe!("realm_objs_ptr", find_word(realm_words, realm.heap.objs.as_ptr() as u64));
    let realm_arrs_ptr = probe!("realm_arrs_ptr", find_word(realm_words, realm.heap.arrs.as_ptr() as u64));

    let layout = HeapLayout {
        realm_objs_ptr,
        realm_arrs_ptr,
        obj_size: std::mem::size_of::<Obj>() as u32,
        obj_shape_arc,
        shape_id_delta,
        obj_vals_ptr,
        obj_vals_len,
        arr_size: std::mem::size_of::<Vec<Value>>() as u32,
        vec_ptr,
        vec_len,
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
        let vlen = *(obj2.add(l.obj_vals_len as usize) as *const u64);
        let vptr = *(obj2.add(l.obj_vals_ptr as usize) as *const *const u64);
        let v0 = *vptr;
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
                realm.heap.objs[2].shape.id,
                Value::number(1.0).bits()
            );
        }
        vlen == 1
            && v0 == Value::number(2.0).bits()
            && sid == realm.heap.objs[2].shape.id
            && alen == 3
            && a0 == Value::number(1.0).bits()
    }
}

pub fn layout() -> Option<&'static HeapLayout> {
    static L: std::sync::OnceLock<Option<HeapLayout>> = std::sync::OnceLock::new();
    L.get_or_init(discover).as_ref()
}

#[cfg(test)]
mod tests {
    #[test]
    fn probes_succeed() {
        let l = super::layout().expect("layout discovery failed");
        assert!(l.obj_size > 0 && l.arr_size == 24);
    }
}
