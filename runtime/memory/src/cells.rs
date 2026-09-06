//! Heap cells: the in-memory layout of objects, arrays, element chunks,
//! closures and cells in the bump heap. A ref is the cell's address; the
//! first word of every cell is its meta word.
//!
//! ```text
//! meta: bit 0 forwarded (word = new address | 1, only mid-collection)
//!       bit 1 mark      bit 2 dirty (remembered)   bit 3 aged (survived a minor)
//!       bits 4..8 kind  bits 8..32 size in words   bits 32..64 len / vlen / cap / n
//! OBJ     [meta][shape ptr][inline x OBJ_INLINE][spill: ELEMS addr or 0]
//! ARR     [meta(len)][elems: ELEMS addr]
//! ELEMS   [meta(cap)][values x cap]
//! CLOSURE [meta(n)][proto ptr][upvals x n]
//! CELL    [meta][value]
//! FREE    [meta][next free]
//! ```

use crate::{Ref, Repr, ShapeData, Value};
use std::sync::Arc;
use tsc_ir::FunctionProto;

pub const K_OBJ: u64 = 1;
pub const K_ARR: u64 = 2;
pub const K_ELEMS: u64 = 3;
pub const K_CLOSURE: u64 = 4;
pub const K_CELL: u64 = 5;
pub const K_FREE: u64 = 6;

pub const M_FWD: u64 = 1;
pub const M_MARK: u64 = 2;
pub const M_DIRTY: u64 = 4;
pub const M_AGED: u64 = 8;
const KIND_SHIFT: u32 = 4;
const SIZE_SHIFT: u32 = 8;
const LEN_SHIFT: u32 = 32;
const SIZE_MASK: u64 = (1 << 24) - 1;

/// Inline value slots per object; fields beyond spill to an ELEMS chunk.
pub const OBJ_INLINE: usize = 4;
pub const OBJ_WORDS: usize = 3 + OBJ_INLINE;
pub const ARR_WORDS: usize = 2;
pub const CELL_WORDS: usize = 2;

#[inline(always)]
pub const fn meta(kind: u64, size_words: usize, len: usize) -> u64 {
    (kind << KIND_SHIFT) | ((size_words as u64) << SIZE_SHIFT) | ((len as u64) << LEN_SHIFT)
}
#[inline(always)]
pub fn kind_of(m: u64) -> u64 {
    (m >> KIND_SHIFT) & 0xF
}
#[inline(always)]
pub fn size_words(m: u64) -> usize {
    ((m >> SIZE_SHIFT) & SIZE_MASK) as usize
}
#[inline(always)]
pub fn len_of(m: u64) -> usize {
    (m >> LEN_SHIFT) as usize
}
#[inline(always)]
pub fn with_len(m: u64, len: usize) -> u64 {
    (m & ((1u64 << LEN_SHIFT) - 1)) | ((len as u64) << LEN_SHIFT)
}

#[inline(always)]
pub fn meta_at(a: Ref) -> u64 {
    // SAFETY: every ref handed out by the heap points at a live cell.
    unsafe { *(a as *const u64) }
}
#[inline(always)]
pub fn set_meta(a: Ref, m: u64) {
    unsafe { *(a as *mut u64) = m }
}
#[inline(always)]
pub fn word(a: Ref, i: usize) -> u64 {
    unsafe { *(a as *const u64).add(i) }
}
#[inline(always)]
pub fn set_word(a: Ref, i: usize, w: u64) {
    unsafe { *(a as *mut u64).add(i) = w }
}

/// Element chunk view: `[meta(cap)][values]`.
#[inline(always)]
pub fn elems_cap(e: Ref) -> usize {
    len_of(meta_at(e))
}
#[inline(always)]
pub fn elems_slice<'a>(e: Ref, len: usize) -> &'a [Value] {
    unsafe { std::slice::from_raw_parts((e as *const Value).add(1), len) }
}
#[inline(always)]
pub fn elems_slice_mut<'a>(e: Ref, len: usize) -> &'a mut [Value] {
    unsafe { std::slice::from_raw_parts_mut((e as *mut Value).add(1), len) }
}

/// Object view over its cell. `repr(C)`: the JIT reads these offsets.
#[repr(C)]
pub struct Obj {
    pub meta: u64,
    pub shape: &'static ShapeData,
    pub inline: [Value; OBJ_INLINE],
    /// ELEMS chunk holding slots past `OBJ_INLINE`, or 0.
    pub spill: u64,
}

impl Obj {
    #[inline(always)]
    pub fn vlen(&self) -> usize {
        len_of(self.meta)
    }
    #[inline(always)]
    pub fn val(&self, i: usize) -> Value {
        if i < OBJ_INLINE {
            self.inline[i]
        } else {
            elems_slice(self.spill, self.vlen() - OBJ_INLINE)[i - OBJ_INLINE]
        }
    }
    /// GC-side store: moves a value, no representation bookkeeping.
    #[inline(always)]
    pub fn set_val(&mut self, i: usize, v: Value) {
        if i < OBJ_INLINE {
            self.inline[i] = v;
        } else {
            elems_slice_mut(self.spill, self.vlen() - OBJ_INLINE)[i - OBJ_INLINE] = v;
        }
    }
    /// Mutator store into an existing slot: keeps the shape's
    /// representation honest.
    #[inline(always)]
    pub fn store(&mut self, i: usize, v: Value) {
        debug_assert!(i < self.vlen());
        self.shape.widen(i, Repr::of(v));
        self.set_val(i, v);
    }
    pub fn get(&self, name: &str) -> Option<Value> {
        self.shape.slot_of(name).map(|i| self.val(i))
    }
    /// (name, value) pairs in slot order.
    pub fn entries(&self) -> impl Iterator<Item = (&Arc<str>, Value)> {
        self.shape.fields.iter().zip((0..self.vlen()).map(|i| self.val(i)))
    }
    /// Capacity of the spill chunk, 0 when there is none.
    #[inline(always)]
    pub fn spill_cap(&self) -> usize {
        if self.spill == 0 { 0 } else { elems_cap(self.spill) }
    }
}

/// Closure view: `[meta(n)][proto][upvals]`.
#[repr(C)]
pub struct Closure {
    pub meta: u64,
    proto: *const FunctionProto,
}

impl Closure {
    #[inline(always)]
    pub fn proto(&self) -> &FunctionProto {
        // SAFETY: protos are Arc-owned by their module for the process
        // lifetime; `hold_proto` covers the cross-realm case.
        unsafe { &*self.proto }
    }
    /// Proto identity word, what the call inline cache compares.
    #[inline(always)]
    pub fn proto_ptr(&self) -> *const FunctionProto {
        self.proto
    }
    /// A fresh strong Arc for the proto (portable values need one).
    pub fn proto_arc(&self) -> Arc<FunctionProto> {
        // SAFETY: the pointer came from `Arc::as_ptr` of a live Arc.
        unsafe {
            Arc::increment_strong_count(self.proto);
            Arc::from_raw(self.proto)
        }
    }
    #[inline(always)]
    pub fn upvals(&self) -> &[Value] {
        let n = len_of(self.meta);
        unsafe { std::slice::from_raw_parts((self as *const Closure as *const Value).add(2), n) }
    }
    #[inline(always)]
    pub fn upvals_mut(&mut self) -> &mut [Value] {
        let n = len_of(self.meta);
        unsafe { std::slice::from_raw_parts_mut((self as *mut Closure as *mut Value).add(2), n) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_round_trips() {
        let m = meta(K_ELEMS, 33, 32) | M_AGED;
        assert_eq!(kind_of(m), K_ELEMS);
        assert_eq!(size_words(m), 33);
        assert_eq!(len_of(m), 32);
        assert_eq!(len_of(with_len(m, 7)), 7);
        assert_eq!(size_words(with_len(m, 7)), 33);
        assert_eq!(std::mem::size_of::<Obj>(), OBJ_WORDS * 8);
        assert_eq!(std::mem::size_of::<Closure>(), 16);
    }
}
