//! Virtio HAL — identity-mapped (vaddr == paddr), matching rcore example.

use core::ptr::NonNull;
use core::sync::atomic::{AtomicU64, Ordering};
use virtio_drivers::{BufferDirection, Hal, PAGE_SIZE, PhysAddr};

unsafe extern "C" {
    static dma_region: u8;
}

static DMA_PADDR: AtomicU64 = AtomicU64::new(0);

pub fn init_dma() {
    let addr = unsafe { &dma_region as *const u8 as u64 };
    DMA_PADDR.store(addr, Ordering::SeqCst);
    crate::serial_println!("[hal] DMA region at {:#x}", addr);
}

pub struct TynHal;

unsafe impl Hal for TynHal {
    fn dma_alloc(pages: usize, _direction: BufferDirection) -> (PhysAddr, NonNull<u8>) {
        let paddr = DMA_PADDR.fetch_add((PAGE_SIZE * pages) as u64, Ordering::SeqCst);
        let vaddr = NonNull::new(paddr as *mut u8).unwrap();
        crate::serial_println!("[dma] pa={:#x} pages={}", paddr, pages);
        (paddr, vaddr)
    }

    unsafe fn dma_dealloc(_paddr: PhysAddr, _vaddr: NonNull<u8>, _pages: usize) -> i32 {
        0 // no-op, bump allocator
    }

    unsafe fn mmio_phys_to_virt(paddr: PhysAddr, _size: usize) -> NonNull<u8> {
        // Identity-mapped: vaddr == paddr
        NonNull::new(paddr as *mut u8).unwrap()
    }

    unsafe fn share(buffer: NonNull<[u8]>, _direction: BufferDirection) -> PhysAddr {
        buffer.as_ptr() as *mut u8 as u64 // vaddr == paddr
    }

    unsafe fn unshare(_paddr: PhysAddr, _buffer: NonNull<[u8]>, _direction: BufferDirection) {}
}
