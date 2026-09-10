//! Allocation spaces. `Young` is one aligned reservation bump-allocated
//! from the bottom; a minor collection copies survivors out and resets
//! the bump pointer. `Old` is a list of chunks with size-class free
//! lists rebuilt by the major sweep, bump allocation as the fallback.
//!
//! Neither allocator ever collects: the mutator holds raw addresses in
//! Rust locals across allocations, so a collection happens only at a
//! safepoint.

use crate::cells::*;
use crate::Ref;
use std::alloc::{alloc, dealloc, Layout};

/// Virtual reservation of the nursery. Aligned to its own size so the
/// young test is `(addr & !(RESERVE-1)) == base`. Untouched pages cost
/// no RSS; the soft limit (`TSC_NURSERY_BYTES`) is what triggers a minor.
pub const YOUNG_RESERVE: usize = 256 << 20;

#[repr(C)]
pub struct Young {
    pub base: usize,
    pub top: usize,
    /// Soft limit: `needs_gc` when `top` passes it. Allocation itself
    /// may run on to the end of the reservation.
    pub limit: usize,
}

impl Young {
    pub fn new(soft_limit: usize) -> Self {
        let layout = Layout::from_size_align(YOUNG_RESERVE, YOUNG_RESERVE).unwrap();
        // SAFETY: valid non-zero layout; large aligned requests are
        // lazily backed, so the reservation is virtual until touched.
        let base = unsafe { alloc(layout) } as usize;
        assert!(base != 0, "nursery reservation failed");
        assert!(base >> 48 == 0, "heap address exceeds the 48-bit Value payload");
        Young { base, top: base, limit: base + soft_limit.min(YOUNG_RESERVE) }
    }
    #[inline(always)]
    pub fn contains(&self, a: Ref) -> bool {
        (a as usize) & !(YOUNG_RESERVE - 1) == self.base
    }
    #[inline(always)]
    pub fn alloc(&mut self, words: usize) -> Ref {
        let a = self.top;
        let next = a + words * 8;
        assert!(next <= self.base + YOUNG_RESERVE, "nursery reservation exhausted between safepoints");
        self.top = next;
        a as Ref
    }
    #[inline(always)]
    pub fn used(&self) -> usize {
        self.top - self.base
    }
    pub fn reset(&mut self) {
        self.top = self.base;
    }
    /// Debug aid (`TSC_GC_POISON`): a stale young ref then reads garbage
    /// that fails every tag check instead of silently aliasing.
    pub fn poison(&mut self, upto: usize) {
        // SAFETY: [base, upto) was allocated and is dead after a minor.
        unsafe { std::ptr::write_bytes(self.base as *mut u8, 0xDD, upto - self.base) }
    }
}

impl Drop for Young {
    fn drop(&mut self) {
        let layout = Layout::from_size_align(YOUNG_RESERVE, YOUNG_RESERVE).unwrap();
        unsafe { dealloc(self.base as *mut u8, layout) }
    }
}

const CHUNK_BYTES: usize = 1 << 20;
/// Cells up to this many words have an exact-size free list.
pub const MAX_SMALL_WORDS: usize = 32;

pub struct Chunk {
    pub base: usize,
    /// Bump pointer: every word below it is a valid cell sequence.
    pub top: usize,
    pub end: usize,
}

#[repr(C)]
pub struct Old {
    /// Bump region of the current chunk; the JIT's pretenure template
    /// reads these two words.
    pub top: usize,
    pub limit: usize,
    pub chunks: Vec<Chunk>,
    /// Exact-size lists indexed by words (index 0 and 1 unused). A fixed
    /// array: the JIT pops these heads at `offset_of!(Old, small)`.
    pub small: [Ref; MAX_SMALL_WORDS + 1],
    /// Free cells above `MAX_SMALL_WORDS`, first fit with split.
    // ponytail: linear scan; a size-ordered tree if large churn shows up
    large: Vec<Ref>,
    /// Free-list bytes handed out since the last minor; bump bytes are
    /// `top - mark` (see `bytes_since_minor`).
    freelist_bytes: usize,
    /// Bump position at the last minor / chunk switch.
    mark: usize,
    /// Start of the born range: cells bump-allocated since the last
    /// minor lie in `[born_from, top)` of chunk `born_chunk` and in every
    /// later chunk. The JIT's old-space template bumps `top` and writes
    /// nothing else, so the range is the log.
    pub born_chunk: usize,
    pub born_from: usize,
    /// Live bytes measured by the last sweep.
    pub live_bytes: usize,
}

impl Old {
    pub fn new() -> Self {
        Old {
            top: 0,
            limit: 0,
            chunks: Vec::new(),
            small: [0; MAX_SMALL_WORDS + 1],
            large: Vec::new(),
            freelist_bytes: 0,
            mark: 0,
            born_chunk: 0,
            born_from: 0,
            live_bytes: 0,
        }
    }

    /// Bytes allocated old since the last minor.
    pub fn bytes_since_minor(&self) -> usize {
        self.freelist_bytes + (self.top - self.mark)
    }
    /// Is `a` a bump-allocated cell of the current born range (and so
    /// needs no log entry)?
    #[inline(always)]
    /// Diagnostics only. Chunks come from the system allocator and are
    /// *not* address-ordered, so this has to find the owning chunk
    /// rather than compare against the last one: doing the latter
    /// reported cells in an early chunk as born whenever that chunk sat
    /// at a higher address, and those cells then went in no sweep list
    /// at all.
    pub fn in_born_range(&self, a: Ref) -> bool {
        let a = a as usize;
        match self.chunks.iter().position(|c| a >= c.base && a < c.end) {
            Some(i) if i > self.born_chunk => true,
            Some(i) if i == self.born_chunk => a >= self.born_from,
            _ => false,
        }
    }
    /// Close the born range after a minor: everything below `top` is
    /// aged or free now.
    pub fn end_minor(&mut self) {
        self.sync_top();
        self.freelist_bytes = 0;
        self.mark = self.top;
        self.born_chunk = self.chunks.len().saturating_sub(1);
        self.born_from = self.top;
    }

    fn new_chunk(&mut self, min_bytes: usize) {
        let bytes = min_bytes.max(CHUNK_BYTES);
        let layout = Layout::from_size_align(bytes, 4096).unwrap();
        // SAFETY: valid layout. Zeroed so a partial sweep walk never sees
        // a garbage meta word.
        let base = unsafe { std::alloc::alloc_zeroed(layout) } as usize;
        assert!(base != 0, "old-space chunk allocation failed");
        assert!(base >> 48 == 0, "heap address exceeds the 48-bit Value payload");
        // the old chunk's tail counted; the new one counts from its base
        self.freelist_bytes += self.top.saturating_sub(self.mark);
        self.chunks.push(Chunk { base, top: base, end: base + bytes });
        self.top = base;
        self.limit = base + bytes;
        self.mark = base;
    }

    /// Store the current bump position back into its chunk record (the
    /// JIT bumps `top` directly, so walks call this first).
    pub fn sync_top(&mut self) {
        if let Some(c) = self.chunks.last_mut() {
            c.top = self.top;
        }
    }

    /// Allocate `words` (>= 2). The cell comes back with a zeroed meta
    /// word; the caller writes the real one. The flag says the cell came
    /// off the bump region, so it lies in the born range and needs no
    /// log entry — the allocator knows this outright, where deciding it
    /// from the address needs chunks to be address-ordered, and they are
    /// not (see `in_born_range`).
    pub fn alloc(&mut self, words: usize) -> (Ref, bool) {
        debug_assert!(words >= 2);
        if words <= MAX_SMALL_WORDS {
            let head = self.small[words];
            if head != 0 {
                self.small[words] = word(head, 1);
                set_meta(head, 0);
                self.freelist_bytes += words * 8;
                return (head, false);
            }
        } else {
            for i in 0..self.large.len() {
                let a = self.large[i];
                let have = size_words(meta_at(a));
                if have >= words {
                    self.large.swap_remove(i);
                    let rest = have - words;
                    if rest >= 2 {
                        self.push_free(a + (words * 8) as Ref, rest);
                    }
                    set_meta(a, 0);
                    self.freelist_bytes += words * 8;
                    return (a, false);
                }
            }
        }
        let bytes = words * 8;
        if self.top + bytes > self.limit {
            self.sync_top();
            self.new_chunk(bytes);
        }
        let a = self.top;
        self.top += bytes;
        self.sync_top();
        (a as Ref, true)
    }

    /// Register a free cell of `words` (writes its FREE header).
    pub fn push_free(&mut self, a: Ref, words: usize) {
        set_meta(a, meta(K_FREE, words, 0));
        if words <= MAX_SMALL_WORDS {
            set_word(a, 1, self.small[words]);
            self.small[words] = a;
        } else {
            self.large.push(a);
        }
    }

    pub fn clear_free_lists(&mut self) {
        self.small.iter_mut().for_each(|h| *h = 0);
        self.large.clear();
    }

    pub fn allocated_bytes(&self) -> usize {
        let last = self.chunks.len().saturating_sub(1);
        self.chunks
            .iter()
            .enumerate()
            .map(|(i, c)| if i == last { self.top - c.base } else { c.top - c.base })
            .sum()
    }
}

/// Fixed-capacity log of old-space cells the JIT took off a free list
/// (a bump-allocated cell is in the born range instead). The JIT stores
/// the boxed Value at `ptr[len]`; a full buffer sends it to the helper.
#[repr(C)]
pub struct BornBuf {
    pub ptr: *mut u64,
    pub len: usize,
    pub cap: usize,
    _store: Box<[u64]>,
}

impl BornBuf {
    pub fn new(cap: usize) -> Self {
        let mut store = vec![0u64; cap].into_boxed_slice();
        BornBuf { ptr: store.as_mut_ptr(), len: 0, cap, _store: store }
    }
    pub fn drain(&mut self) -> impl Iterator<Item = u64> + '_ {
        let n = self.len;
        self.len = 0;
        // SAFETY: ptr..ptr+n was written by the JIT / push.
        (0..n).map(move |i| unsafe { *self.ptr.add(i) })
    }
    #[inline(always)]
    pub fn is_full(&self) -> bool {
        self.len >= self.cap
    }
}

impl Drop for Old {
    fn drop(&mut self) {
        for c in &self.chunks {
            let layout = Layout::from_size_align(c.end - c.base, 4096).unwrap();
            unsafe { dealloc(c.base as *mut u8, layout) }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn young_bump_and_contains() {
        let mut y = Young::new(1 << 20);
        let a = y.alloc(7);
        let b = y.alloc(2);
        assert_eq!(b - a, 56);
        assert!(y.contains(a) && y.contains(b));
        assert!(!y.contains(a + YOUNG_RESERVE as Ref));
        assert_eq!(y.used(), 72);
        y.reset();
        assert_eq!(y.used(), 0);
    }

    #[test]
    fn old_reuses_freed_cells_and_splits_large() {
        let mut o = Old::new();
        let (a, bumped_a) = o.alloc(7);
        assert!(bumped_a, "first cell of a fresh chunk comes off the bump");
        set_meta(a, meta(K_OBJ, 7, 0));
        let (b, _) = o.alloc(7);
        o.push_free(a, 7);
        let (reused, bumped) = o.alloc(7);
        assert_eq!(reused, a, "exact-size free list reused");
        assert!(!bumped, "a free-list cell is not in the born range");
        assert_ne!(o.alloc(7).0, b);
        let (big, _) = o.alloc(100);
        o.push_free(big, 100);
        let (part, split_bumped) = o.alloc(40);
        assert_eq!(part, big);
        assert!(!split_bumped, "a split large cell is not in the born range either");
        let (rest, _) = o.alloc(50);
        assert_eq!(rest, big + 40 * 8);
    }
}
