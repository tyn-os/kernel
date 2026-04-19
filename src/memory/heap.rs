//! Kernel heap backed by a static array.
//!
//! Uses `linked_list_allocator` as the global allocator. After
//! `init_static()`, `alloc::Box`, `alloc::Vec`, etc. are available.

use linked_list_allocator::LockedHeap;

/// Heap size: 64 KiB (static array, no page table ops needed).
const HEAP_SIZE: usize = 64 * 1024;

static mut HEAP: [u8; HEAP_SIZE] = [0; HEAP_SIZE];

#[global_allocator]
static ALLOCATOR: LockedHeap = LockedHeap::empty();

/// Initialize the global allocator from a static array.
///
/// Must be called once before any heap allocation.
pub fn init_static() {
    // SAFETY: Called once during single-threaded boot. The static HEAP
    // array is not accessed from any other location.
    unsafe {
        let start = core::ptr::addr_of_mut!(HEAP) as usize;
        ALLOCATOR.lock().init(start, HEAP_SIZE);
    }
}
