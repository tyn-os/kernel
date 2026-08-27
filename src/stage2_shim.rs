//! Isolation Stage 2 — ring-3 transition shim (feature `stage2_shim`, never in
//! production). Proves the DPL=3 int-gate → `TSS.RSP0` clean stack → `swapgs` →
//! `iretq` round-trip on a *trivial* ring-3 program (not BEAM), per
//! `directions/STAGE2_SHIM_RESUME.md`.
//!
//! Path (entirely separate from BEAM's LSTAR/`syscall` path, so the ring-0
//! serving path is untouched): the driver builds a ring-3 `iretq` frame, `swapgs`,
//! `iretq` into the shim at `0x0C00_0000` (a US=1 page). The shim does N
//! `int 0x80` round-trips (each: RSP0 clean stack → `swapgs` → read `gs:[0]` to
//! prove per-CPU GS is restored → `swapgs` → `iretq`) then `int 0x81` (done), whose
//! handler `swapgs`es and longjmps back to the driver via a saved kernel RSP.
//!
//! `cli` spans the whole excursion (the int gates also run IF=0), so no interrupt
//! lands mid-excursion while GS is the user base. **Honest scope note:** this means
//! the shim proves the *clean* transition only — the interrupt-during-ring-3 path
//! (a timer landing while in ring 3, needing its own swapgs keyed on where it
//! interrupted from) is NOT exercised here; that is a real Stage-3 concern (BEAM in
//! ring 3 with interrupts on), not proven by this cli'd shim.
//!
//! swapgs parity: driver(1) + int0x81(1) = 2 → GS ends on per-CPU.

#![cfg(feature = "stage2_shim")]

use crate::serial_println;

core::arch::global_asm!(
    ".section .data",
    ".balign 8",
    ".global stage2_int80_count",
    "stage2_int80_count: .quad 0",
    ".global stage2_gs_check",
    "stage2_gs_check: .quad 0",
    ".global stage2_r15",
    "stage2_r15: .quad 0",
    ".global stage2_rflags",
    "stage2_rflags: .quad 0",
    ".global stage2_saved_rsp",
    "stage2_saved_rsp: .quad 0",
    ".global stage2_user_cs",
    "stage2_user_cs: .quad 0",
    ".global stage2_user_ss",
    "stage2_user_ss: .quad 0",
    ".global stage2_mut_skip_swapgs",
    "stage2_mut_skip_swapgs: .byte 0",
    ".global stage2_mut_bad_iret",
    "stage2_mut_bad_iret: .byte 0",

    ".section .text",
    // --- int 0x80 handler: one round-trip. Lands on RSP0 (clean stack), IF=0. ---
    ".global stage2_int80_entry",
    "stage2_int80_entry:",
    "    cmp byte ptr [rip + stage2_mut_skip_swapgs], 0",
    "    jne .Ls2_no_swapin",          // mutation: skip entry swapgs (GS stays user)
    "    swapgs",
    ".Ls2_no_swapin:",
    "    push rax",
    "    mov rax, gs:[0]",             // per-CPU kernel stack top — valid ONLY if swapgs ran
    "    mov [rip + stage2_gs_check], rax",
    "    inc qword ptr [rip + stage2_int80_count]",
    "    pop rax",
    "    cmp byte ptr [rip + stage2_mut_skip_swapgs], 0",
    "    jne .Ls2_no_swapout",         // skip exit swapgs too → handler is GS-balanced
    "    swapgs",
    ".Ls2_no_swapout:",
    "    iretq",

    // --- int 0x81 handler: shim done. Capture state, longjmp to the driver. ---
    ".global stage2_int81_entry",
    "stage2_int81_entry:",
    "    swapgs",                      // GS: user -> per-CPU (staying in kernel)
    "    mov [rip + stage2_r15], r15", // T3: r15 marker survived the round-trips?
    "    mov rax, [rsp + 16]",         // ring-3 iret frame: RIP@0 CS@8 RFLAGS@16
    "    mov [rip + stage2_rflags], rax",
    "    mov rsp, [rip + stage2_saved_rsp]",
    "    ret",                         // -> stage2_resume

    // --- driver excursion: enter ring 3 at rdi (RIP), rsi (RSP top). ---
    ".global stage2_excursion",
    "stage2_excursion:",
    "    push rbx",
    "    push rbp",
    "    push r12",
    "    push r13",
    "    push r14",
    "    push r15",
    "    lea rax, [rip + stage2_resume]",
    "    push rax",                    // resume address
    "    mov [rip + stage2_saved_rsp], rsp",
    "    mov rax, [rip + stage2_user_ss]",
    "    push rax",                    // SS
    "    push rsi",                    // RSP (shim stack top)
    "    mov rax, 0x2",                // RFLAGS: reserved bit1=1, IF=0, DF=0
    "    push rax",
    "    mov rax, [rip + stage2_user_cs]",
    "    cmp byte ptr [rip + stage2_mut_bad_iret], 0",
    "    je .Ls2_cs_ok",
    "    mov rax, 0xfff8",             // mutation: malformed CS (past GDT limit) -> iretq #GP
    ".Ls2_cs_ok:",
    "    push rax",                    // CS
    "    push rdi",                    // RIP (shim entry)
    "    swapgs",                      // GS: per-CPU -> user (for ring 3)
    "    iretq",
    "stage2_resume:",                  // int0x81 longjmps here; GS is per-CPU
    "    pop r15",
    "    pop r14",
    "    pop r13",
    "    pop r12",
    "    pop rbp",
    "    pop rbx",
    "    ret",
);

extern "C" {
    static stage2_int80_count: u64;
    static stage2_gs_check: u64;
    static stage2_r15: u64;
    static stage2_rflags: u64;
    static mut stage2_user_cs: u64;
    static mut stage2_user_ss: u64;
    static mut stage2_mut_skip_swapgs: u8;
    static mut stage2_mut_bad_iret: u8;
    fn stage2_excursion(rip: u64, rsp_top: u64);
}

const SHIM_BASE: u64 = 0x0C00_0000;
const SHIM_STACK_TOP: u64 = 0x0C10_0000; // 1 MiB into the US=1 region, grows down
const N: u32 = 100_000;

/// Ring-0 baseline for the transition-cost delta: N trivial calls with no ring
/// change. Same shape as the shim loop (a call + a counter bump) minus the
/// ring3→0→3 transition, so (shim − base)/N ≈ the raw per-syscall transition tax.
#[inline(never)]
fn ring0_trivial(ctr: &core::sync::atomic::AtomicU64) {
    ctr.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    core::hint::black_box(());
}

fn rd(p: *const u64) -> u64 {
    unsafe { core::ptr::read_volatile(p) }
}

/// Run the Stage-2 shim acceptance: clean round-trips (T1/T3) + timing + the
/// swapgs mutation (T2 entry-side, value-detected). With `stage2_mut_iret`, also
/// fires the malformed-iretq-frame mutation (T2 return-side, #GP — halts).
pub fn run() {
    use core::sync::atomic::{AtomicU64, Ordering};
    unsafe {
        // Write the ring-3 shim machine code into the US=1 page.
        let n = N.to_le_bytes();
        let shim: [u8; 24] = [
            0x49, 0xBF, 0xBE, 0xBA, 0xFE, 0xCA, 0x00, 0x00, 0x00, 0x00, // movabs r15, 0xCAFEBABE
            0xFD, // std  (T3 DF marker)
            0x48, 0xC7, 0xC1, n[0], n[1], n[2], n[3], // mov rcx, N
            0xCD, 0x80, // int 0x80
            0xE2, 0xFC, // loop -4
            0xCD, 0x81, // int 0x81
        ];
        core::ptr::copy_nonoverlapping(shim.as_ptr(), SHIM_BASE as *mut u8, shim.len());

        let (ucode, udata) = crate::percpu::user_selectors();
        core::ptr::write_volatile(core::ptr::addr_of_mut!(stage2_user_cs), (ucode | 3) as u64);
        core::ptr::write_volatile(core::ptr::addr_of_mut!(stage2_user_ss), (udata | 3) as u64);

        serial_println!(
            "[stage2] shim @ {:#x}, CS={:#x} SS={:#x}, N={}",
            SHIM_BASE, ucode | 3, udata | 3, N
        );

        // ---- T1/T3: clean round-trip, timed ----
        core::ptr::write_volatile(core::ptr::addr_of_mut!(stage2_mut_skip_swapgs), 0u8);
        x86_64::instructions::interrupts::disable();
        let t0 = core::arch::x86_64::_rdtsc();
        stage2_excursion(SHIM_BASE, SHIM_STACK_TOP);
        let t1 = core::arch::x86_64::_rdtsc();
        core::arch::asm!("cld", options(nomem, nostack)); // clear the DF the shim set
        x86_64::instructions::interrupts::enable();

        let count = rd(core::ptr::addr_of!(stage2_int80_count));
        let r15 = rd(core::ptr::addr_of!(stage2_r15));
        let rflags = rd(core::ptr::addr_of!(stage2_rflags));
        let gs_clean = rd(core::ptr::addr_of!(stage2_gs_check));
        let df_set = (rflags & (1 << 10)) != 0; // RFLAGS.DF is bit 10

        // Ring-0 baseline for the transition-cost delta.
        let ctr = AtomicU64::new(0);
        let b0 = core::arch::x86_64::_rdtsc();
        for _ in 0..N {
            ring0_trivial(&ctr);
        }
        let b1 = core::arch::x86_64::_rdtsc();
        let shim_cyc = t1 - t0;
        let base_cyc = b1 - b0;
        let tax = (shim_cyc.saturating_sub(base_cyc)) / (N as u64);

        // gs_check must be a plausible per-CPU kernel stack top (KSTACK/.bss range).
        let gs_valid = gs_clean >= 0x0070_0000 && gs_clean < 0xA000_0000;
        let t1_ok = count == N as u64;
        let t3_ok = r15 == 0xCAFE_BABE && df_set && gs_valid;
        serial_println!(
            "[stage2] T1 round-trips: count={}/{} -> {}",
            count, N, if t1_ok { "PASS" } else { "FAIL" }
        );
        serial_println!(
            "[stage2] T3 state: r15={:#x} DF={} gs_check={:#x}(valid={}) -> {}",
            r15, df_set, gs_clean, gs_valid, if t3_ok { "PASS" } else { "FAIL" }
        );
        serial_println!(
            "[stage2] TIMING: shim={} base={} cyc; transition tax ~{} cyc/round-trip",
            shim_cyc, base_cyc, tax
        );

        // ---- T2 entry-side mutation: skip swapgs → gs_check reads wrong base ----
        core::ptr::write_volatile(core::ptr::addr_of!(stage2_int80_count) as *mut u64, 0);
        core::ptr::write_volatile(core::ptr::addr_of_mut!(stage2_mut_skip_swapgs), 1u8);
        x86_64::instructions::interrupts::disable();
        stage2_excursion(SHIM_BASE, SHIM_STACK_TOP);
        core::arch::asm!("cld", options(nomem, nostack));
        x86_64::instructions::interrupts::enable();
        core::ptr::write_volatile(core::ptr::addr_of_mut!(stage2_mut_skip_swapgs), 0u8);
        let gs_broken = rd(core::ptr::addr_of!(stage2_gs_check));
        let broken_invalid = !(gs_broken >= 0x0070_0000 && gs_broken < 0xA000_0000);
        // Teeth: the skipped swapgs must be DETECTABLE — gs_check now reads the
        // wrong (user) base, differing from the clean value and out of kernel range.
        let t2_swapgs_ok = broken_invalid && gs_broken != gs_clean;
        serial_println!(
            "[stage2] T2(entry/skip-swapgs): gs_check clean={:#x} broken={:#x} -> caught={} -> {}",
            gs_clean, gs_broken, broken_invalid,
            if t2_swapgs_ok { "PASS" } else { "FAIL" }
        );

        let all = t1_ok && t3_ok && t2_swapgs_ok;
        serial_println!(
            "[stage2] ACCEPTANCE (clean+T3+timing+swapgs-mut): {}",
            if all { "PASS" } else { "FAIL" }
        );

        // ---- T2 return-side mutation: malformed iretq CS → #GP (HALTS). ----
        #[cfg(feature = "stage2_mut_iret")]
        {
            serial_println!(
                "[stage2] T2(return/bad-iret): pushing malformed CS=0xfff8, expect #GP on iretq..."
            );
            core::ptr::write_volatile(core::ptr::addr_of_mut!(stage2_mut_bad_iret), 1u8);
            x86_64::instructions::interrupts::disable();
            stage2_excursion(SHIM_BASE, SHIM_STACK_TOP); // #GP here — never returns
            // If we reach here, the malformed frame did NOT fault — that's a FAIL.
            x86_64::instructions::interrupts::enable();
            serial_println!("[stage2] T2(return/bad-iret): NO FAULT — return-frame mutation FAIL");
        }
        let _ = core::ptr::addr_of!(stage2_mut_bad_iret);
    }
}
