//! Virtio HAL — identity-mapped DMA (vaddr == paddr).
//!
//! With multiboot's identity-mapped 4 GiB address space, physical and
//! virtual addresses are the same. DMA buffers are allocated sequentially
//! from a linker-defined region after the kernel's BSS.

use core::ptr::NonNull;
use core::sync::atomic::{AtomicU64, Ordering};
use virtio_drivers::{BufferDirection, Hal, PAGE_SIZE, PhysAddr};

unsafe extern "C" {
    static dma_region: u8;
}

static DMA_PADDR: AtomicU64 = AtomicU64::new(0);

/// Initialize the DMA bump allocator from the linker-provided `dma_region` symbol.
pub fn init_dma() {
    // SAFETY: `dma_region` is defined by the linker script at the end of BSS.
    let addr = unsafe { &dma_region as *const u8 as u64 };
    DMA_PADDR.store(addr, Ordering::SeqCst);
    crate::serial_println!("[hal] DMA base={:#x}", addr);
}

/// HAL implementation for the `virtio-drivers` crate.
pub struct TynHal;

unsafe impl Hal for TynHal {
    fn dma_alloc(pages: usize, _direction: BufferDirection) -> (PhysAddr, NonNull<u8>) {
        let paddr = DMA_PADDR.fetch_add((PAGE_SIZE * pages) as u64, Ordering::SeqCst);
        // SAFETY: Identity-mapped — paddr is a valid writable address in our
        // flat 4 GiB address space. The bump allocator guarantees no overlap.
        let vaddr = NonNull::new(paddr as *mut u8).unwrap();
        (paddr, vaddr)
    }

    unsafe fn dma_dealloc(_paddr: PhysAddr, _vaddr: NonNull<u8>, _pages: usize) -> i32 {
        0 // Bump allocator — no deallocation
    }

    unsafe fn mmio_phys_to_virt(paddr: PhysAddr, _size: usize) -> NonNull<u8> {
        // SAFETY: Identity-mapped — device MMIO at paddr is directly accessible.
        NonNull::new(paddr as *mut u8).unwrap()
    }

    unsafe fn share(buffer: NonNull<[u8]>, _direction: BufferDirection) -> PhysAddr {
        // SAFETY: Identity-mapped — vaddr == paddr.
        buffer.as_ptr() as *mut u8 as u64
    }

    unsafe fn unshare(_paddr: PhysAddr, _buffer: NonNull<[u8]>, _direction: BufferDirection) {}
}
