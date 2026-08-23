//! Stage-0 paging layer — a *splittable* identity map.
//!
//! The boot asm (`multiboot.S`) maps 0–4 GiB with four **1 GiB huge pages**
//! (`PDPTE.PS`), all supervisor+writable, and never touches the tables again.
//! 1 GiB pages cannot express per-page attributes, so guard pages (a single
//! not-present 4 KiB page under a stack) — and later per-page US/NX for the
//! ring-3 isolation stages — are impossible on that map.
//!
//! This module rebuilds the *same* 0–4 GiB identity map as a **hybrid hierarchy**
//! and loads it into CR3: GiB 0 as a **2 MiB PD** (splittable to 4 KiB where a
//! guard — or later a US boundary — needs it; the kernel-stack arena lives here),
//! and GiB 1–3 kept as **1 GiB huge pages** (nothing there needs fine granularity
//! in Stage 0, and 1 GiB TLB reach avoids a measured ~17% serving-throughput cost
//! that blanket 2 MiB paging added — GiB 1–2 cover JIT code, GiB 3 covers MMIO).
//! It is **behavior-preserving**: same addresses, same supervisor+writable
//! flags, same cache behavior (no PCD/PWT, matching the boot map — MMIO stays as
//! the firmware MTRRs dictate). The only observable change is *granularity*.
//!
//! SMP: `smp::boot_ap` reads the BSP's live CR3 and hands it to each AP, so
//! calling [`init`] on the BSP **before** `smp::boot_aps` makes every core adopt
//! this hierarchy automatically — no trampoline change.
//!
//! Stage 0 does **no** privilege separation: every page stays supervisor. US/NX
//! and the ring-3 transition are Stage 1+ (see `docs/ISOLATION_SCOPING.md`).

use core::sync::atomic::{AtomicUsize, Ordering};

const PAGE_PRESENT: u64 = 1 << 0;
const PAGE_WRITABLE: u64 = 1 << 1;
const PAGE_HUGE: u64 = 1 << 7; // PS: 2 MiB page at PD level
/// Physical-address field of a PTE (bits 12..52), 4 KiB-aligned.
const ADDR_MASK: u64 = 0x000f_ffff_ffff_f000;

const GIB: u64 = 0x4000_0000;
const TWO_MIB: u64 = 0x20_0000;
const FOUR_KIB: u64 = 0x1000;

#[repr(C, align(4096))]
#[derive(Clone, Copy)]
struct PageTable {
    entries: [u64; 512],
}
impl PageTable {
    const EMPTY: PageTable = PageTable { entries: [0; 512] };
}

// Base hierarchy for the 4 GiB identity map (all in .bss, 4 KiB-aligned):
//   PML4[0]      -> PDPT
//   PDPT[0..4]   -> PD0..PD3            (one PD per GiB)
//   PDx[0..512]  -> 2 MiB identity pages
static mut PML4: PageTable = PageTable::EMPTY;
static mut PDPT: PageTable = PageTable::EMPTY;
static mut PDS: [PageTable; 4] = [PageTable::EMPTY; 4];

/// Pool of page tables consumed when a 2 MiB entry is split to 4 KiB (one PT per
/// distinct 2 MiB region needing finer granularity). Sized for the kernel-stack
/// arena pre-split below (8 regions) plus headroom.
const MAX_SPLIT_PTS: usize = 16;
static mut SPLIT_PTS: [PageTable; MAX_SPLIT_PTS] = [PageTable::EMPTY; MAX_SPLIT_PTS];
static SPLIT_NEXT: AtomicUsize = AtomicUsize::new(0);

/// Kernel-stack guard arena. The scheduler bump-allocates per-thread kernel
/// stacks from here (`sched.rs`, base `0x0700_0000`); we **pre-split** it to
/// 4 KiB at [`init`] — *before* APs boot — so (a) no core ever caches a 2 MiB
/// TLB entry covering a future guard page (which would let an overflow slip past
/// the guard on that core), and (b) runtime guard installation is a pure PTE
/// clear needing no cross-core TLB shootdown. 16 MiB ≈ 680 stacks at the 24 KiB
/// stride — far above BEAM's kernel-thread count.
pub const KSTACK_ARENA_BASE: u64 = 0x0700_0000;
pub const KSTACK_ARENA_SIZE: u64 = 16 * 1024 * 1024;

/// Physical address of a static table. The kernel is identity-mapped, so the
/// virtual address of the static *is* its physical address.
#[inline]
fn phys_of<T>(p: *const T) -> u64 {
    p as u64
}

/// Build the 2 MiB identity hierarchy and load it into CR3.
///
/// # Safety
/// Call exactly once, on the BSP, early in boot and **before** `smp::boot_aps`
/// (so APs inherit this CR3). The new map is identity-equivalent to the boot map,
/// so switching CR3 is transparent to all running code.
pub unsafe fn init() {
    unsafe {
        let pds = core::ptr::addr_of_mut!(PDS);
        let pdpt = core::ptr::addr_of_mut!(PDPT);
        // GiB 0: a full 2 MiB PD. The kernel-stack guard arena (and any later
        // fine-grained region) lives here and needs sub-1 GiB granularity.
        let pd0 = &mut (*pds)[0];
        for i in 0..512usize {
            pd0.entries[i] = (i as u64) * TWO_MIB | PAGE_PRESENT | PAGE_WRITABLE | PAGE_HUGE;
        }
        (*pdpt).entries[0] = phys_of(pd0) | PAGE_PRESENT | PAGE_WRITABLE;
        // GiB 1-3: keep 1 GiB huge pages (PDPTE.PS). Nothing here needs fine
        // granularity in Stage 0, so preserving the boot map's 1 GiB TLB reach
        // avoids the measured ~17% serving-throughput cost of blanket 2 MiB pages
        // — it covers JIT code (the BeamAsm mmap region spills into GiB 1-2) and
        // MMIO (GiB 3). A later stage needing US/NX up here would split them then.
        for gib in 1..4u64 {
            (*pdpt).entries[gib as usize] =
                gib * GIB | PAGE_PRESENT | PAGE_WRITABLE | PAGE_HUGE;
        }
        let pml4 = core::ptr::addr_of_mut!(PML4);
        (*pml4).entries[0] = phys_of(core::ptr::addr_of!(PDPT)) | PAGE_PRESENT | PAGE_WRITABLE;

        // Pre-split the kernel-stack guard arena to 4 KiB before any core (incl.
        // this one, post-CR3) can cache a 2 MiB entry for it. Guard installs are
        // then pure PTE clears — no TLB shootdown. Done before the CR3 load so the
        // first table the BSP runs on already has the arena at 4 KiB granularity.
        let mut off = 0u64;
        while off < KSTACK_ARENA_SIZE {
            ensure_pt(KSTACK_ARENA_BASE + off);
            off += TWO_MIB;
        }

        let cr3 = phys_of(pml4);
        core::arch::asm!("mov cr3, {}", in(reg) cr3, options(nostack, preserves_flags));
        crate::serial_println!(
            "[paging] 2 MiB identity hierarchy live (kstack arena 4 KiB pre-split), CR3={:#x}",
            cr3
        );
    }
}

/// Ensure the 2 MiB region containing `addr` is expressed as a 4 KiB PT (split
/// the huge page into an identity PT on first use), and return that PT. Idempotent
/// — a region already split just returns its existing PT. Returns null if the
/// split-PT pool is exhausted.
///
/// # Safety
/// Mutates the live page tables; call single-threaded (during `init`) or on a
/// region no other core has cached as a 2 MiB entry.
unsafe fn ensure_pt(addr: u64) -> *mut PageTable {
    unsafe {
        let gib = (addr / GIB) as usize;
        let pd_idx = ((addr % GIB) / TWO_MIB) as usize;
        if gib >= 4 {
            return core::ptr::null_mut();
        }
        let pd = &mut (*core::ptr::addr_of_mut!(PDS))[gib];
        if pd.entries[pd_idx] & PAGE_HUGE != 0 {
            let slot = SPLIT_NEXT.fetch_add(1, Ordering::Relaxed);
            if slot >= MAX_SPLIT_PTS {
                crate::serial_println!("[paging] split-PT pool exhausted");
                return core::ptr::null_mut();
            }
            let pt = &mut (*core::ptr::addr_of_mut!(SPLIT_PTS))[slot];
            let region_base = addr & !(TWO_MIB - 1);
            for j in 0..512usize {
                pt.entries[j] =
                    (region_base + (j as u64) * FOUR_KIB) | PAGE_PRESENT | PAGE_WRITABLE;
            }
            pd.entries[pd_idx] = phys_of(pt as *const PageTable) | PAGE_PRESENT | PAGE_WRITABLE;
        }
        (pd.entries[pd_idx] & ADDR_MASK) as *mut PageTable
    }
}

/// Mark the 4 KiB page containing `addr` **not-present** (a guard page). Splits
/// the enclosing 2 MiB page into a 4 KiB PT on first use of that region; the rest
/// of the region keeps its identity mapping. A subsequent access to `addr`
/// (e.g. a kernel stack overflowing into it) then takes a clean `#PF` instead of
/// silently corrupting the neighbor.
///
/// `addr` is rounded down to its 4 KiB page. Returns the guarded page base, or 0
/// if the split-PT pool is exhausted (logged; guard not installed).
///
/// # Safety
/// `addr` must lie in the 0–4 GiB identity region and must NOT be actively used
/// as mapped memory (it is about to become unmapped). Intended for the dead page
/// *below* a freshly allocated kernel stack, before that stack is used.
pub unsafe fn map_guard_page(addr: u64) -> u64 {
    unsafe {
        let page = addr & !(FOUR_KIB - 1);
        let pt = ensure_pt(page);
        if pt.is_null() {
            crate::serial_println!("[paging] guard {:#x} NOT installed (split failed)", page);
            return 0;
        }
        let pte_idx = ((page % TWO_MIB) / FOUR_KIB) as usize;
        (*pt).entries[pte_idx] = 0; // not present
        core::arch::asm!("invlpg [{}]", in(reg) page, options(nostack, preserves_flags));
        page
    }
}
