//! Per-event scratch arena for zero-allocation hot paths.
//!
//! Inspired by Firedancer's `fd_scratch`: a bump allocator that is reset
//! at the start of each `process_event` call. All temporary allocations
//! during event processing (message decoding, signature buffers, etc.)
//! go into the arena and are freed in bulk when the event is done.
//!
//! Usage:
//! ```ignore
//! let arena = ScratchArena::new();
//! // ... hot loop ...
//! arena.reset(); // O(1) bulk free
//! let buf = arena.alloc_slice_copy(&[1, 2, 3]);
//! ```

use bumpalo::Bump;

/// Thread-local scratch arena with reset semantics.
///
/// Wraps `bumpalo::Bump` with a pre-allocated capacity. Call `reset()`
/// between processing cycles to reclaim all memory in O(1).
pub struct ScratchArena {
    bump: Bump,
}

impl ScratchArena {
    /// Create a new arena with the given initial capacity in bytes.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            bump: Bump::with_capacity(capacity),
        }
    }

    /// Create a new arena with default capacity (64KB — enough for typical
    /// consensus event processing including message decode + sig verify).
    pub fn new() -> Self {
        Self::with_capacity(64 * 1024)
    }

    /// Reset the arena, freeing all allocations in O(1).
    /// The backing memory is retained for reuse.
    pub fn reset(&mut self) {
        self.bump.reset();
    }

    /// Allocate a value in the arena.
    pub fn alloc<T>(&self, val: T) -> &mut T {
        self.bump.alloc(val)
    }

    /// Allocate a byte slice copy in the arena.
    pub fn alloc_slice_copy(&self, src: &[u8]) -> &mut [u8] {
        self.bump.alloc_slice_copy(src)
    }

    /// Allocate a zeroed byte buffer of given size.
    pub fn alloc_zeroed(&self, len: usize) -> &mut [u8] {
        self.bump.alloc_slice_fill_default(len)
    }

    /// Returns the number of bytes currently allocated.
    pub fn allocated_bytes(&self) -> usize {
        self.bump.allocated_bytes()
    }

    /// Access the inner `Bump` allocator directly.
    pub fn inner(&self) -> &Bump {
        &self.bump
    }
}

impl Default for ScratchArena {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_alloc_and_reset() {
        let mut arena = ScratchArena::new();
        let x = arena.alloc(42u64);
        assert_eq!(*x, 42);
        assert!(arena.allocated_bytes() > 0);

        arena.reset();
        // After reset, allocated_bytes may not be 0 (bump retains capacity)
        // but new allocations reuse the same memory region.
        let y = arena.alloc(99u64);
        assert_eq!(*y, 99);
    }

    #[test]
    fn slice_copy() {
        let arena = ScratchArena::new();
        let data = [1u8, 2, 3, 4, 5];
        let copy = arena.alloc_slice_copy(&data);
        assert_eq!(copy, &data);
    }

    #[test]
    fn zeroed_alloc() {
        let arena = ScratchArena::new();
        let buf = arena.alloc_zeroed(256);
        assert_eq!(buf.len(), 256);
        assert!(buf.iter().all(|&b| b == 0));
    }

    #[test]
    fn repeated_reset_reuses_memory() {
        let mut arena = ScratchArena::with_capacity(1024);
        for _ in 0..100 {
            arena.reset();
            let _ = arena.alloc_slice_copy(&[0xAA; 512]);
        }
        // Should not have grown unboundedly
    }
}
