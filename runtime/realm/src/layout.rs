//! Heap layout for the JIT templates. Cell and heap-word offsets come
//! from `offset_of!` on the runtime's own structs; the two `Vec`s whose
//! internal word order Rust does not promise (`Realm.stack`, the const
//! cache) are located by probing a scratch realm.

use crate::Realm;
use std::mem::offset_of;
use tsr_jit::baseline::HeapOffsets;
use tsr_memory::cells::{Obj, OBJ_INLINE, OBJ_WORDS};
use tsr_memory::space::{BornBuf, Old, Young, YOUNG_RESERVE};
use tsr_memory::Heap;

/// Word offset inside `Realm` of the `Vec` word holding `needle`,
/// searched only within the three words of the Vec at `vec_off`.
fn vec_word(realm: &Realm, vec_off: usize, needle: u64) -> Option<u32> {
    let base = realm as *const Realm as *const u64;
    (0..3).find_map(|k| {
        let off = vec_off + k * 8;
        // SAFETY: inside the Realm allocation, 8-aligned.
        let w = unsafe { *base.add(off / 8) };
        (w == needle).then_some(off as u32)
    })
}

fn discover() -> Option<HeapOffsets> {
    let mut realm = Realm::new();
    realm.stack.reserve(64);
    realm.stack.resize(37, tsr_memory::Value::UNDEFINED);
    let stack_off = offset_of!(Realm, stack);
    let realm_stack_ptr = vec_word(&realm, stack_off, realm.stack.as_ptr() as u64)?;
    let realm_stack_len = vec_word(&realm, stack_off, 37)?;
    let cc_off = offset_of!(Realm, const_cache);
    let const_cache_ptr = vec_word(&realm, cc_off, realm.const_cache.as_ptr() as u64)?;
    let heap = offset_of!(Realm, heap);
    let young = heap + offset_of!(Heap, young);
    let old = heap + offset_of!(Heap, old);
    let born = heap + offset_of!(Heap, born_buf);
    Some(HeapOffsets {
        realm_stack_ptr,
        realm_stack_len,
        const_cache_ptr,
        young_base: (young + offset_of!(Young, base)) as u32,
        young_top: (young + offset_of!(Young, top)) as u32,
        young_limit: (young + offset_of!(Young, limit)) as u32,
        old_top: (old + offset_of!(Old, top)) as u32,
        old_limit: (old + offset_of!(Old, limit)) as u32,
        old_small: (old + offset_of!(Old, small)) as u32,
        born_ptr: (born + offset_of!(BornBuf, ptr)) as u32,
        born_len: (born + offset_of!(BornBuf, len)) as u32,
        born_cap: (born + offset_of!(BornBuf, cap)) as u32,
        pretenure_off: (heap + offset_of!(Heap, pretenure)) as u32,
        young_shift: YOUNG_RESERVE.trailing_zeros(),
        bump_alloc: std::env::var_os("TSC_NO_NURSERY").is_none(),
        obj_shape: offset_of!(Obj, shape) as u32,
        obj_inline: offset_of!(Obj, inline) as u32,
        obj_inline_n: OBJ_INLINE as u32,
        obj_spill: offset_of!(Obj, spill) as u32,
        obj_words: OBJ_WORDS as u32,
        arr_elems: 8,
        elems_vals: 8,
        closure_proto: 8,
        closure_upvals: 16,
        cell_val: 8,
        shape_id: 0, // ShapeData is repr(C) with `id` first
    })
}

pub fn layout() -> Option<&'static HeapOffsets> {
    static L: std::sync::OnceLock<Option<HeapOffsets>> = std::sync::OnceLock::new();
    L.get_or_init(discover).as_ref()
}

#[cfg(test)]
mod tests {
    #[test]
    fn probes_succeed() {
        let l = super::layout().expect("layout discovery failed");
        assert!(l.realm_stack_ptr != l.realm_stack_len);
        assert_eq!(l.obj_shape, 8);
        assert_eq!(l.obj_inline, 16);
    }
}
