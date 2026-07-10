mod alloc;
mod platform;
mod tests;
use alloc::AllocatorError;
use core::alloc::Layout;
use core::ptr::null_mut;
use platform::Platform;
#[derive(Debug)]
pub struct BlockHeader {
    prev: *mut BlockHeader,
    mmap_ptr: *mut u8,
    mmap_size: usize,
}

#[derive(Debug)]
pub struct Arena {
    current_block: *mut BlockHeader,
    cursor: *mut u8,
    end: *mut u8,
    double_allowed: bool,
}
pub struct EmptyBlockWrapper(BlockHeader);
unsafe impl Sync for EmptyBlockWrapper {}
unsafe impl Sync for BlockHeader {}

pub static EMPTY_BLOCK: EmptyBlockWrapper = EmptyBlockWrapper(BlockHeader {
    prev: null_mut(),
    mmap_ptr: null_mut(),
    mmap_size: 0,
});

impl EmptyBlockWrapper {
    pub fn get(&self) -> *mut BlockHeader {
        &self.0 as *const BlockHeader as *mut BlockHeader
    }
    pub fn get_ptr(&self) -> *mut u8 {
        self.0.mmap_ptr
    }
}
impl BlockHeader {
    fn new(prev: *mut BlockHeader, mmap_ptr: *mut u8, mmap_size: usize) -> Self {
        Self {
            prev,
            mmap_ptr,
            mmap_size,
        }
    }
    fn prev_ptr(&self) -> *mut BlockHeader {
        self.prev
    }
    fn ptr(&self) -> *mut u8 {
        self.mmap_ptr
    }
    fn size(&self) -> usize {
        self.mmap_size
    }
}

impl Arena {
    pub fn new() -> Self {
        Self {
            current_block: EMPTY_BLOCK.get() as *const BlockHeader as *mut BlockHeader,
            cursor: EMPTY_BLOCK.get_ptr(),
            end: EMPTY_BLOCK.get_ptr(),
            double_allowed: true,
        }
    }

    pub fn alloc(&mut self, layout: Layout) -> *mut u8 {
        Self::try_allocate(self, layout).unwrap_or_else(|err| err.panic())
    }
    
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

    pub fn try_allocate_slow(
        &mut self,
        size: usize,
        align: usize,
    ) -> Result<*mut u8, AllocatorError> {
        let prev_block_header = self.current_block;
        let prev_block_size = unsafe { (*self.current_block).size() };
        let aligned_requested_size = Self::align_up(
            size + size_of::<BlockHeader>() + (align - 1), // total size
            Platform::get_page_size(),                     // align to page_size (4KIB on Linux)
        )
        .unwrap_or_else(|| AllocatorError::Overflow.panic());
        let new_block_size = match prev_block_size.checked_mul(2) {
            Some(d) => d.max(aligned_requested_size),
            _ => aligned_requested_size,
        };
        match Self::new_block(self, new_block_size, prev_block_header) {
            Ok(_) => Ok(self.try_allocate_fast(size, align).unwrap()),
            Err(allocerr) => Err(allocerr),
        }
    }

    pub fn align_up(size: usize, align: usize) -> Option<usize> {
        let checked_cursor_alignment = size.checked_add(align - 1)?;
        Some(checked_cursor_alignment & !(align - 1))
    }

    pub fn align_up_unchecked(size: usize, align: usize) -> usize {
        (size + align - 1) & !(align - 1)
    }
    // fn grow(&mut self, size: usize, align: usize) {
    //     let prev_block_header = self.current_block;
    //     let prev_block_size = unsafe { (*self.current_block).size() };
    //     let aligned_requested_size = Self::align_up(
    //         size + size_of::<BlockHeader>() + (align - 1), // total size
    //         Platform::get_page_size(),                     // align to page_size (4KIB on Linux)
    //     )
    //     .expect("size overflow"); // TODO: Simple error handling for now
    //     let new_block_size = match prev_block_size.checked_mul(2) {
    //         Some(d) => d.max(aligned_requested_size),
    //         _ => aligned_requested_size,
    //     };
    //     // match Self::new_block(self, new_block_size, prev_block_header) {
    //     //     Ok(_) => self.try_allocate_fast(size, align).unwrap_or(AllocatorError::AllocationFailed),
    //     //     _ => panic!()
    //     // }
    // }

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

    fn write_metadata(&mut self, block_header: BlockHeader) {
        let header_ptr = block_header.ptr() as *mut BlockHeader;
        unsafe {
            // TODO: call align_up function
            //
            self.reset_cursor_to(&block_header);
            // self.cursor = block_header.ptr().add(Self::align_up_unchecked(
            //     size_of::<BlockHeader>(),
            //     align_of::<BlockHeader>(),
            // ));
            header_ptr.write(block_header);
            self.current_block = header_ptr;
        }
    }
    #[allow(dead_code)]
    // TODO: Fix Links
    fn reset(&mut self) {
        unsafe {
            while !((*self.current_block).prev_ptr().is_null()) {
                let current_block = core::ptr::read(self.current_block);
                Platform::munmap(current_block.ptr(), current_block.size());
                self.current_block = current_block.prev;
            }

            let current = core::ptr::read(self.current_block);
            self.cursor = current
                .mmap_ptr
                .add(Self::align_up(size_of::<BlockHeader>(), align_of::<BlockHeader>()).unwrap());

            self.end = current.ptr().add(current.mmap_size);
        }
    }
}

impl Drop for Arena {
    fn drop(&mut self) {
        let mut current = self.current_block;
        unsafe {
            while !(current.is_null()) {
                let current_block = core::ptr::read(current);
                Platform::munmap(current_block.ptr(), current_block.size());
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
