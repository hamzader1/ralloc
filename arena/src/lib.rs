mod platform;
use core::ptr::null_mut;
use platform::Platform;
use std::ptr;

pub struct BlockHeader {
    prev: *mut BlockHeader,
    mmap_ptr: *mut u8,
    mmap_size: usize,
}
pub struct Arena {
    current_block: *mut BlockHeader,
    cursor: *mut u8,
    end: *mut u8,
}

struct EmptyBlockWrapper(BlockHeader);
unsafe impl Sync for EmptyBlockWrapper {}

static EMPTY_BLOCK: EmptyBlockWrapper = EmptyBlockWrapper(BlockHeader {
    prev: core::ptr::null_mut(),
    mmap_ptr: core::ptr::null_mut(),
    mmap_size: 0,
});

impl BlockHeader {
    fn new(prev: *mut BlockHeader, mmap_ptr: *mut u8, mmap_size: usize) -> Self {
        Self {
            prev,
            mmap_ptr,
            mmap_size,
        }
    }
}

impl Arena {
    pub fn new() -> Self {
        let page_size = Platform::get_page_size();
        let mmap_ptr = Platform::mmap(page_size);

        let block_header = BlockHeader {
            prev: null_mut(),
            mmap_ptr,
            mmap_size: page_size,
        };
        let mut arena = Self {
            current_block: null_mut(),
            cursor: mmap_ptr,
            end: unsafe { mmap_ptr.add(page_size) },
        };
        arena.write_metadata(block_header);
        arena
    }

    pub fn alloc(&mut self, layout: std::alloc::Layout) -> *mut u8 {
        let (size, align) = (layout.size(), layout.align());
        if size == 0 {
            return null_mut();
        }

        let aligned_cursor = match Self::align_up(self.cursor as usize, align) {
            Some(ac) => ac,
            None => return null_mut(),
        };

        let alloc_end = match aligned_cursor.checked_add(size) {
            Some(new_block_size) => new_block_size,
            None => return null_mut(),
        };

        if alloc_end > self.end as usize {
            self.grow(size, align);
            return self.alloc(layout);
        }

        self.cursor = unsafe { self.cursor.add(alloc_end - self.cursor as usize) };
        unsafe {
            let current_block_ptr = (*self.current_block).mmap_ptr;
            current_block_ptr.add(aligned_cursor - current_block_ptr as usize)
        }
    }

    pub fn align_up(size: usize, align: usize) -> Option<usize> {
        let checked_cursor_alignment = size.checked_add(align - 1)?;
        Some(checked_cursor_alignment & !(align - 1))
    }

   pub fn align_up_unchecked(size: usize, align: usize) -> usize {
    (size + align -1) &!(align -1)
    }
    fn grow(&mut self, requested_size: usize, requested_align: usize) {
        let prev_block_header = self.current_block;
        let prev_block_size = unsafe { (*self.current_block).mmap_size };
        let aligned_requested_size = Self::align_up(
            requested_size + size_of::<BlockHeader>() + (requested_align - 1),
            Platform::get_page_size(),
        )
        .expect("size overflow");
        let new_block_size = match prev_block_size.checked_mul(2) {
            Some(d) => d.max(aligned_requested_size),
            None => aligned_requested_size,
        };

        let ptr = Platform::mmap(new_block_size);
        if ptr.is_null() {
            panic!("FAILED TO ALLOCATE MORE MEMORY");
        }
        let new_block_header = BlockHeader::new(prev_block_header, ptr, new_block_size);
        self.end = unsafe { ptr.add(new_block_size) };
        self.write_metadata(new_block_header);
    }

    fn write_metadata(&mut self, block_header: BlockHeader) {
        let header_ptr = block_header.mmap_ptr as *mut BlockHeader;
        unsafe {
            self.cursor = block_header.mmap_ptr.add(
                (size_of::<BlockHeader>() + align_of::<BlockHeader>() - 1)
                    & !(align_of::<BlockHeader>() - 1),
            );
            header_ptr.write(block_header);
            self.current_block = header_ptr;
        }
    }
    #[allow(dead_code)]
    fn reset(&mut self) {
        unsafe {
            while !((*self.current_block).prev.is_null()) {
                let current_block = ptr::read(self.current_block);
                Platform::munmap(current_block.mmap_ptr, current_block.mmap_size);
                self.current_block = current_block.prev;
            }

            let current = core::ptr::read(self.current_block);
            self.cursor = current
                .mmap_ptr
                .add(Self::align_up(size_of::<BlockHeader>(), align_of::<BlockHeader>()).unwrap());

            self.end = current.mmap_ptr.add(current.mmap_size);
        }
    }
}

impl Drop for Arena {
    fn drop(&mut self) {
        let mut current = self.current_block;
        unsafe {
            while !(current.is_null()) {
                let current_block = core::ptr::read(current);
                Platform::munmap(current_block.mmap_ptr, current_block.mmap_size);
                current = current_block.prev;
            }
        }
    }
}

impl std::default::Default for Arena {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::alloc::Layout;

    #[test]
    fn write_metadata_places_header_at_mmap_ptr_and_aligns_cursor() {
        let arena = Arena::new();

        // header should live at the very start of the chunk
        let mmap_ptr = unsafe { (*arena.current_block).mmap_ptr };
        assert_eq!(arena.current_block as *mut u8, mmap_ptr);

        // cursor should be header_size rounded up to 8, not raw header_size
        let expected_offset = Arena::align_up(size_of::<BlockHeader>(), 8).unwrap();
        let expected_cursor = unsafe { mmap_ptr.add(expected_offset) };
        assert_eq!(arena.cursor, expected_cursor);

        // end should be exactly one page past mmap_ptr
        let page_size = Platform::get_page_size();
        assert_eq!(arena.end, unsafe { mmap_ptr.add(page_size) });
    }

    #[test]
    fn write_metadata_links_prev_correctly_on_grow() {
        let mut arena = Arena::new();
        let first_block = arena.current_block;

        // force a grow with a request bigger than default chunk
        let huge = std::alloc::Layout::from_size_align(Platform::get_page_size() * 4, 8).unwrap();
        arena.alloc(huge);

        let second_block = arena.current_block;
        assert_ne!(first_block, second_block);
        assert_eq!(unsafe { (*second_block).prev }, first_block);
        assert_eq!(unsafe { (*first_block).prev }, null_mut());
    }

    #[test]
    fn alloc_returns_non_null_and_moves_cursor() {
        let mut arena = Arena::new();
        let layout = std::alloc::Layout::from_size_align(16, 8).unwrap();

        let cursor_before = arena.cursor;
        let ptr = arena.alloc(layout);

        assert!(!ptr.is_null());
        assert!(ptr as usize >= cursor_before as usize); // ptr can only move forward (for alignment padding)
        assert_eq!(ptr as usize % 8, 0); // actually aligned as requested
        assert_eq!(arena.cursor, unsafe { ptr.add(16) }); // cursor now sits right after this allocation
    }

    #[test]
    fn alloc_zero_size_returns_null() {
        let mut arena = Arena::new();
        let layout = std::alloc::Layout::from_size_align(0, 8).unwrap();
        assert!(arena.alloc(layout).is_null());
    }

    #[test]
    fn consecutive_allocs_do_not_overlap() {
        let mut arena = Arena::new();
        let layout = std::alloc::Layout::from_size_align(24, 8).unwrap();

        let a = arena.alloc(layout);
        let b = arena.alloc(layout);

        assert!(!a.is_null() && !b.is_null());
        assert_ne!(a, b);
        // b must start at or after a + size
        assert!(b as usize >= a as usize + 24);
    }

    #[test]
    fn alloc_respects_alignment() {
        let mut arena = Arena::new();
        // deliberately unbalance cursor first
        arena.alloc(std::alloc::Layout::from_size_align(3, 1).unwrap());

        let layout = std::alloc::Layout::from_size_align(32, 32).unwrap();
        let ptr = arena.alloc(layout);

        assert_eq!(ptr as usize % 32, 0);
    }

    #[test]
    fn alloc_never_writes_past_end() {
        let mut arena = Arena::new();
        let layout = std::alloc::Layout::from_size_align(64, 8).unwrap();

        for _ in 0..1000 {
            let ptr = arena.alloc(layout);
            assert!(!ptr.is_null());
            assert!(unsafe { ptr.add(64) } as usize <= arena.end as usize);
        }
    }

    #[test]
    fn alloc_triggers_grow_when_chunk_is_full() {
        let mut arena = Arena::new();
        let page_size = Platform::get_page_size();
        let first_block = arena.current_block;

        // fill up the first chunk entirely
        let filler = std::alloc::Layout::from_size_align(page_size, 8).unwrap();
        let _ = arena.alloc(filler); // likely triggers grow since header eats some space

        assert_ne!(
            arena.current_block, first_block,
            "expected grow to have run"
        );
    }

    #[test]
    fn grow_chunk_size_at_least_fits_request() {
        let mut arena = Arena::new();
        let page_size = Platform::get_page_size();
        let requested = page_size * 10;

        let layout = std::alloc::Layout::from_size_align(requested, 8).unwrap();
        let ptr = arena.alloc(layout);

        assert!(!ptr.is_null());
        let new_block_size = unsafe { (*arena.current_block).mmap_size };
        assert!(new_block_size >= requested + size_of::<BlockHeader>());
    }

    #[test]
    fn grow_doubles_when_request_is_small() {
        let mut arena = Arena::new();
        let first_size = unsafe { (*arena.current_block).mmap_size };

        // force exactly one grow with a small request
        let filler = std::alloc::Layout::from_size_align(first_size, 8).unwrap();
        let _ = arena.alloc(filler);

        let new_size = unsafe { (*arena.current_block).mmap_size };
        assert_eq!(new_size, first_size * 2);
    }
    #[test]
    fn alloc_cursor_accounts_for_alignment_padding() {
        let mut arena = Arena::new();

        // Step 1: force cursor to an oddly-unaligned position.
        // Alloc 3 bytes with align=1 -- guarantees no padding was added here,
        // so afterward self.cursor sits at some address with no particular alignment.
        let odd_layout = std::alloc::Layout::from_size_align(3, 1).unwrap();
        let odd_ptr = arena.alloc(odd_layout);
        assert!(!odd_ptr.is_null());

        let cursor_after_odd = arena.cursor as usize;

        // Step 2: alloc something that requires real alignment padding.
        // If cursor_after_odd isn't already a multiple of 32, this forces padding.
        let big_align_layout = std::alloc::Layout::from_size_align(64, 32).unwrap();
        let big_ptr = arena.alloc(big_align_layout);
        assert!(!big_ptr.is_null());

        // Compute what SHOULD have happened, independently, using the same
        // align_up logic the allocator itself uses.
        let expected_aligned = Arena::align_up(cursor_after_odd, 32).unwrap();
        let expected_new_cursor = expected_aligned + 64;

        // 1. Returned pointer must be at the correctly aligned address, not raw cursor.
        assert_eq!(big_ptr as usize, expected_aligned);
        assert_eq!(big_ptr as usize % 32, 0);

        // 2. Cursor after the alloc must equal aligned_start + size --
        //    NOT old_cursor + size (which is the buggy formula).
        assert_eq!(arena.cursor as usize, expected_new_cursor);

        // 3. The two allocated regions must not overlap:
        //    odd_ptr..odd_ptr+3 must end before big_ptr starts.
        assert!(odd_ptr as usize + 3 <= big_ptr as usize);

        // 4. Sanity: if there WAS padding (cursor wasn't already 32-aligned),
        //    prove it's nonzero, so this test is actually exercising the bug path
        //    and not accidentally testing the zero-padding case.
        let padding = expected_aligned - cursor_after_odd;
        assert!(
            padding > 0,
            "test didn't actually exercise padding -- cursor was already aligned by luck, rerun/adjust sizes"
        );
    }

    // 1. drop does not panic / crash on a single block
    #[test]
    fn test_drop_single_block() {
        let arena = Arena::new();
        drop(arena);
        // if this test completes without segfault/panic, drop worked
    }

    // 2. drop does not panic after growth (multiple blocks)
    #[test]
    fn test_drop_multiple_blocks() {
        let mut arena = Arena::new();
        let page = Platform::get_page_size();

        // force growth by allocating past the first block
        let layout = Layout::from_size_align(page * 2, 8).unwrap();
        let ptr = arena.alloc(layout);
        assert!(!ptr.is_null());

        drop(arena);
    }

    // 3. drop walks the entire prev chain (3+ blocks)
    #[test]
    fn test_drop_walks_full_chain() {
        let mut arena = Arena::new();
        let page = Platform::get_page_size();

        // force multiple growths
        for _ in 0..5 {
            let layout = Layout::from_size_align(page * 2, 8).unwrap();
            let ptr = arena.alloc(layout);
            assert!(!ptr.is_null());
        }

        drop(arena);
    }

    // 4. after drop, a NEW arena can still successfully mmap
    //    (proves memory was actually returned to the OS, not leaked)
    #[test]
    fn test_drop_releases_memory_for_reuse() {
        {
            let mut arena = Arena::new();
            let page = Platform::get_page_size();
            for _ in 0..10 {
                let layout = Layout::from_size_align(page * 4, 8).unwrap();
                arena.alloc(layout);
            }
            // arena drops here
        }

        // if previous arena leaked, this should still succeed
        // (not a strict leak proof, but catches gross failures)
        let arena2 = Arena::new();
        drop(arena2);
    }

    // 5. dropping an arena with zero extra allocations (just the initial block)
    #[test]
    fn test_drop_initial_block_only() {
        let arena = Arena::new();
        // no alloc calls at all
        drop(arena);
    }

    // 6. stress: create and drop many arenas in a loop (catches leaks via OOM if broken)
    #[test]
    fn test_drop_stress_many_arenas() {
        for _ in 0..1000 {
            let mut arena = Arena::new();
            let layout = Layout::from_size_align(64, 8).unwrap();
            arena.alloc(layout);
            drop(arena);
        }
        // if drop leaks, this loop will exhaust address space / OOM eventually
    }

    // simple xorshift PRNG so tests are deterministic and reproducible
    struct Rng(u64);
    impl Rng {
        fn new(seed: u64) -> Self {
            Self(seed)
        }
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn range(&mut self, lo: u64, hi: u64) -> u64 {
            lo + (self.next() % (hi - lo))
        }
    }

    #[test]
    fn stress_random_allocs_no_overlap() {
        let mut arena = Arena::new();
        let mut rng = Rng::new(0xDEADBEEF);
        // shadow: track every live allocation as (start, size)
        let mut live: Vec<(usize, usize)> = Vec::new();

        let iteration = if cfg!(miri) { 500 } else { 100_000 };
        for i in 0..iteration {
            let size = rng.range(1, 4096) as usize;
            let align_pow = rng.range(0, 7) as u32; // 1,2,4,8,16,32,64
            let align = 1usize << align_pow;

            let layout = Layout::from_size_align(size, align).unwrap();
            let ptr = arena.alloc(layout);

            assert!(!ptr.is_null(), "alloc failed at iteration {}", i);

            let addr = ptr as usize;

            // alignment check
            assert_eq!(addr % align, 0, "misaligned at iter {}: align={}", i, align);

            // overlap check against every previous live allocation
            for &(start, sz) in &live {
                let overlaps = addr < start + sz && start < addr + size;
                assert!(
                    !overlaps,
                    "OVERLAP at iter {}: new=({:#x},{}) vs existing=({:#x},{})",
                    i, addr, size, start, sz
                );
            }

            // write a pattern derived from the pointer itself, into every byte
            unsafe {
                for b in 0..size {
                    *ptr.add(b) = (addr as u8).wrapping_add(b as u8);
                }
            }

            live.push((addr, size));
        }

        // after all allocations, verify every write is still intact
        // (arena never frees individually, so nothing should have been corrupted)
        for &(start, size) in &live {
            unsafe {
                let ptr = start as *mut u8;
                for b in 0..size {
                    let expected = (start as u8).wrapping_add(b as u8);
                    assert_eq!(
                        *ptr.add(b),
                        expected,
                        "CORRUPTION at addr {:#x} byte {}",
                        start,
                        b
                    );
                }
            }
        }
    }

    #[test]
    fn stress_reset_and_refill_cycles() {
        let mut arena = Arena::new();
        let mut rng = Rng::new(12345);

        for _cycle in 0..1000 {
            let mut live: Vec<(usize, usize)> = Vec::new();

            for _ in 0..50 {
                let size = rng.range(1, 512) as usize;
                let align = 1usize << rng.range(0, 6);
                let layout = Layout::from_size_align(size, align).unwrap();
                let ptr = arena.alloc(layout);
                assert!(!ptr.is_null());
                let addr = ptr as usize;

                for &(start, sz) in &live {
                    let overlaps = addr < start + sz && start < addr + size;
                    assert!(!overlaps);
                }
                live.push((addr, size));
            }

            arena.reset();
        }
    }

    #[test]
    fn stress_growth_many_blocks() {
        let mut arena = Arena::new();
        let page = Platform::get_page_size();

        // deliberately force many growth events
        for i in 0..200 {
            let size = page * 3; // bigger than initial block every time
            let layout = Layout::from_size_align(size, 8).unwrap();
            let ptr = arena.alloc(layout);
            assert!(!ptr.is_null(), "failed to grow at iteration {}", i);
        }
    }

    #[test]
    fn stress_extreme_alignments() {
        let mut arena = Arena::new();
        for &align in &[1, 2, 4, 8, 16, 32, 64, 128, 256, 512, 1024, 2048, 4096] {
            let layout = Layout::from_size_align(8, align).unwrap();
            let ptr = arena.alloc(layout);
            assert!(!ptr.is_null());
            assert_eq!(ptr as usize % align, 0);
        }
    }

    #[test]
    fn stress_create_and_drop_many_arenas() {
        for _ in 0..5000 {
            let mut arena = Arena::new();
            let layout = Layout::from_size_align(128, 8).unwrap();
            arena.alloc(layout);
            drop(arena);
        }
    }
}
