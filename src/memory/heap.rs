//! Kernel heap — two modes:
//! - init_static: from a static array (multiboot, no page tables needed)
//! - init: from mapped virtual pages (requires a mapper + frame allocator)

use linked_list_allocator::LockedHeap;
use x86_64::structures::paging::mapper::MapToError;
use x86_64::structures::paging::{FrameAllocator, Mapper, Page, PageTableFlags, Size4KiB};
use x86_64::VirtAddr;

pub const HEAP_START: u64 = 0x_4444_4444_0000;
pub const HEAP_SIZE: u64 = 2 * 1024 * 1024;

const STATIC_HEAP_SIZE: usize = 64 * 1024; // 64K for multiboot mode
static mut STATIC_HEAP: [u8; STATIC_HEAP_SIZE] = [0; STATIC_HEAP_SIZE];

#[global_allocator]
static ALLOCATOR: LockedHeap = LockedHeap::empty();

/// Initialize heap from a static array — no page table operations.
/// Used with multiboot identity-mapped setup.
pub fn init_static() {
    unsafe {
        let heap_start = core::ptr::addr_of_mut!(STATIC_HEAP) as usize;
        ALLOCATOR.lock().init(heap_start, STATIC_HEAP_SIZE);
    }
}

/// Initialize heap from mapped virtual pages — requires paging setup.
pub fn init(
    mapper: &mut impl Mapper<Size4KiB>,
    frame_allocator: &mut impl FrameAllocator<Size4KiB>,
) -> Result<(), MapToError<Size4KiB>> {
    let heap_start = VirtAddr::new(HEAP_START);
    let heap_end = heap_start + HEAP_SIZE - 1u64;
    let page_range = {
        let start_page = Page::containing_address(heap_start);
        let end_page = Page::containing_address(heap_end);
        Page::range_inclusive(start_page, end_page)
    };

    for page in page_range {
        let frame = frame_allocator
            .allocate_frame()
            .ok_or(MapToError::FrameAllocationFailed)?;
        let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE;
        unsafe {
            mapper.map_to(page, frame, flags, frame_allocator)?.flush();
        }
    }

    unsafe {
        ALLOCATOR
            .lock()
            .init(heap_start.as_u64() as usize, HEAP_SIZE as usize);
    }
    Ok(())
}
