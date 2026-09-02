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
const PAGE_USER: u64 = 1 << 2; // US: user/supervisor (advisory at ring 0)
const PAGE_HUGE: u64 = 1 << 7; // PS: 2 MiB page at PD level
const PAGE_NX: u64 = 1 << 63; // NX: no-execute (honored — EFER.NXE is set at boot)
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
        // US=1 on the upper-level entries covering GiB 0 is PERMISSIVE, not a grant:
        // the effective US is the AND across PML4E→PDPTE→PDE→PTE, so ring-3 access
        // is decided by the *leaf* (kernel leaves stay US=0 → denied; BEAM/shim
        // leaves marked US=1 → allowed). Without US here, a US=1 leaf is still
        // unreachable from ring 3 (this was the Stage-1 attribution's latent gap).
        (*pdpt).entries[0] = phys_of(pd0) | PAGE_PRESENT | PAGE_WRITABLE | PAGE_USER;
        // Stage 3b.0: the low 2 MiB stays US=0 (kernel-confined) — the Stage-3a blanket
        // US=1 relaxation is REMOVED. Stock ERTS startup does latent NULL-base low READS
        // (`&NULL[i]`; ring 0 masked them, ring 3 faults — e.g. #PF cr2=0x2be4). Those
        // are now satisfied on demand by `map_low_zero`: the #PF handler points that one
        // 4 KiB page at a shared READ-ONLY ZERO frame with US=1, so BEAM reads zeros
        // (matches an empty table) and kernel low-RAM (SMP trampoline / BIOS) is NEVER
        // exposed to ring 3. This closes the confinement hole AND keeps the SMAP hunt
        // clean (kernel low accesses stay US=0, not false copy sites). The 0-2 MiB region
        // is pre-split to 4 KiB below (with the kstack arena) so demand-zero is a pure
        // PTE flip — no runtime split / TLB shootdown.
        // GiB 1-3: keep 1 GiB huge pages (PDPTE.PS). Nothing here needs fine
        // granularity in Stage 0, so preserving the boot map's 1 GiB TLB reach
        // avoids the measured ~17% serving-throughput cost of blanket 2 MiB pages
        // — it covers JIT code (the BeamAsm mmap region spills into GiB 1-2) and
        // MMIO (GiB 3). A later stage needing US/NX up here would split them then.
        for gib in 1..4u64 {
            let mut e = gib * GIB | PAGE_PRESENT | PAGE_WRITABLE | PAGE_HUGE;
            // Stage 3a: GiB 1-2 hold the upper BEAM JIT/mmap (MMAP_LIMIT=0xA000_0000);
            // ring-3 BEAM must reach it, so US=1 (permissive 1 GiB, advisory at ring 0).
            // GiB 3 (0xC000_0000+, MMIO/APIC) stays US=0 — kernel only.
            if gib < 3 {
                e |= PAGE_USER;
            }
            (*pdpt).entries[gib as usize] = e;
        }
        let pml4 = core::ptr::addr_of_mut!(PML4);
        (*pml4).entries[0] =
            phys_of(core::ptr::addr_of!(PDPT)) | PAGE_PRESENT | PAGE_WRITABLE | PAGE_USER;

        // Pre-split the kernel-stack guard arena to 4 KiB before any core (incl.
        // this one, post-CR3) can cache a 2 MiB entry for it. Guard installs are
        // then pure PTE clears — no TLB shootdown. Done before the CR3 load so the
        // first table the BSP runs on already has the arena at 4 KiB granularity.
        let mut off = 0u64;
        while off < KSTACK_ARENA_SIZE {
            ensure_pt(KSTACK_ARENA_BASE + off);
            off += TWO_MIB;
        }

        // Stage 3b.0: pre-split the low 2 MiB to 4 KiB (identity, US=0) so `map_low_zero`
        // can satisfy a stray ring-3 low read with a pure PTE flip — no runtime split of
        // a huge page other cores may have cached (no TLB shootdown needed).
        ensure_pt(0);

        // Isolation Stage 1: label the map (BEAM US=1, kernel US=0, NX on kernel
        // data). Advisory at ring 0 — inert, but ready for Stage-3 ring-3 SMEP/SMAP.
        attribute_regions();

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

#[inline]
fn apply_attrs(entry: u64, us: bool, nx: bool) -> u64 {
    let mut v = entry;
    if us { v |= PAGE_USER } else { v &= !PAGE_USER }
    if nx { v |= PAGE_NX } else { v &= !PAGE_NX }
    v
}

/// US-AND-across-levels INVARIANT enforcer. The effective US of a page is the AND of
/// PML4E·PDPTE·PDE·PTE, so a leaf marked US=1 is STILL unreachable from ring 3 unless
/// every covering level is also US=1. This trap has bitten TWICE — Stage-2 (the page
/// walk needed US=1 on PML4[0]/PDPT[0], not just leaves) and Stage-3a (`ensure_pt`
/// builds split PDEs US=0, masking a US=1 PTE). So it is now structurally impossible:
/// EVERY path that marks a leaf US=1 (`set_attrs`, `set_page_us`) first calls this,
/// which walks `addr` and marks each covering level (strictly above the leaf) US=1.
/// Permissive — the leaf itself still enforces (kernel leaves stay US=0). Idempotent.
///
/// # Safety
/// Mutates live page tables; single-threaded (init) or caller guarantees no concurrent
/// walkers of `addr`'s upper tables.
unsafe fn mark_user_covering_levels(addr: u64) {
    unsafe {
        let gib = (addr / GIB) as usize;
        if gib >= 4 {
            return;
        }
        // PML4E always covers GiB 0–3 in this single-PML4 identity map.
        (*core::ptr::addr_of_mut!(PML4)).entries[0] |= PAGE_USER;
        let pdpt = core::ptr::addr_of_mut!(PDPT);
        if (*pdpt).entries[gib] & PAGE_HUGE != 0 {
            return; // PDPTE is the 1 GiB leaf (caller sets it); its only cover, PML4E, is done
        }
        (*pdpt).entries[gib] |= PAGE_USER;
        let pd = &mut (*core::ptr::addr_of_mut!(PDS))[gib];
        let pd_idx = ((addr % GIB) / TWO_MIB) as usize;
        if pd.entries[pd_idx] & PAGE_HUGE != 0 {
            return; // PDE is the 2 MiB leaf (caller sets it); covers PML4E+PDPTE done
        }
        pd.entries[pd_idx] |= PAGE_USER; // PDE covers the 4 KiB PT leaf
    }
}

/// Set US (user) and NX bits on every present page in `[start, end)`. Both bounds
/// must be 2 MiB-aligned. Operates at the current granularity: a 2 MiB huge PDE is
/// set on the PDE; a split (4 KiB) region is set on each present PTE (not-present
/// guard pages are left alone). GiB 1–3 (1 GiB huge pages in the hybrid map) are
/// skipped — nothing needing per-region US/NX lives there in Stage 1 (deferred to
/// the Stage-3 split). Only the address/attribute bits change; the mapping does not.
///
/// # Safety
/// Mutates the live page tables; call single-threaded (during `init`, before the
/// CR3 load) or with the caller guaranteeing no concurrent walkers.
unsafe fn set_attrs(start: u64, end: u64, us: bool, nx: bool) {
    unsafe {
        let mut a = start & !(TWO_MIB - 1);
        while a < end {
            let gib = (a / GIB) as usize;
            if gib >= 4 {
                break;
            }
            // GiB 1–3 are 1 GiB huge pages in the hybrid map — skip (Stage 3 splits).
            if (*core::ptr::addr_of!(PDPT)).entries[gib] & PAGE_HUGE != 0 {
                a += TWO_MIB;
                continue;
            }
            // US-AND invariant: before marking any leaf US=1, make its covering levels
            // permissive (else the leaf is silently unreachable from ring 3).
            if us {
                mark_user_covering_levels(a);
            }
            let pd = &mut (*core::ptr::addr_of_mut!(PDS))[gib];
            let pd_idx = ((a % GIB) / TWO_MIB) as usize;
            let e = pd.entries[pd_idx];
            if e == 0 {
                // not mapped (shouldn't happen in the identity map) — skip
            } else if e & PAGE_HUGE != 0 {
                pd.entries[pd_idx] = apply_attrs(e, us, nx);
            } else {
                let pt = &mut *((e & ADDR_MASK) as *mut PageTable);
                for j in 0..512usize {
                    if pt.entries[j] & PAGE_PRESENT != 0 {
                        pt.entries[j] = apply_attrs(pt.entries[j], us, nx);
                    }
                }
            }
            a += TWO_MIB;
        }
    }
}

/// Isolation Stage 1: attribute the identity map — BEAM regions US=1, kernel
/// regions US=0, NX on non-executable data. **Advisory at ring 0** (US is ignored
/// for access control while CPL=0, and NX only bites pages nothing executes from),
/// so this is behaviorally INERT — it labels the map for the Stage-3 ring-3
/// SMEP/SMAP enforcement without changing any runtime behavior now. All bounds are
/// fixed 2 MiB-aligned constants (see docs memory map): kernel `.text` is in
/// `[0x0F00_0000, 0x0F200000)` so everything in `[0x0F200000, 0x1A00_0000)` is
/// non-exec kernel data regardless of exact section end. GiB 0 only (the 2 MiB PD);
/// the JIT/MMIO in GiB 1–2 stay coarse until Stage 3 splits them for enforcement.
///
/// # Safety
/// Call once during `init`, before the CR3 load.
unsafe fn attribute_regions() {
    unsafe {
        // BEAM / user (US=1). ELF+brk block contains BeamAsm-independent native
        // text (0x600000+) so it must stay executable (NX=false); the JIT region is
        // user-RWX by necessity. Only the user stack is NX among the BEAM regions.
        set_attrs(0x0040_0000, 0x0700_0000, true, false); // BEAM ELF image + brk heap
        set_attrs(0x0E00_0000, 0x0E20_0000, true, true); // BEAM user stack (NX)
        set_attrs(0x1A00_0000, 0x4000_0000, true, false); // BEAM JIT/mmap low (RWX)
        // Kernel (US=0). NX the contiguous non-exec data block (rodata tail, data,
        // bss+heap, DMA, ELF-copy, cpio) — kernel .text (below 0x0F200000), the low
        // trampoline, and the kstack arena are left executable/untouched here.
        set_attrs(0x0F20_0000, 0x1A00_0000, false, true); // kernel non-exec data (NX)
        // Isolation Stage 2: the ring-3 transition shim runs from a US=1 page in the
        // otherwise-unused gap at 0x0C00_0000. US=1 so ring 3 can fetch/access it;
        // executable (NX=false) so ring 3 can run the shim code. Advisory at ring 0.
        #[cfg(feature = "stage2_shim")]
        set_attrs(0x0C00_0000, 0x0C20_0000, true, false);
        crate::serial_println!(
            "[paging] Stage-1 US/NX attributed (BEAM US=1, kernel data NX; advisory at ring 0)"
        );
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
/// Stage 3a confinement teeth (feature `confine_probe`): flip the US bit on the
/// single 4 KiB page containing `addr`, splitting its 2 MiB region to 4 KiB first.
/// Used to (a) make one kernel-`.text` page ring-3-executable for the probe stub,
/// and (b) toggle a scratch page's US bit to prove enforcement (US=0) is the SOLE
/// difference between "faults+contained" and "write succeeds+corrupts". Flushes the
/// page's TLB entry. Single-threaded / boot-thread use only (no cross-core shootdown).
///
/// # Safety
/// Mutates live page tables; `addr` must lie in the 0–4 GiB identity region and the
/// caller must guarantee no concurrent walkers of that page on other cores.
pub unsafe fn set_page_us(addr: u64, us: bool) {
    unsafe {
        let page = addr & !(FOUR_KIB - 1);
        let pt = ensure_pt(page);
        if pt.is_null() {
            crate::serial_println!("[paging] set_page_us {:#x} FAILED (split pool)", page);
            return;
        }
        // US-AND invariant: making the PTE US=1 requires every covering level US=1
        // (`ensure_pt` builds split PDEs US=0). Route through the single enforcer so a
        // US=1 leaf is never silently masked. Only for us=true — a US=0 leaf is denied
        // regardless of uppers, and other split pages in the 2 MiB may need US=1.
        if us {
            mark_user_covering_levels(page);
        }
        let idx = ((page % TWO_MIB) / FOUR_KIB) as usize;
        let e = (*pt).entries[idx];
        (*pt).entries[idx] = if us { e | PAGE_USER } else { e & !PAGE_USER };
        core::arch::asm!("invlpg [{}]", in(reg) page, options(nostack, preserves_flags));
    }
}

/// Stage 3b.0: pool of per-page WRITABLE scratch frames for stray ring-3 low accesses.
/// Stock ERTS startup does a read-MODIFY-WRITE through a NULL-base table (reads 0x2be4,
/// then writes it), so a read-only page is insufficient — each touched low page gets its
/// OWN zero-initialized throwaway frame (dedicated kernel memory, NOT the trampoline/
/// BIOS low-RAM), so BEAM's latent NULL-base RMW is absorbed harmlessly and kernel
/// low-RAM stays confined (US=0). Per-page (not shared) so distinct low pages don't
/// alias. 16 frames = 64 KiB; ERTS touches ~1.
#[repr(C, align(4096))]
#[derive(Clone, Copy)]
struct ScratchFrame([u8; 4096]);
const LOW_SCRATCH_FRAMES: usize = 16;
static mut LOW_SCRATCH: [ScratchFrame; LOW_SCRATCH_FRAMES] =
    [ScratchFrame([0u8; 4096]); LOW_SCRATCH_FRAMES];
static LOW_SCRATCH_NEXT: AtomicUsize = AtomicUsize::new(0);

/// Stage 3b.0: satisfy a stray ring-3 read/write of the low 2 MiB (latent ERTS NULL-base
/// RMW that ring 0 masked) by pointing that one 4 KiB page at a dedicated writable scratch
/// frame with US=1 — BEAM reads/writes zeros-then-garbage there, kernel low-RAM (SMP
/// trampoline / BIOS) is NEVER exposed. The 0-2 MiB region is pre-split at init, so this
/// is a pure PTE flip + invlpg. Returns false if `addr` is outside the low 2 MiB or the
/// scratch pool is exhausted — the caller then treats the fault as a real violation.
///
/// # Safety
/// Mutates live page tables; per-core self-heal (the faulting core invlpg's its own
/// entry). The low 2 MiB is not kernel-accessed at runtime, so a stale huge TLB entry
/// on another core is harmless.
pub unsafe fn map_low_scratch(addr: u64) -> bool {
    let page = addr & !(FOUR_KIB - 1);
    if page >= TWO_MIB {
        return false;
    }
    unsafe {
        let slot = LOW_SCRATCH_NEXT.fetch_add(1, Ordering::Relaxed);
        if slot >= LOW_SCRATCH_FRAMES {
            crate::serial_println!("[low-zero] scratch pool exhausted at {:#x}", page);
            return false;
        }
        let pt = ensure_pt(page);
        if pt.is_null() {
            return false;
        }
        mark_user_covering_levels(page); // US-AND invariant: covering levels permissive
        let frame = core::ptr::addr_of!(LOW_SCRATCH[slot]) as u64; // identity-mapped: PA == VA
        let idx = ((page % TWO_MIB) / FOUR_KIB) as usize;
        (*pt).entries[idx] = (frame & ADDR_MASK) | PAGE_PRESENT | PAGE_WRITABLE | PAGE_USER;
        core::arch::asm!("invlpg [{}]", in(reg) page, options(nostack, preserves_flags));
        true
    }
}

/// Stage 3b.2 confused-deputy guard (ground truth). Is EVERY 4 KiB page of
/// `[addr, addr+len)` mapped PRESENT and US=1 at EVERY paging level (PML4E·PDPTE·PDE·PTE
/// — the same AND the CPU applies)? A ring-3 syscall pointer must target BEAM's own
/// (US=1) memory; a kernel-range (US=0), unmapped, or out-of-map pointer returns false so
/// `uaccess` rejects it with EFAULT BEFORE the kernel dereferences it. SMAP forces the
/// stac, but ONLY this check stops the kernel touching kernel memory on BEAM's behalf.
pub fn user_accessible(addr: u64, len: u64) -> bool {
    // 3b.2 TEETH mutation: disable the bounds-check so the confused-deputy deref
    // demonstrably occurs (the mutation build proves the check below is the SOLE
    // enforcement). Never in a real build.
    #[cfg(feature = "deputy_mutation")]
    {
        let _ = (addr, len);
        return true;
    }
    #[cfg(not(feature = "deputy_mutation"))]
    {
        if len == 0 {
            return true;
        }
        let end = match addr.checked_add(len) {
            Some(e) => e,
            None => return false, // wrap / non-canonical
        };
        if end > 4 * GIB {
            return false; // beyond the 4 GiB identity map — never user memory
        }
        let mut a = addr & !(FOUR_KIB - 1);
        while a < end {
            if !page_is_user(a) {
                return false;
            }
            a += FOUR_KIB;
        }
        true
    }
}

/// Stage 3b.2: turn on CR4.SMAP (bit 21) on the CURRENT CPU. After this, any ring-0
/// access to a US=1 (BEAM) page faults unless bracketed by stac/clac (see `uaccess`).
/// The BSP calls this right before `jump_to_user` (after the legit boot-time ELF-load /
/// argv writes to US=1 memory); each AP sets it in `ap_main` (CR4 is per-CPU).
///
/// # Safety
/// Enables a hardware protection that faults on unguarded kernel↔user access — every
/// such site must already route through `uaccess`, or must be reached only after here.
pub unsafe fn enable_smap() {
    unsafe {
        let mut cr4: u64;
        core::arch::asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack, preserves_flags));
        cr4 |= 1 << 21;
        core::arch::asm!("mov cr4, {}", in(reg) cr4, options(nomem, nostack, preserves_flags));
    }
}

/// Walk `addr` and require PRESENT+US at every level (huge PDPTE/PDE terminate early).
fn page_is_user(addr: u64) -> bool {
    const PU: u64 = PAGE_PRESENT | PAGE_USER;
    unsafe {
        let pml4 = &*core::ptr::addr_of!(PML4);
        let e = pml4.entries[((addr >> 39) & 0x1ff) as usize];
        if e & PU != PU {
            return false;
        }
        let pdpt = &*((e & ADDR_MASK) as *const PageTable);
        let e = pdpt.entries[((addr >> 30) & 0x1ff) as usize];
        if e & PU != PU {
            return false;
        }
        if e & PAGE_HUGE != 0 {
            return true; // 1 GiB leaf, US=1
        }
        let pd = &*((e & ADDR_MASK) as *const PageTable);
        let e = pd.entries[((addr >> 21) & 0x1ff) as usize];
        if e & PU != PU {
            return false;
        }
        if e & PAGE_HUGE != 0 {
            return true; // 2 MiB leaf, US=1
        }
        let pt = &*((e & ADDR_MASK) as *const PageTable);
        let e = pt.entries[((addr >> 12) & 0x1ff) as usize];
        e & PU == PU
    }
}

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
