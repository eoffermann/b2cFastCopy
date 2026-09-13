//! The RAM arena: one allocation, carved into fixed-size buffers.
//!
//! Allocated once at startup and never grown, so no buffer ever comes from the
//! heap on the hot path. `VirtualAlloc` returns page-aligned memory, which
//! satisfies the sector-alignment requirement of unbuffered I/O on every volume
//! this tool will meet.

use crate::error::{Error, Result};
use crate::win::mem::large_page_minimum;
use windows_sys::Win32::System::Memory::{
    VirtualAlloc, VirtualFree, MEM_COMMIT, MEM_LARGE_PAGES, MEM_RELEASE, MEM_RESERVE,
    PAGE_READWRITE,
};

pub struct Arena {
    base: *mut u8,
    block: usize,
    count: usize,
    pub large_pages: bool,
}

// The arena hands out disjoint buffer slices to workers; it never mutates
// shared state itself.
unsafe impl Send for Arena {}
unsafe impl Sync for Arena {}

impl Arena {
    /// Carve `total` bytes into buffers of `block` bytes.
    ///
    /// Large pages are attempted only when `try_large` is set, and failure is
    /// not an error: they need physically contiguous memory, so a multi-GB
    /// request is genuinely likely to fail on a machine that has been up a
    /// while. Normal pages are a fully supported configuration.
    pub fn new(total: usize, block: usize, try_large: bool) -> Result<Arena> {
        let count = (total / block).max(4);
        let mut bytes = count * block;

        if try_large {
            let granularity = large_page_minimum();
            if granularity > 0 {
                let rounded = bytes.div_ceil(granularity) * granularity;
                let p = unsafe {
                    VirtualAlloc(
                        std::ptr::null(),
                        rounded,
                        MEM_RESERVE | MEM_COMMIT | MEM_LARGE_PAGES,
                        PAGE_READWRITE,
                    )
                };
                if !p.is_null() {
                    return Ok(Arena {
                        base: p.cast(),
                        block,
                        count,
                        large_pages: true,
                    });
                }
            }
        }

        // MEM_COMMIT without touching the pages is intentional: physical pages
        // are faulted in on first use, so startup stays fast even for a
        // multi-GB arena.
        let p = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                bytes,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_READWRITE,
            )
        };
        if p.is_null() {
            bytes = 0;
            let _ = bytes;
            return Err(Error::last("allocate arena"));
        }
        Ok(Arena {
            base: p.cast(),
            block,
            count,
            large_pages: false,
        })
    }

    pub fn block_size(&self) -> usize {
        self.block
    }

    pub fn count(&self) -> usize {
        self.count
    }

    pub fn total_bytes(&self) -> usize {
        self.block * self.count
    }

    /// Pointer to buffer `idx`.
    ///
    /// # Safety
    /// The caller must hold exclusive claim to `idx` via the free ring, and must
    /// not read or write beyond `block_size()` bytes.
    pub unsafe fn ptr(&self, idx: u32) -> *mut u8 {
        debug_assert!((idx as usize) < self.count);
        self.base.add(idx as usize * self.block)
    }
}

impl Drop for Arena {
    fn drop(&mut self) {
        if !self.base.is_null() {
            unsafe { VirtualFree(self.base.cast(), 0, MEM_RELEASE) };
        }
    }
}
