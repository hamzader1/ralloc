/// The allocator owns one or more memory blocks obtained from the platform
/// layer. Each block stores a small header at the beginning of the mapping,
/// followed by user allocations. Allocation is bump-style: the arena keeps a
/// cursor inside the current block, aligns that cursor for each request, and
/// advances it after a successful allocation.
///
/// When the current block cannot satisfy a request, the arena maps a new block
/// and links it to the previous block. `reset` keeps the current block and
/// releases older blocks, allowing the arena to cheaply reuse the largest
/// recently needed allocation region. `Drop` releases every mapped block.
///
/// This arena does not run destructors for values stored inside it. Storing
/// plain data and `Copy` values is the intended use. Values such as `String`,
/// `Vec`, `Box`, or other types with ownership of external resources require
/// separate destructor handling, which this allocator does not currently
/// provide.
mod alloc;
mod platform;
use alloc::AllocatorError;
use core::alloc::Layout;
use core::ptr::null_mut;
use platform::Platform;

/// Metadata stored at the beginning of every mapped arena block.
///
/// The header is stored in-band, meaning it lives inside the same memory
/// mapping that later serves user allocations. `prev` links blocks backward so
/// that `reset`, `clear`, and `Drop` can walk and release older mappings.
#[derive(Debug)]
pub struct BlockHeader {
    /// Previous block in the arena chain, or `EMPTY_BLOCK` for the first real
    /// block.
    pub prev: *mut BlockHeader,

    /// Pointer returned by the platform mapping call.
    pub mmap_ptr: *mut u8,

    /// Total size of the mapped block, including the header and user payload
    /// area.
    pub mmap_size: usize,
}

///
/// `current_block` points at the header for the active block.
/// `cursor` points to the next available byte inside that block.
/// `end` points one byte past the active block.
///  Allocations succeed quickly when the aligned cursor and requested size fit before `end`;
///  otherwise the slow path maps and links new block.
#[derive(Debug)]
pub struct Arena {
    /// Header for the block that receives new allocations.
    pub current_block: *mut BlockHeader,

    /// Next allocation position inside the current block.
    pub cursor: *mut u8,

    /// One-past-the-end pointer for the current block.
    pub end: *mut u8,

    /// Controls whether the next growth should double the current block size.
    ///
    /// After `reset`, the arena keeps the current block and disables immediate
    /// doubling for the next growth. This avoids growing too aggressively after
    /// a reset cycle.
    pub double_allowed: bool,
}

/// Wrapper that allows the static empty sentinel to satisfy `Sync`.
///
/// The sentinel is used as a stable block-chain terminator before the arena has
/// mapped any real memory.
pub struct EmptyBlockWrapper(BlockHeader);
unsafe impl Sync for EmptyBlockWrapper {}
unsafe impl Sync for BlockHeader {}

/// Static sentinel used by empty arenas and as the tail of every block chain.
///
/// A newly created arena points at this value instead of mapping memory
/// immediately. The first real allocation replaces it with an in-band
/// `BlockHeader`.
pub static EMPTY_BLOCK: EmptyBlockWrapper = EmptyBlockWrapper(BlockHeader {
    prev: null_mut(),
    mmap_ptr: null_mut(),
    mmap_size: 0,
});

impl EmptyBlockWrapper {
    /// Returns a raw pointer to the sentinel block header.
    pub fn get(&self) -> *mut BlockHeader {
        &self.0 as *const BlockHeader as *mut BlockHeader
    }

    /// Returns the sentinel mapping pointer.
    ///
    /// For the empty sentinel this is null, which makes a fresh arena's
    /// `cursor` and `end` null until the first allocation maps a real block.
    pub fn get_ptr(&self) -> *mut u8 {
        self.0.mmap_ptr
    }
}
impl BlockHeader {
    /// Builds metadata for a freshly mapped block.
    fn new(prev: *mut BlockHeader, mmap_ptr: *mut u8, mmap_size: usize) -> Self {
        Self {
            prev,
            mmap_ptr,
            mmap_size,
        }
    }

    /// Returns the previous block in the chain.
    fn prev(&self) -> *mut BlockHeader {
        self.prev
    }

    /// Returns the base address of this block's mapping.
    fn ptr(&self) -> *mut u8 {
        self.mmap_ptr
    }

    /// Returns the total mapping size for this block.
    fn size(&self) -> usize {
        self.mmap_size
    }
}

impl Arena {
    /// Creates an empty arena without mapping memory.
    ///
    /// The arena starts at `EMPTY_BLOCK`; the first successful allocation maps
    /// the initial real block.
    pub fn new() -> Self {
        Self {
            current_block: EMPTY_BLOCK.get() as *const BlockHeader as *mut BlockHeader,
            cursor: EMPTY_BLOCK.get_ptr(),
            end: EMPTY_BLOCK.get_ptr(),
            double_allowed: true,
        }
    }

    /// Allocates raw memory for `layout`, panicking on allocation failure.
    ///
    /// This is the convenience layer over `try_allocate`. Core allocation
    /// errors are represented by `AllocatorError` in the fallible API, while
    /// this method converts those errors into panics.
    pub fn alloc(&mut self, layout: Layout) -> *mut u8 {
        Self::try_allocate(self, layout).unwrap_or_else(|err| err.panic())
    }

    /// Allocates space for one `T`, moves `val` into arena memory, and returns
    /// a typed raw pointer to it.
    ///
    /// The arena does not currently track destructors, so values that need
    /// `Drop` will not be automatically destroyed by `reset`, `clear`, or
    /// `Drop`.
    pub fn alloc_val<T>(&mut self, val: T) -> *mut T {
        let layout = Layout::new::<T>();
        let ptr = self.alloc(layout) as *mut T;
        unsafe {
            ptr.write(val);
        }
        ptr
    }

    /// Attempts to allocate raw memory for `layout`.
    ///
    /// A zero-sized layout is rejected by this allocator. Non-zero allocations
    /// first try the current block's fast bump path, then fall back to mapping a
    /// new block if the current block has insufficient space.
    pub fn try_allocate(&mut self, layout: Layout) -> Result<*mut u8, AllocatorError> {
        let (size, align) = (layout.size(), layout.align());
        debug_assert!(
            align & (align - 1) == 0,
            "Assertion failed: Alignment ({}) must be a power of two",
            align
        );
        if size == 0 {
            return Err(AllocatorError::ZeroSizedType);
        }

        if let Some(ptr) = Self::try_allocate_fast(self, size, align) {
            Ok(ptr)
        } else {
            Ok(Self::try_allocate_slow(self, size, align)?)
        }
    }

    /// Attempts to satisfy an allocation from the current block only.
    ///
    /// The cursor is rounded up to the requested alignment. If the allocation
    /// fits before `end`, the cursor advances and the aligned pointer is
    /// returned. If not, the method returns `None` so the slow path can grow the
    /// arena.
    ///
    /// ```text
    ///                 Cursor                End                                       Cursor  End
    ///                    |                   |                                          |       |
    ///                    v                   v                                          v       v
    /// +------+-----+-----+-------------------+  Allocate C   +------+-----+-----+-------+---------+
    /// |Header|  A  |  B  |       free        | ------------> |Header|  A  |  B  |   C   |  free   |
    /// +------+-----+-----+-------------------+               +------+-----+-----+-------+---------+
    ///
    /// ^                                      ^
    /// +--------------------------------------+
    ///
    ///       Arena block of 4096 byte
    /// ```
    pub fn try_allocate_fast(&mut self, size: usize, align: usize) -> Option<*mut u8> {
        let aligned_cursor = match Self::align_up(self.cursor as usize, align) {
            Some(ac) => ac,
            _ => AllocatorError::Overflow.panic(),
        };

        let alloc_end = match aligned_cursor.checked_add(size) {
            Some(new_block_size) => new_block_size,
            _ => AllocatorError::Overflow.panic(),
        };
        if alloc_end > self.end as usize {
            return None;
        }
        self.cursor = unsafe { self.cursor.add(alloc_end - self.cursor as usize) };
        unsafe {
            let current_block_ptr = (*self.current_block).ptr();
            Some(current_block_ptr.add(aligned_cursor - current_block_ptr as usize))
        }
    }

    /// Allocates a new block and retries an allocation that did not fit in the
    /// current block.
    ///
    /// The new block is page-aligned in size and large enough for the requested
    /// allocation, the block header, and worst-case alignment padding. When
    /// allowed, growth doubles the previous block size; otherwise it uses the
    /// request size rounded to at least one page.
    ///
    ///
    /*
                                                                                      No enough
                             Cursor      End                                          memory for D
                                |         |    Allocate D                                  |
                                v         v        of                                      v
     +------+-----+-----+-------+---------+    2048 byte   +------+-----+-----+-------+---------+
     |Header|  A  |  B  |   C   |  free   |  ------------> |Header|  A  |  B  |   C   |  free   |
     +------+-----+-----+-------+---------+                +------+-----+-----+-------+---------+
                                     ^                        ^
                                     |                        |               |
                    1024 byte  ------+                        |               | Request new
                                                       +------+               | memory block   End
                                                     Prev                     v                 |
                                                      ptr                                       v
                                                       |   +------+---------+-------------------+
                                                       +---|Header|    D    |       free        |
                                                           +------+---------+-------------------+
                                                                            ^
                                                                            |
                                                                          Cursor
    */
    ///
    ///
    ///
    pub fn try_allocate_slow(
        &mut self,
        size: usize,
        align: usize,
    ) -> Result<*mut u8, AllocatorError> {
        let prev_block_header = self.current_block;
        let prev_block_size = unsafe { (*self.current_block).size() };
        let aligned_requested_size = Self::align_up(
            size + size_of::<BlockHeader>() + (align - 1),
            Platform::get_page_size(),
        )
        .unwrap_or_else(|| AllocatorError::Overflow.panic());
        let new_block_size;
        if self.double_allowed {
            // if double is allowed, then try to double prev block size
            new_block_size = match prev_block_size.checked_mul(2) {
                Some(d) => d.max(aligned_requested_size),
                _ => aligned_requested_size,
            };
        } else {
            new_block_size = aligned_requested_size.max(Platform::get_page_size());
            self.double_allowed = true
        }
        match Self::new_block(self, new_block_size, prev_block_header) {
            Ok(_) => Ok(self.try_allocate_fast(size, align).unwrap()),
            Err(allocerr) => Err(allocerr),
        }
    }

    /// Maps a new block and installs its metadata as the current block.
    fn new_block(
        &mut self,
        new_block_size: usize,
        prev_block_header: *mut BlockHeader,
    ) -> Result<(), AllocatorError> {
        let ptr = Platform::mmap(new_block_size);
        if ptr.is_null() {
            return Err(AllocatorError::OutOfMemory);
        }

        let new_block_header = BlockHeader::new(prev_block_header, ptr, new_block_size);
        self.end = unsafe { ptr.add(new_block_size) };
        self.write_metadata(new_block_header);
        Ok(())
    }

    /// Writes an in-band block header at the beginning of a mapped block.
    ///
    /// After the header is written, the cursor is moved to the first payload
    /// address after the aligned header area, and `current_block` points at the
    /// newly written header.
    fn write_metadata(&mut self, block_header: BlockHeader) {
        let header_ptr = block_header.ptr() as *mut BlockHeader;
        unsafe {
            self.reset_cursor_to(&block_header);
            header_ptr.write(block_header);
            self.current_block = header_ptr;
        }
    }

    /// Releases older blocks and rewinds the current block for reuse.
    ///
    /// The latest block is kept because it is often the largest block reached
    /// by the previous allocation phase. Its `prev` link is reset to
    /// `EMPTY_BLOCK` so later drops do not walk into unmapped memory.
    ///
    ///
    /*
                                  Cursor                            Cursor
                                     |                                 |
                                     v                                 v
     +------+-----+-------+----------+----+  reset   +------+----------+------------------+
     |Header|4KIB | 8KIB  |  16KIB   |free| -------> |Header|  16KIB   |       free       |
     +------+-----+-------+----------+----+          +------+----------+------------------+


    */
    ///
    ///
    ///
    pub fn reset(&mut self) {
        unsafe {
            self.deallocate_blocks_until_stop((*self.current_block).prev(), EMPTY_BLOCK.get());
            self.reset_cursor_to(&*self.current_block);
            (*self.current_block).prev = EMPTY_BLOCK.get();
        }
        self.double_allowed = false;
    }

    /// Releases blocks from `current_block` backward until `stop_block`.
    fn deallocate_blocks_until_stop(
        &mut self,
        current_block: *mut BlockHeader,
        stop_block: *mut BlockHeader,
    ) {
        let mut curr_block = current_block;
        while curr_block != stop_block {
            unsafe {
                let prev = (*curr_block).prev();
                self.dealloc(&*curr_block);
                curr_block = prev;
            }
        }
    }

    /// Unmaps one block.
    fn dealloc(&self, block: &BlockHeader) {
        Platform::munmap(block.ptr(), block.size());
    }

    /// Places the cursor at the first payload byte after a block header.
    fn reset_cursor_to(&mut self, block: &BlockHeader) {
        unsafe {
            self.cursor = block.ptr().add(Self::align_up_unchecked(
                size_of::<BlockHeader>(),
                align_of::<BlockHeader>(),
            ))
        }
    }

    /// Releases every mapped block and returns the arena to the empty sentinel
    /// state.
    pub fn clear(&mut self) {
        self.deallocate_blocks_until_stop(self.current_block, EMPTY_BLOCK.get());
        self.cursor = EMPTY_BLOCK.get_ptr();
        self.end = EMPTY_BLOCK.get_ptr();
    }

    /// Rounds `size` up to the next multiple of `align`.
    ///
    /// `align` must be a power of two. Overflow returns `None`.
    #[inline(always)]
    fn align_up(size: usize, align: usize) -> Option<usize> {
        let checked_cursor_alignment = size.checked_add(align - 1)?;
        Some(checked_cursor_alignment & !(align - 1))
    }

    /// Rounds `size` up to the next multiple of `align` without checking for
    /// overflow.
    ///
    /// This helper is only suitable for small internal metadata sizes where the
    /// arithmetic is known to fit.
    #[inline(always)]
    fn align_up_unchecked(size: usize, align: usize) -> usize {
        (size + align - 1) & !(align - 1)
    }

    /// Returns whether `ptr` points at the most recent allocation of `size`
    /// bytes.
    ///
    /// In a bump allocator, only the most recent allocation can be resized in
    /// place, because only that allocation ends exactly at the cursor.
    #[inline(always)]
    pub fn is_last_allocation(&self, ptr: *mut u8, size: usize) -> bool {
        unsafe { ptr == self.cursor.sub(size) }
    }

    /// Resizes an allocation to a larger layout.
    ///
    /// If the allocation is the last one in the current block and the new size
    /// fits, the cursor is advanced and the same pointer is returned. Otherwise
    /// a new allocation is made, the old bytes are copied into it, and the new
    /// pointer is returned. The old allocation is not individually freed; it
    /// remains part of the arena until `reset`, `clear`, or `Drop`.
    ///
    /*

    Attempts to grow the most recent allocation in place by extending it
    into adjacent free space within the current block. Only possible when
    `ptr` is the last allocation made and the alignment is compatible;
    otherwise falls back to a fresh allocation + copy.

                            Cursor (old)    End
                                |            |
                                v            v
     +------+-----+-----+------+-------------+
     |Header|  A  |  B  |  C   |    free     |
     +------+-----+-----+------+-------------+
                          ^
                          |
                    grow(C, old_size=64, new_size=192)
                    C is the LAST allocation, room available
                          |
                          v
                                            Cursor   End
                                               |      |
                                               v      v
     +------+-----+-----+-----------------------+-----+
     |Header|  A  |  B  |   C (grown in place)  |free |
     +------+-----+-----+-----------------------+-----+
                          ^
                          no copy needed. same pointer returned,
                          cursor just moves further

     if C is NOT the last allocation (e.g. B instead)

     +------+-----+-----+------+-------------+
     |Header|  A  |  B  |  C   |    free     |
     +------+-----+-----+------+-------------+
                   ^
                   grow(B, ...) — something (C) is already allocated
                   after B, cannot extend in place
                   |
                   v
     fallback: allocate fresh block elsewhere, copy B's data, return new ptr

     +------+-----+-----+------+------------------------+-----+
     |Header|  A  |  B  |  C   |   B (grown, relocated) | free|
     +------+-----+-----+------+------------------------+-----+
                   ^ old B bytes now wasted/unreachable
    */
    pub fn grow(&mut self, ptr: *mut u8, old_layout: Layout, new_layout: Layout) -> *mut u8 {
        // check if the alin valid or not
        let is_valid_align = old_layout.align() >= new_layout.align();
        if is_valid_align && self.is_last_allocation(ptr, old_layout.size()) {
            let delta = new_layout.size() - old_layout.size();
            if self.try_allocate_fast(delta, old_layout.align()).is_some() {
                return ptr;
            }
        }
        unsafe {
            let new_ptr = self.alloc(new_layout);
            core::ptr::copy_nonoverlapping(ptr, new_ptr, old_layout.size());
            new_ptr
        }
    }

    /// Resizes an allocation to a smaller layout.
    ///
    /// If the allocation is the last one in the current block and the new
    /// layout has compatible alignment, the cursor moves backward and the
    /// released tail space becomes available for future allocations. If the
    /// allocation is not last, shrinking is a logical no-op for the arena and
    /// memory is reclaimed only by `reset`, `clear`, or `Drop`.
    ///
    /*
                                            Cursor   End
                                               |      |
                                               v      v
    +------+-----+-----+-----------------------+------+
    |Header|  A  |  B  |   C (size=192)        | free |
    +------+-----+-----+-----------------------+------+
             ^
             |
       shrink(C, old_size=192, new_size=64)
       C is the LAST allocation
             |
             v
                            Cursor (new)               End
                              |                         |
                              v                         v
    +------+-----+-----+------+-------------------------+
    |Header|  A  |  B  |  C   |         free            |
    +------+-----+-----+------+-------------------------+
             ^
             same pointer returned, cursor pulled back,
             reclaimed tail is usable free space again


    */
    ///

    pub fn shrink(&mut self, ptr: *mut u8, old_layout: Layout, new_layout: Layout) -> *mut u8 {
        let is_valid_to_shrink =
            new_layout.size() <= old_layout.size() && old_layout.align() >= new_layout.align();

        if is_valid_to_shrink && self.is_last_allocation(ptr, old_layout.size()) {
            let delta = old_layout.size() - new_layout.size();

            unsafe {
                let new_cursor = self.cursor.sub(delta);
                debug_assert!(
                    new_cursor as usize >= (*self.current_block).mmap_ptr as usize,
                    "shrink would move cursor before the start of the current block"
                );
                self.cursor = new_cursor;
            }
        }

        ptr
    }
}

impl Drop for Arena {
    /// Releases every mapped block owned by the arena.
    fn drop(&mut self) {
        let mut current = self.current_block;
        unsafe {
            while current != EMPTY_BLOCK.get() {
                let current_block = core::ptr::read(current);
                Platform::munmap(current_block.ptr(), current_block.size());
                current = current_block.prev;
            }
        }
    }
}

impl std::default::Default for Arena {
    /// Creates an empty arena.
    fn default() -> Self {
        Self::new()
    }
}
#[cfg(test)]
mod tests {
    use crate::alloc::AllocatorError;
    use crate::{Arena, BlockHeader, Platform, EMPTY_BLOCK};
    use std::alloc::Layout;

    #[test]
    fn new_starts_at_empty_block_without_mapping_memory() {
        let arena = Arena::new();

        assert_eq!(arena.current_block, EMPTY_BLOCK.get());
        assert!(arena.cursor.is_null());
        assert!(arena.end.is_null());
    }

    #[test]
    fn first_alloc_creates_block_and_writes_header() {
        let mut arena = Arena::new();
        let layout = Layout::from_size_align(16, 8).unwrap();

        let ptr = arena.alloc(layout);
        let block = unsafe { &*arena.current_block };

        assert!(!ptr.is_null());
        assert_ne!(arena.current_block, EMPTY_BLOCK.get());
        assert_eq!(arena.current_block as *mut u8, block.mmap_ptr);
        assert_eq!(block.prev, EMPTY_BLOCK.get());
        assert!(block.mmap_size >= Platform::get_page_size());
        assert_eq!(ptr as usize % 8, 0);
        assert_eq!(arena.cursor, unsafe { ptr.add(16) });
        assert_eq!(arena.end, unsafe { block.mmap_ptr.add(block.mmap_size) });
    }

    #[test]
    fn grow_links_new_block_to_previous_block() {
        let mut arena = Arena::new();
        arena.alloc(Layout::from_size_align(8, 8).unwrap());
        let first_block = arena.current_block;

        let huge = Layout::from_size_align(Platform::get_page_size() * 4, 8).unwrap();
        arena.alloc(huge);

        let second_block = arena.current_block;
        assert_ne!(first_block, second_block);
        assert_eq!(unsafe { (*second_block).prev }, first_block);
        assert_eq!(unsafe { (*first_block).prev }, EMPTY_BLOCK.get());
    }

    #[test]
    fn try_allocate_rejects_zero_sized_layout() {
        let mut arena = Arena::new();
        let layout = Layout::from_size_align(0, 8).unwrap();

        assert_eq!(
            arena.try_allocate(layout),
            Err(AllocatorError::ZeroSizedType)
        );
    }

    #[test]
    #[should_panic(expected = "cannot allocate a zero-sized type")]
    fn alloc_panics_on_zero_sized_layout() {
        let mut arena = Arena::new();
        let layout = Layout::from_size_align(0, 8).unwrap();

        arena.alloc(layout);
    }

    #[test]
    fn consecutive_allocs_do_not_overlap() {
        let mut arena = Arena::new();
        let layout = Layout::from_size_align(24, 8).unwrap();

        let a = arena.alloc(layout);
        let b = arena.alloc(layout);

        assert!(!a.is_null() && !b.is_null());
        assert_ne!(a, b);
        assert!(b as usize >= a as usize + 24);
    }

    #[test]
    fn alloc_respects_alignment() {
        let mut arena = Arena::new();
        arena.alloc(Layout::from_size_align(3, 1).unwrap());

        let layout = Layout::from_size_align(32, 32).unwrap();
        let ptr = arena.alloc(layout);

        assert_eq!(ptr as usize % 32, 0);
    }

    #[test]
    fn alloc_never_writes_past_end() {
        let mut arena = Arena::new();
        let layout = Layout::from_size_align(64, 8).unwrap();

        for _ in 0..1000 {
            let ptr = arena.alloc(layout);
            assert!(!ptr.is_null());
            assert!(unsafe { ptr.add(64) } as usize <= arena.end as usize);
        }
    }

    #[test]
    fn alloc_triggers_grow_when_current_block_is_full() {
        let mut arena = Arena::new();
        arena.alloc(Layout::from_size_align(8, 8).unwrap());
        let first_block = arena.current_block;
        let page_size = Platform::get_page_size();

        let filler = Layout::from_size_align(page_size, 8).unwrap();
        arena.alloc(filler);

        assert_ne!(arena.current_block, first_block);
    }

    #[test]
    fn grow_chunk_size_at_least_fits_request() {
        let mut arena = Arena::new();
        let page_size = Platform::get_page_size();
        let requested = page_size * 10;

        let layout = Layout::from_size_align(requested, 8).unwrap();
        let ptr = arena.alloc(layout);

        assert!(!ptr.is_null());
        let new_block_size = unsafe { (*arena.current_block).mmap_size };
        assert!(new_block_size >= requested + size_of::<BlockHeader>());
    }

    #[test]
    fn grow_doubles_when_request_is_small_after_initial_block_exists() {
        let mut arena = Arena::new();
        arena.alloc(Layout::from_size_align(8, 8).unwrap());
        let first_size = unsafe { (*arena.current_block).mmap_size };

        let filler = Layout::from_size_align(first_size, 8).unwrap();
        arena.alloc(filler);

        let new_size = unsafe { (*arena.current_block).mmap_size };
        assert_eq!(new_size, first_size * 2);
    }

    #[test]
    fn alloc_cursor_accounts_for_alignment_padding() {
        let mut arena = Arena::new();

        let odd_layout = Layout::from_size_align(3, 1).unwrap();
        let odd_ptr = arena.alloc(odd_layout);
        assert!(!odd_ptr.is_null());

        let cursor_after_odd = arena.cursor as usize;
        let big_align_layout = Layout::from_size_align(64, 32).unwrap();
        let big_ptr = arena.alloc(big_align_layout);

        let expected_aligned = Arena::align_up(cursor_after_odd, 32).unwrap();
        let expected_new_cursor = expected_aligned + 64;

        assert_eq!(big_ptr as usize, expected_aligned);
        assert_eq!(big_ptr as usize % 32, 0);
        assert_eq!(arena.cursor as usize, expected_new_cursor);
        assert!(odd_ptr as usize + 3 <= big_ptr as usize);
        assert!(expected_aligned > cursor_after_odd);
    }

    #[test]
    fn alloc_val_writes_value_into_arena_memory() {
        let mut arena = Arena::new();

        let ptr = arena.alloc_val(1234_u64);

        assert!(!ptr.is_null());
        assert_eq!(ptr as usize % align_of::<u64>(), 0);
        assert_eq!(unsafe { *ptr }, 1234);
    }

    #[test]
    fn reset_keeps_current_block_and_rewinds_cursor() {
        let mut arena = Arena::new();
        arena.alloc(Layout::from_size_align(8, 8).unwrap());
        let block = arena.current_block;
        let block_ptr = unsafe { (*block).mmap_ptr };
        let expected_cursor = unsafe {
            block_ptr.add(Arena::align_up_unchecked(
                size_of::<BlockHeader>(),
                align_of::<BlockHeader>(),
            ))
        };

        arena.alloc(Layout::from_size_align(128, 8).unwrap());
        arena.reset();

        assert_eq!(arena.current_block, block);
        assert_eq!(arena.cursor, expected_cursor);
        assert_eq!(unsafe { (*arena.current_block).prev }, EMPTY_BLOCK.get());
    }

    #[test]
    fn reset_after_growth_keeps_latest_block() {
        let mut arena = Arena::new();
        arena.alloc(Layout::from_size_align(8, 8).unwrap());
        let first_block = arena.current_block;

        let huge = Layout::from_size_align(Platform::get_page_size() * 4, 8).unwrap();
        arena.alloc(huge);
        let latest_block = arena.current_block;

        arena.reset();

        assert_ne!(first_block, latest_block);
        assert_eq!(arena.current_block, latest_block);
        assert_eq!(unsafe { (*latest_block).prev }, EMPTY_BLOCK.get());
    }

    #[test]
    fn reset_allows_refill_without_overlap_inside_surviving_block() {
        let mut arena = Arena::new();
        let first = arena.alloc(Layout::from_size_align(64, 8).unwrap());

        arena.reset();
        let second = arena.alloc(Layout::from_size_align(64, 8).unwrap());

        assert_eq!(first, second);
    }

    #[test]
    fn drop_handles_empty_arena() {
        let arena = Arena::new();
        drop(arena);
    }

    #[test]
    fn drop_handles_single_mapped_block() {
        let mut arena = Arena::new();
        arena.alloc(Layout::from_size_align(64, 8).unwrap());
        drop(arena);
    }

    #[test]
    fn drop_handles_multiple_blocks() {
        let mut arena = Arena::new();
        let page = Platform::get_page_size();

        for _ in 0..5 {
            let layout = Layout::from_size_align(page * 2, 8).unwrap();
            let ptr = arena.alloc(layout);
            assert!(!ptr.is_null());
        }

        drop(arena);
    }

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

    fn chain_len(arena: &Arena) -> usize {
        let mut len = 0;
        let mut current = arena.current_block;

        unsafe {
            while current != EMPTY_BLOCK.get() {
                len += 1;
                current = (*current).prev;
            }
        }

        len
    }

    fn assert_no_overlap(live: &[(usize, usize)], addr: usize, size: usize) {
        for &(start, sz) in live {
            let overlaps = addr < start + sz && start < addr + size;
            assert!(!overlaps);
        }
    }

    #[test]
    fn stress_random_allocs_no_overlap() {
        let mut arena = Arena::new();
        let mut rng = Rng::new(0xDEADBEEF);
        let mut live: Vec<(usize, usize)> = Vec::new();

        let iteration = if cfg!(miri) { 500 } else { 100_000 };
        for i in 0..iteration {
            let size = rng.range(1, 4096) as usize;
            let align = 1usize << rng.range(0, 7);
            let layout = Layout::from_size_align(size, align).unwrap();
            let ptr = arena.alloc(layout);
            let addr = ptr as usize;

            assert!(!ptr.is_null(), "alloc failed at iteration {}", i);
            assert_eq!(addr % align, 0, "misaligned at iter {}: align={}", i, align);

            for &(start, sz) in &live {
                let overlaps = addr < start + sz && start < addr + size;
                assert!(
                    !overlaps,
                    "OVERLAP at iter {}: new=({:#x},{}) vs existing=({:#x},{})",
                    i, addr, size, start, sz
                );
            }

            unsafe {
                for b in 0..size {
                    *ptr.add(b) = (addr as u8).wrapping_add(b as u8);
                }
            }

            live.push((addr, size));
        }

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

            for _ in 0..8 {
                let size = rng.range(1, 64) as usize;
                let align = 1usize << rng.range(0, 6);
                let layout = Layout::from_size_align(size, align).unwrap();
                let ptr = arena.alloc(layout);
                let addr = ptr as usize;

                assert!(!ptr.is_null());
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

        for i in 0..200 {
            let size = page * 3;
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
            arena.alloc(Layout::from_size_align(128, 8).unwrap());
            drop(arena);
        }
    }

    #[test]
    fn alloc_val_many_values_preserve_contents() {
        let mut arena = Arena::new();
        let a = arena.alloc_val(1_u8);
        let b = arena.alloc_val(2_u16);
        let c = arena.alloc_val(3_u32);
        let d = arena.alloc_val(4_u64);
        let e = arena.alloc_val([5_u32; 16]);

        assert_eq!(unsafe { *a }, 1);
        assert_eq!(unsafe { *b }, 2);
        assert_eq!(unsafe { *c }, 3);
        assert_eq!(unsafe { *d }, 4);
        assert_eq!(unsafe { *e }, [5_u32; 16]);
    }

    #[test]
    fn alloc_val_respects_struct_alignment() {
        #[repr(align(64))]
        struct Wide(u8);

        let mut arena = Arena::new();
        let ptr = arena.alloc_val(Wide(9));

        assert_eq!(ptr as usize % 64, 0);
        assert_eq!(unsafe { (*ptr).0 }, 9);
    }

    #[test]
    fn reset_reuses_latest_block_start() {
        let mut arena = Arena::new();
        let page = Platform::get_page_size();

        arena.alloc(Layout::from_size_align(8, 8).unwrap());
        arena.alloc(Layout::from_size_align(page * 4, 8).unwrap());
        let block = arena.current_block;
        let mmap_ptr = unsafe { (*block).mmap_ptr };
        let expected = unsafe {
            mmap_ptr.add(Arena::align_up_unchecked(
                size_of::<BlockHeader>(),
                align_of::<BlockHeader>(),
            ))
        };

        arena.reset();
        let ptr = arena.alloc(Layout::from_size_align(32, 8).unwrap());

        assert_eq!(arena.current_block, block);
        assert_eq!(ptr, expected);
    }

    #[test]
    fn reset_after_many_growths_keeps_one_block() {
        let mut arena = Arena::new();
        let page = Platform::get_page_size();

        for i in 1..16 {
            arena.alloc(Layout::from_size_align(page * i, 8).unwrap());
        }

        assert!(chain_len(&arena) > 1);
        let latest = arena.current_block;
        arena.reset();

        assert_eq!(arena.current_block, latest);
        assert_eq!(chain_len(&arena), 1);
        assert_eq!(unsafe { (*arena.current_block).prev }, EMPTY_BLOCK.get());
    }

    #[test]
    fn reset_after_many_growths_then_drop_is_safe() {
        let mut arena = Arena::new();
        let page = Platform::get_page_size();

        for i in 1..24 {
            arena.alloc(Layout::from_size_align(page * i, 16).unwrap());
        }

        arena.reset();
        drop(arena);
    }

    #[test]
    fn block_chain_lengths_increase_on_growth() {
        let mut arena = Arena::new();
        let page = Platform::get_page_size();

        assert_eq!(chain_len(&arena), 0);
        arena.alloc(Layout::from_size_align(8, 8).unwrap());
        assert_eq!(chain_len(&arena), 1);
        arena.alloc(Layout::from_size_align(page * 2, 8).unwrap());
        assert_eq!(chain_len(&arena), 2);
        arena.alloc(Layout::from_size_align(page * 8, 8).unwrap());
        assert_eq!(chain_len(&arena), 3);
    }

    #[test]
    fn block_chain_links_end_at_empty_block() {
        let mut arena = Arena::new();
        let page = Platform::get_page_size();

        for i in 1..12 {
            arena.alloc(Layout::from_size_align(page * i, 8).unwrap());
        }

        let mut current = arena.current_block;

        unsafe {
            while (*current).prev != EMPTY_BLOCK.get() {
                current = (*current).prev;
            }

            assert_eq!((*current).prev, EMPTY_BLOCK.get());
        }
    }

    #[test]
    fn repeated_reset_reuses_same_large_block() {
        let mut arena = Arena::new();
        let page = Platform::get_page_size();

        arena.alloc(Layout::from_size_align(page * 8, 8).unwrap());
        let block = arena.current_block;

        for _ in 0..1000 {
            arena.reset();
            arena.alloc(Layout::from_size_align(256, 8).unwrap());
            assert_eq!(arena.current_block, block);
            assert_eq!(chain_len(&arena), 1);
        }
    }

    #[test]
    fn alternating_small_and_large_allocations_do_not_overlap() {
        let mut arena = Arena::new();
        let page = Platform::get_page_size();
        let mut live = Vec::new();

        for i in 0..512 {
            let size = if i % 7 == 0 { page + i } else { (i % 251) + 1 };
            let align = 1usize << (i % 12);
            let ptr = arena.alloc(Layout::from_size_align(size, align).unwrap());
            let addr = ptr as usize;

            assert_eq!(addr % align, 0);
            assert_no_overlap(&live, addr, size);
            live.push((addr, size));
        }
    }

    #[test]
    fn all_power_of_two_alignments_survive_many_rounds() {
        let mut arena = Arena::new();

        for round in 0..256 {
            for pow in 0..13 {
                let align = 1usize << pow;
                let size = round + pow + 1;
                let ptr = arena.alloc(Layout::from_size_align(size, align).unwrap());

                assert_eq!(ptr as usize % align, 0);
                assert!(unsafe { ptr.add(size) } as usize <= arena.end as usize);
            }
        }
    }

    #[test]
    fn exact_page_requests_remain_inside_blocks() {
        let mut arena = Arena::new();
        let page = Platform::get_page_size();

        for i in 1..64 {
            let size = page * i;
            let ptr = arena.alloc(Layout::from_size_align(size, 8).unwrap());

            assert!(unsafe { ptr.add(size) } as usize <= arena.end as usize);
        }
    }

    #[test]
    fn large_alignment_with_small_size_remains_valid() {
        let mut arena = Arena::new();

        for &align in &[4096, 8192, 16384, 32768] {
            let ptr = arena.alloc(Layout::from_size_align(1, align).unwrap());

            assert_eq!(ptr as usize % align, 0);
            assert!(ptr as usize >= unsafe { (*arena.current_block).mmap_ptr } as usize);
            assert!((ptr as usize) < arena.end as usize);
        }
    }

    #[test]
    fn byte_patterns_survive_many_growths() {
        let mut arena = Arena::new();
        let page = Platform::get_page_size();
        let mut live = Vec::new();

        for i in 0..512 {
            let size = if i % 13 == 0 {
                page + i
            } else {
                (i % 1024) + 1
            };
            let align = 1usize << (i % 8);
            let ptr = arena.alloc(Layout::from_size_align(size, align).unwrap());
            let seed = i as u8;

            unsafe {
                for offset in 0..size {
                    *ptr.add(offset) = seed.wrapping_add(offset as u8);
                }
            }

            live.push((ptr, size, seed));
        }

        for &(ptr, size, seed) in &live {
            unsafe {
                for offset in 0..size {
                    assert_eq!(*ptr.add(offset), seed.wrapping_add(offset as u8));
                }
            }
        }
    }

    #[test]
    fn random_reset_growth_and_refill_cycles() {
        let mut arena = Arena::new();
        let mut rng = Rng::new(0xBAD5EED);
        let page = Platform::get_page_size();

        for cycle in 0..300 {
            let mut live = Vec::new();
            let count = if cycle % 5 == 0 { 80 } else { 24 };

            for _ in 0..count {
                let size = if rng.range(0, 10) == 0 {
                    page + rng.range(0, page as u64) as usize
                } else {
                    rng.range(1, 1024) as usize
                };
                let align = 1usize << rng.range(0, 12);
                let ptr = arena.alloc(Layout::from_size_align(size, align).unwrap());
                let addr = ptr as usize;

                assert_eq!(addr % align, 0);
                assert_no_overlap(&live, addr, size);
                live.push((addr, size));
            }

            arena.reset();
            assert_eq!(unsafe { (*arena.current_block).prev }, EMPTY_BLOCK.get());
            assert_eq!(chain_len(&arena), 1);
        }
    }

    #[test]
    fn many_arenas_each_grow_reset_and_drop() {
        let page = Platform::get_page_size();

        for i in 0..512 {
            let mut arena = Arena::new();

            for j in 0..8 {
                let size = page + ((i + j) % 97);
                arena.alloc(Layout::from_size_align(size, 8).unwrap());
            }

            arena.reset();
            arena.alloc(Layout::from_size_align(128, 16).unwrap());
            drop(arena);
        }
    }

    #[test]
    fn try_allocate_matches_alloc_for_successful_layouts() {
        let mut arena = Arena::new();

        for i in 1..2048 {
            let align = 1usize << (i % 10);
            let layout = Layout::from_size_align(i, align).unwrap();
            let ptr = arena.try_allocate(layout).unwrap();

            assert!(!ptr.is_null());
            assert_eq!(ptr as usize % align, 0);
        }
    }

    #[test]
    fn cursor_equals_end_of_last_allocation_for_dense_alignments() {
        let mut arena = Arena::new();

        for size in 1..256 {
            let ptr = arena.alloc(Layout::from_size_align(size, 1).unwrap());

            assert_eq!(arena.cursor, unsafe { ptr.add(size) });
        }
    }

    #[test]
    fn cursor_advances_by_padding_plus_size() {
        let mut arena = Arena::new();

        for align in [2, 4, 8, 16, 32, 64, 128, 256] {
            arena.alloc(Layout::from_size_align(3, 1).unwrap());
            let before = arena.cursor as usize;
            let ptr = arena.alloc(Layout::from_size_align(17, align).unwrap());
            let aligned = Arena::align_up(before, align).unwrap();

            assert_eq!(ptr as usize, aligned);
            assert_eq!(arena.cursor as usize, aligned + 17);
        }
    }

    #[test]
    fn reset_then_large_allocation_can_grow_again() {
        let mut arena = Arena::new();
        let page = Platform::get_page_size();

        arena.alloc(Layout::from_size_align(page * 2, 8).unwrap());
        arena.reset();
        let kept = arena.current_block;
        arena.alloc(Layout::from_size_align(page * 16, 8).unwrap());

        assert_ne!(arena.current_block, kept);
        assert_eq!(unsafe { (*arena.current_block).prev }, kept);
    }

    #[test]
    fn reset_then_small_allocations_do_not_force_growth() {
        let mut arena = Arena::new();
        let page = Platform::get_page_size();

        arena.alloc(Layout::from_size_align(page * 4, 8).unwrap());
        let kept = arena.current_block;
        arena.reset();

        for _ in 0..256 {
            arena.alloc(Layout::from_size_align(16, 8).unwrap());
            assert_eq!(arena.current_block, kept);
        }
    }

    #[test]
    fn alloc_val_after_reset_uses_rewound_space() {
        let mut arena = Arena::new();
        let first = arena.alloc_val(11_u64);

        arena.reset();
        let second = arena.alloc_val(22_u64);

        assert_eq!(first, second);
        assert_eq!(unsafe { *second }, 22);
    }

    #[test]
    fn stress_random_layouts_across_many_seeds() {
        for seed in 0..32 {
            let mut arena = Arena::new();
            let mut rng = Rng::new(0xA11C_A7E0 ^ seed);
            let mut live = Vec::new();

            for _ in 0..2048 {
                let size = rng.range(1, 8192) as usize;
                let align = 1usize << rng.range(0, 13);
                let ptr = arena.alloc(Layout::from_size_align(size, align).unwrap());
                let addr = ptr as usize;

                assert_eq!(addr % align, 0);
                assert!(unsafe { ptr.add(size) } as usize <= arena.end as usize);
                assert_no_overlap(&live, addr, size);
                live.push((addr, size));
            }
        }
    }

    #[test]
    fn stress_random_byte_patterns_across_many_seeds() {
        for seed in 0..16 {
            let mut arena = Arena::new();
            let mut rng = Rng::new(0xC0FFEE ^ seed);
            let mut live = Vec::new();

            for i in 0..512 {
                let size = rng.range(1, 2048) as usize;
                let align = 1usize << rng.range(0, 10);
                let ptr = arena.alloc(Layout::from_size_align(size, align).unwrap());
                let byte = (i as u8).wrapping_mul(17).wrapping_add(seed as u8);

                unsafe {
                    for offset in 0..size {
                        *ptr.add(offset) = byte.wrapping_add(offset as u8);
                    }
                }

                live.push((ptr, size, byte));
            }

            for &(ptr, size, byte) in &live {
                unsafe {
                    for offset in 0..size {
                        assert_eq!(*ptr.add(offset), byte.wrapping_add(offset as u8));
                    }
                }
            }
        }
    }

    #[test]
    fn stress_reset_growth_reset_growth_cycles() {
        let mut arena = Arena::new();
        let page = Platform::get_page_size();

        for cycle in 0..512 {
            let large = page * ((cycle % 9) + 1);
            arena.alloc(Layout::from_size_align(large, 8).unwrap());
            let kept = arena.current_block;
            arena.reset();

            assert_eq!(arena.current_block, kept);
            assert_eq!(chain_len(&arena), 1);
            assert_eq!(unsafe { (*arena.current_block).prev }, EMPTY_BLOCK.get());

            for i in 0..32 {
                let size = ((cycle + i) % 257) + 1;
                let align = 1usize << ((cycle + i) % 9);
                let ptr = arena.alloc(Layout::from_size_align(size, align).unwrap());
                assert_eq!(ptr as usize % align, 0);
            }
        }
    }

    #[test]
    fn stress_chain_metadata_matches_block_bounds() {
        let mut arena = Arena::new();
        let page = Platform::get_page_size();

        for i in 1..80 {
            let size = page * ((i % 11) + 1);
            let align = 1usize << (i % 12);
            arena.alloc(Layout::from_size_align(size, align).unwrap());
        }

        let mut current = arena.current_block;

        unsafe {
            while current != EMPTY_BLOCK.get() {
                let block = &*current;
                assert_eq!(current as *mut u8, block.mmap_ptr);
                assert!(block.mmap_size >= Platform::get_page_size());
                assert_eq!(block.mmap_size % Platform::get_page_size(), 0);
                current = block.prev;
            }
        }
    }

    #[test]
    fn stress_allocations_at_page_boundaries() {
        let mut arena = Arena::new();
        let page = Platform::get_page_size();

        for pages in 1..48 {
            for delta in [0usize, 1, 2, 7, 15, 31, 63, 127, 255] {
                let size = page * pages + delta;
                let align = 1usize << (delta % 13);
                let ptr = arena.alloc(Layout::from_size_align(size, align).unwrap());

                assert_eq!(ptr as usize % align, 0);
                assert!(unsafe { ptr.add(size) } as usize <= arena.end as usize);
            }
        }
    }

    #[test]
    fn stress_allocations_just_below_page_boundaries() {
        let mut arena = Arena::new();
        let page = Platform::get_page_size();

        for pages in 1..64 {
            for delta in [1usize, 2, 3, 8, 16, 64, 256, 1024] {
                let size = page * pages - delta.min(page * pages - 1);
                let align = 1usize << (pages % 13);
                let ptr = arena.alloc(Layout::from_size_align(size, align).unwrap());

                assert_eq!(ptr as usize % align, 0);
                assert!(unsafe { ptr.add(size) } as usize <= arena.end as usize);
            }
        }
    }

    #[test]
    fn stress_alloc_val_many_arrays() {
        let mut arena = Arena::new();
        let mut ptrs = Vec::new();

        for i in 0..4096 {
            let ptr = arena.alloc_val([i as u64; 8]);
            ptrs.push((ptr, i as u64));
        }

        for &(ptr, value) in &ptrs {
            assert_eq!(unsafe { *ptr }, [value; 8]);
        }
    }

    #[test]
    fn stress_alloc_val_mixed_aligned_types() {
        #[repr(align(128))]
        struct A(u64);

        #[repr(align(256))]
        struct B([u8; 33]);

        let mut arena = Arena::new();
        let mut a_ptrs = Vec::new();
        let mut b_ptrs = Vec::new();

        for i in 0..512 {
            let a = arena.alloc_val(A(i));
            let b = arena.alloc_val(B([i as u8; 33]));
            assert_eq!(a as usize % 128, 0);
            assert_eq!(b as usize % 256, 0);
            a_ptrs.push((a, i));
            b_ptrs.push((b, i as u8));
        }

        for &(ptr, value) in &a_ptrs {
            assert_eq!(unsafe { (*ptr).0 }, value);
        }

        for &(ptr, value) in &b_ptrs {
            assert_eq!(unsafe { (*ptr).0 }, [value; 33]);
        }
    }

    #[test]
    fn stress_reset_with_alignment_sensitive_refills() {
        let mut arena = Arena::new();
        let page = Platform::get_page_size();

        arena.alloc(Layout::from_size_align(page * 16, 4096).unwrap());

        for cycle in 0..1024 {
            arena.reset();

            for pow in 0..13 {
                let align = 1usize << pow;
                let size = ((cycle + pow) % 129) + 1;
                let ptr = arena.alloc(Layout::from_size_align(size, align).unwrap());

                assert_eq!(ptr as usize % align, 0);
                assert!(unsafe { ptr.add(size) } as usize <= arena.end as usize);
            }
        }
    }

    #[test]
    fn stress_many_empty_arenas() {
        for _ in 0..50_000 {
            let arena = Arena::new();
            assert_eq!(arena.current_block, EMPTY_BLOCK.get());
            drop(arena);
        }
    }

    #[test]
    fn stress_many_small_arenas_with_one_allocation() {
        for i in 0..20_000 {
            let mut arena = Arena::new();
            let align = 1usize << (i % 8);
            let ptr = arena.alloc(Layout::from_size_align((i % 128) + 1, align).unwrap());

            assert_eq!(ptr as usize % align, 0);
            drop(arena);
        }
    }

    #[test]
    fn stress_many_arenas_with_many_small_allocations() {
        for seed in 0..256 {
            let mut arena = Arena::new();
            let mut rng = Rng::new(seed);
            let mut live = Vec::new();

            for _ in 0..128 {
                let size = rng.range(1, 512) as usize;
                let align = 1usize << rng.range(0, 9);
                let ptr = arena.alloc(Layout::from_size_align(size, align).unwrap());
                let addr = ptr as usize;

                assert_eq!(addr % align, 0);
                assert_no_overlap(&live, addr, size);
                live.push((addr, size));
            }
        }
    }

    #[test]
    fn stress_sparse_writes_do_not_corrupt_neighbors() {
        let mut arena = Arena::new();
        let mut ranges = Vec::new();

        for i in 0..4096 {
            let size = (i % 2048) + 1;
            let align = 1usize << (i % 10);
            let ptr = arena.alloc(Layout::from_size_align(size, align).unwrap());
            let first = i as u8;
            let last = first.wrapping_mul(3);

            unsafe {
                *ptr = first;
                *ptr.add(size - 1) = last;
            }

            ranges.push((ptr, size, first, last));
        }

        for &(ptr, size, first, last) in &ranges {
            unsafe {
                assert_eq!(*ptr, first);
                assert_eq!(*ptr.add(size - 1), last);
            }
        }
    }

    #[test]
    fn stress_cursor_is_always_within_current_block() {
        let mut arena = Arena::new();
        let mut rng = Rng::new(0x1234_5678);

        for _ in 0..50_000 {
            let size = rng.range(1, 4096) as usize;
            let align = 1usize << rng.range(0, 13);
            arena.alloc(Layout::from_size_align(size, align).unwrap());

            unsafe {
                let block = &*arena.current_block;
                assert!(arena.cursor as usize >= block.mmap_ptr as usize);
                assert!(arena.cursor as usize <= arena.end as usize);
                assert_eq!(arena.end, block.mmap_ptr.add(block.mmap_size));
            }
        }
    }

    #[test]
    fn stress_reset_cursor_is_block_payload_start() {
        let mut arena = Arena::new();
        let page = Platform::get_page_size();

        for pages in 1..128 {
            arena.alloc(Layout::from_size_align(page * pages, 8).unwrap());
            arena.reset();

            let block = unsafe { &*arena.current_block };
            let expected = unsafe {
                block.mmap_ptr.add(Arena::align_up_unchecked(
                    size_of::<BlockHeader>(),
                    align_of::<BlockHeader>(),
                ))
            };

            assert_eq!(arena.cursor, expected);
            assert_eq!(unsafe { (*arena.current_block).prev }, EMPTY_BLOCK.get());
        }
    }

    #[test]
    fn stress_try_allocate_errors_do_not_change_empty_arena() {
        let mut arena = Arena::new();

        for align in [1, 2, 4, 8, 16, 32, 64, 128] {
            let layout = Layout::from_size_align(0, align).unwrap();
            assert_eq!(
                arena.try_allocate(layout),
                Err(AllocatorError::ZeroSizedType)
            );
            assert_eq!(arena.current_block, EMPTY_BLOCK.get());
            assert!(arena.cursor.is_null());
            assert!(arena.end.is_null());
        }
    }

    #[test]
    fn stress_try_allocate_errors_do_not_change_non_empty_arena() {
        let mut arena = Arena::new();
        arena.alloc(Layout::from_size_align(64, 8).unwrap());
        let block = arena.current_block;
        let cursor = arena.cursor;
        let end = arena.end;

        for align in [1, 2, 4, 8, 16, 32, 64, 128] {
            let layout = Layout::from_size_align(0, align).unwrap();
            assert_eq!(
                arena.try_allocate(layout),
                Err(AllocatorError::ZeroSizedType)
            );
            assert_eq!(arena.current_block, block);
            assert_eq!(arena.cursor, cursor);
            assert_eq!(arena.end, end);
        }
    }

    #[test]
    fn stress_growth_sizes_are_page_aligned() {
        let mut arena = Arena::new();
        let page = Platform::get_page_size();

        for i in 1..256 {
            let size = page + i * 37;
            arena.alloc(Layout::from_size_align(size, 8).unwrap());
            let block = unsafe { &*arena.current_block };

            assert_eq!(block.mmap_size % page, 0);
            assert!(block.mmap_size >= size + size_of::<BlockHeader>());
        }
    }

    #[test]
    fn stress_many_resets_keep_growth_control_valid() {
        let mut arena = Arena::new();
        let page = Platform::get_page_size();

        arena.alloc(Layout::from_size_align(page * 32, 8).unwrap());
        let large_block = arena.current_block;

        for _ in 0..512 {
            arena.reset();
            arena.alloc(Layout::from_size_align(page, 8).unwrap());
            assert_eq!(arena.current_block, large_block);
        }

        arena.reset();
        arena.alloc(Layout::from_size_align(page * 64, 8).unwrap());

        assert_ne!(arena.current_block, large_block);
    }

    #[test]
    fn grow_last_allocation_in_place_returns_same_pointer() {
        let mut arena = Arena::new();
        let old = Layout::from_size_align(64, 8).unwrap();
        let new = Layout::from_size_align(128, 8).unwrap();
        let ptr = arena.alloc(old);
        let block = arena.current_block;

        let grown = arena.grow(ptr, old, new);

        assert_eq!(grown, ptr);
        assert_eq!(arena.current_block, block);
        assert_eq!(arena.cursor, unsafe { ptr.add(128) });
    }

    #[test]
    fn grow_last_allocation_preserves_existing_bytes() {
        let mut arena = Arena::new();
        let old = Layout::from_size_align(128, 8).unwrap();
        let new = Layout::from_size_align(512, 8).unwrap();
        let ptr = arena.alloc(old);

        unsafe {
            for i in 0..old.size() {
                *ptr.add(i) = i as u8;
            }
        }

        let grown = arena.grow(ptr, old, new);

        assert_eq!(grown, ptr);
        unsafe {
            for i in 0..old.size() {
                assert_eq!(*grown.add(i), i as u8);
            }
        }
    }

    #[test]
    fn grow_non_last_allocation_moves_and_preserves_bytes() {
        let mut arena = Arena::new();
        let old = Layout::from_size_align(128, 8).unwrap();
        let new = Layout::from_size_align(512, 8).unwrap();
        let first = arena.alloc(old);
        let blocker = arena.alloc(Layout::from_size_align(64, 8).unwrap());

        unsafe {
            for i in 0..old.size() {
                *first.add(i) = (255 - i) as u8;
            }
            for i in 0..64 {
                *blocker.add(i) = 0xAB;
            }
        }

        let grown = arena.grow(first, old, new);

        assert_ne!(grown, first);
        assert_no_overlap(
            &[(first as usize, old.size()), (blocker as usize, 64)],
            grown as usize,
            new.size(),
        );
        unsafe {
            for i in 0..old.size() {
                assert_eq!(*grown.add(i), (255 - i) as u8);
            }
            for i in 0..64 {
                assert_eq!(*blocker.add(i), 0xAB);
            }
        }
    }

    #[test]
    fn grow_last_allocation_to_new_block_preserves_bytes() {
        let mut arena = Arena::new();
        let page = Platform::get_page_size();
        let old = Layout::from_size_align(page, 8).unwrap();
        let new = Layout::from_size_align(page * 8, 8).unwrap();
        let ptr = arena.alloc(old);
        let old_block = arena.current_block;

        unsafe {
            for i in 0..old.size() {
                *ptr.add(i) = (i as u8).wrapping_mul(11);
            }
        }

        let grown = arena.grow(ptr, old, new);

        assert_ne!(arena.current_block, old_block);
        unsafe {
            for i in 0..old.size() {
                assert_eq!(*grown.add(i), (i as u8).wrapping_mul(11));
            }
        }
    }

    #[test]
    fn grow_with_stronger_alignment_moves_when_needed() {
        let mut arena = Arena::new();
        let old = Layout::from_size_align(96, 8).unwrap();
        let new = Layout::from_size_align(192, 64).unwrap();
        let ptr = arena.alloc(old);

        unsafe {
            for i in 0..old.size() {
                *ptr.add(i) = i.wrapping_mul(3) as u8;
            }
        }

        let grown = arena.grow(ptr, old, new);

        assert_eq!(grown as usize % 64, 0);
        unsafe {
            for i in 0..old.size() {
                assert_eq!(*grown.add(i), i.wrapping_mul(3) as u8);
            }
        }
    }

    #[test]
    fn shrink_last_allocation_rewinds_cursor() {
        let mut arena = Arena::new();
        let old = Layout::from_size_align(256, 8).unwrap();
        let new = Layout::from_size_align(96, 8).unwrap();
        let ptr = arena.alloc(old);

        arena.shrink(ptr, old, new);

        assert_eq!(arena.cursor, unsafe { ptr.add(new.size()) });
        assert!(arena.is_last_allocation(ptr, new.size()));
    }

    #[test]
    fn shrink_last_allocation_then_alloc_reuses_tail() {
        let mut arena = Arena::new();
        let old = Layout::from_size_align(256, 8).unwrap();
        let new = Layout::from_size_align(64, 8).unwrap();
        let ptr = arena.alloc(old);

        arena.shrink(ptr, old, new);
        let next = arena.alloc(Layout::from_size_align(32, 8).unwrap());

        assert_eq!(next, unsafe { ptr.add(new.size()) });
    }

    #[test]
    fn shrink_non_last_allocation_keeps_cursor_unchanged() {
        let mut arena = Arena::new();
        let old = Layout::from_size_align(256, 8).unwrap();
        let new = Layout::from_size_align(64, 8).unwrap();
        let first = arena.alloc(old);
        let second = arena.alloc(Layout::from_size_align(128, 8).unwrap());
        let cursor = arena.cursor;

        arena.shrink(first, old, new);

        assert_eq!(arena.cursor, cursor);
        assert!(arena.is_last_allocation(second, 128));
    }

    #[test]
    fn shrink_with_stronger_alignment_is_no_op() {
        let mut arena = Arena::new();
        let old = Layout::from_size_align(256, 8).unwrap();
        let new = Layout::from_size_align(64, 64).unwrap();
        let ptr = arena.alloc(old);
        let cursor = arena.cursor;

        arena.shrink(ptr, old, new);

        assert_eq!(arena.cursor, cursor);
    }

    #[test]
    fn shrink_to_same_size_keeps_cursor() {
        let mut arena = Arena::new();
        let layout = Layout::from_size_align(128, 8).unwrap();
        let ptr = arena.alloc(layout);
        let cursor = arena.cursor;

        arena.shrink(ptr, layout, layout);

        assert_eq!(arena.cursor, cursor);
    }

    #[test]
    fn grow_after_shrink_reuses_same_allocation() {
        let mut arena = Arena::new();
        let large = Layout::from_size_align(512, 8).unwrap();
        let small = Layout::from_size_align(128, 8).unwrap();
        let ptr = arena.alloc(large);

        arena.shrink(ptr, large, small);
        let grown = arena.grow(ptr, small, large);

        assert_eq!(grown, ptr);
        assert_eq!(arena.cursor, unsafe { ptr.add(large.size()) });
    }

    #[test]
    fn grow_fallback_old_allocation_remains_readable() {
        let mut arena = Arena::new();
        let old = Layout::from_size_align(128, 8).unwrap();
        let new = Layout::from_size_align(512, 8).unwrap();
        let ptr = arena.alloc(old);
        arena.alloc(Layout::from_size_align(64, 8).unwrap());

        unsafe {
            for i in 0..old.size() {
                *ptr.add(i) = (i as u8).wrapping_add(7);
            }
        }

        let grown = arena.grow(ptr, old, new);

        assert_ne!(grown, ptr);
        unsafe {
            for i in 0..old.size() {
                assert_eq!(*ptr.add(i), (i as u8).wrapping_add(7));
                assert_eq!(*grown.add(i), (i as u8).wrapping_add(7));
            }
        }
    }

    #[test]
    fn stress_grow_last_allocation_many_steps() {
        let mut arena = Arena::new();
        let mut layout = Layout::from_size_align(8, 8).unwrap();
        let ptr = arena.alloc(layout);
        let initialized = layout.size();

        unsafe {
            for i in 0..initialized {
                *ptr.add(i) = 0xC1;
            }
        }

        for size in (16..4096).step_by(16) {
            let new_layout = Layout::from_size_align(size, 8).unwrap();
            let grown = arena.grow(ptr, layout, new_layout);

            assert_eq!(grown, ptr);
            unsafe {
                for i in 0..initialized {
                    assert_eq!(*ptr.add(i), 0xC1);
                }
            }
            layout = new_layout;
        }
    }

    #[test]
    fn stress_shrink_last_allocation_many_steps() {
        let mut arena = Arena::new();
        let mut layout = Layout::from_size_align(4096, 8).unwrap();
        let ptr = arena.alloc(layout);

        for size in (8..4096).rev().step_by(8) {
            let new_layout = Layout::from_size_align(size, 8).unwrap();
            arena.shrink(ptr, layout, new_layout);

            assert_eq!(arena.cursor, unsafe { ptr.add(size) });
            assert!(arena.is_last_allocation(ptr, size));
            layout = new_layout;
        }
    }

    #[test]
    fn stress_grow_non_last_many_allocations_preserve_data() {
        let mut arena = Arena::new();
        let mut entries = Vec::new();

        for i in 0..256 {
            let old = Layout::from_size_align(64, 8).unwrap();
            let ptr = arena.alloc(old);
            arena.alloc(Layout::from_size_align(8, 8).unwrap());
            unsafe {
                for j in 0..old.size() {
                    *ptr.add(j) = (i as u8).wrapping_add(j as u8);
                }
            }
            entries.push((ptr, old, i as u8));
        }

        for (ptr, old, seed) in entries {
            let new = Layout::from_size_align(256, 8).unwrap();
            let grown = arena.grow(ptr, old, new);

            assert_ne!(grown, ptr);
            unsafe {
                for j in 0..old.size() {
                    assert_eq!(*grown.add(j), seed.wrapping_add(j as u8));
                }
            }
        }
    }

    #[test]
    fn stress_random_grow_and_shrink_last_allocation() {
        let mut arena = Arena::new();
        let mut rng = Rng::new(0x600D_514E);

        for _ in 0..1024 {
            let base_size = rng.range(8, 512) as usize;
            let align = 1usize << rng.range(3, 10);
            let old =
                Layout::from_size_align(Arena::align_up(base_size, align).unwrap(), align).unwrap();
            let ptr = arena.alloc(old);
            let grow_size = old.size() + rng.range(1, 4096) as usize;
            let grown_layout = Layout::from_size_align(grow_size, align).unwrap();
            let grown = arena.grow(ptr, old, grown_layout);
            let shrink_size = rng.range(1, grown_layout.size() as u64) as usize;
            let shrunk_layout = Layout::from_size_align(shrink_size, align).unwrap();

            assert_eq!(grown, ptr);
            arena.shrink(grown, grown_layout, shrunk_layout);
            assert_eq!(arena.cursor, unsafe { ptr.add(shrunk_layout.size()) });
            arena.reset();
        }
    }

    #[test]
    fn stress_grow_across_reset_cycles() {
        let mut arena = Arena::new();
        let page = Platform::get_page_size();

        for cycle in 0..512 {
            let old = Layout::from_size_align(128, 8).unwrap();
            let new = Layout::from_size_align(page + cycle, 8).unwrap();
            let ptr = arena.alloc(old);

            unsafe {
                for i in 0..old.size() {
                    *ptr.add(i) = cycle as u8;
                }
            }

            let grown = arena.grow(ptr, old, new);

            unsafe {
                for i in 0..old.size() {
                    assert_eq!(*grown.add(i), cycle as u8);
                }
            }

            arena.reset();
            assert_eq!(chain_len(&arena), 1);
        }
    }

    #[test]
    fn stress_shrink_non_last_never_corrupts_following_allocations() {
        let mut arena = Arena::new();
        let mut following = Vec::new();

        for i in 0..2048 {
            let old = Layout::from_size_align(256, 8).unwrap();
            let new = Layout::from_size_align(32, 8).unwrap();
            let first = arena.alloc(old);
            let second = arena.alloc(Layout::from_size_align(64, 8).unwrap());

            unsafe {
                for j in 0..64 {
                    *second.add(j) = (i as u8).wrapping_mul(5).wrapping_add(j as u8);
                }
            }

            arena.shrink(first, old, new);
            following.push((second, i as u8));
        }

        for (ptr, seed) in following {
            unsafe {
                for j in 0..64 {
                    assert_eq!(*ptr.add(j), seed.wrapping_mul(5).wrapping_add(j as u8));
                }
            }
        }
    }

    #[test]
    fn stress_grow_fallback_with_page_sized_data() {
        let mut arena = Arena::new();
        let page = Platform::get_page_size();

        for i in 0..128 {
            let old = Layout::from_size_align(page, 8).unwrap();
            let new = Layout::from_size_align(page * 2 + i, 8).unwrap();
            let ptr = arena.alloc(old);
            arena.alloc(Layout::from_size_align(16, 8).unwrap());

            unsafe {
                for j in 0..old.size() {
                    *ptr.add(j) = (j as u8).wrapping_add(i as u8);
                }
            }

            let grown = arena.grow(ptr, old, new);

            unsafe {
                for j in 0..old.size() {
                    assert_eq!(*grown.add(j), (j as u8).wrapping_add(i as u8));
                }
            }
        }
    }

    #[test]
    fn is_last_allocation_tracks_after_alloc_grow_and_shrink() {
        let mut arena = Arena::new();
        let a_layout = Layout::from_size_align(64, 8).unwrap();
        let b_layout = Layout::from_size_align(128, 8).unwrap();
        let c_layout = Layout::from_size_align(256, 8).unwrap();

        let a = arena.alloc(a_layout);
        assert!(arena.is_last_allocation(a, a_layout.size()));

        let b = arena.alloc(b_layout);
        assert!(!arena.is_last_allocation(a, a_layout.size()));
        assert!(arena.is_last_allocation(b, b_layout.size()));

        let b_grown = arena.grow(b, b_layout, c_layout);
        assert_eq!(b_grown, b);
        assert!(arena.is_last_allocation(b, c_layout.size()));

        arena.shrink(b, c_layout, a_layout);
        assert!(arena.is_last_allocation(b, a_layout.size()));
    }
}
