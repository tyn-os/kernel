/* FS_BASE-preemption detector NIF for Tyn (directions/SCALAR_STATE_ELIM.md).
 *
 * Purpose: the dose-response pinned BUG-1's corruptor to Tyn's PREEMPTIVE
 * context-switch path; elimination (syscall_entry saves all GPRs + RFLAGS; md5
 * is scalar so XMM is irrelevant) leaves FS_BASE / TLS as the leading suspect.
 * Code reading found it: `context_switch` (src/thread.rs) restores rsp+callee-
 * saved GPRs but NOT FS_BASE, while the idle-resume path (src/sched.rs) does
 * `wrmsr(0xC000_0100)` — "same invariant as context_switch", which context_switch
 * does not actually uphold. And `sys_arch_prctl(ARCH_SET_FS)` writes the MSR but
 * never updates ctx.fs_base.
 *
 * This probe is the scalar sibling of xmm_probe: it keeps a KNOWN scalar value —
 * the thread's own FS_BASE — held across a long, CALL-FREE spin so a timer
 * preemption lands while it is the live TLS base, then checks it survived. In
 * musl the TCB sits at FS_BASE and TCB[0] is a self-pointer, so `mov %fs:0, reg`
 * reads back FS_BASE itself. If a preemptive context switch resumes this thread
 * with another thread's FS_BASE (not restored), %fs:0 comes back != the value
 * captured before the spin. The spin does NOT touch fs/TLS, so a mismatch means
 * the *register state* (FS_BASE) was lost across the preemption — a true FS_BASE
 * detector, able to say "not this" (returns 0) if FS_BASE is in fact preserved.
 *
 * probe(Outer, Spin) -> integer(): count of the Outer live-spans in which %fs:0
 * (== FS_BASE) came back != its pre-spin value.
 *
 * Erlang side: src/erl/fsbase_probe.erl calls erlang:load_nif("fsbase_probe", 0).
 * Built into beam.smp via --enable-static-nifs (compiled -DSTATIC_ERLANG_NIF). */
#include <erl_nif.h>
#include <stdint.h>

static ERL_NIF_TERM probe_nif(ErlNifEnv *env, int argc, const ERL_NIF_TERM argv[]) {
    long outer, spin;
    if (argc != 2 ||
        !enif_get_long(env, argv[0], &outer) ||
        !enif_get_long(env, argv[1], &spin)) {
        return enif_make_badarg(env);
    }

    long bad = 0;

    for (long i = 0; i < outer; i++) {
        uint64_t tp0 = 0, tp1 = 0;
        long cnt = spin;
        /* Read FS_BASE (musl: %fs:0 = TCB self-pointer = fs_base), spin on a GP
         * counter (call-free, no fs/TLS touch), then read FS_BASE again. FS_BASE
         * stays the live TLS base across the whole loop; a preemptive context
         * switch that resumes this thread without restoring FS_BASE makes the
         * second read return another thread's base. */
        __asm__ __volatile__ (
            "mov %%fs:0, %1\n\t"
            "1:\n\t"
            "dec %0\n\t"
            "jnz 1b\n\t"
            "mov %%fs:0, %2\n\t"
            : "+r"(cnt), "=r"(tp0), "=r"(tp1)
            :
            : "cc"
        );
        if (tp1 != tp0) bad++;
    }

    return enif_make_long(env, bad);
}

/* GP-register survival: hold knowns in callee-saved r12-r15 across a call-free
 * spin, verify. syscall_entry + context_switch both save these; a mismatch means
 * the preemptive path drops a GP reg. */
static ERL_NIF_TERM gp_probe_nif(ErlNifEnv *env, int argc, const ERL_NIF_TERM argv[]) {
    long outer, spin;
    if (argc != 2 || !enif_get_long(env, argv[0], &outer) || !enif_get_long(env, argv[1], &spin))
        return enif_make_badarg(env);
    const uint64_t V12=0x1212121212121212ULL, V13=0x1313131313131313ULL,
                   V14=0x1414141414141414ULL, V15=0x1515151515151515ULL;
    long bad = 0;
    for (long i = 0; i < outer; i++) {
        uint64_t o12=0,o13=0,o14=0,o15=0; long cnt = spin;
        __asm__ __volatile__(
            "mov %[v12], %%r12\n\t" "mov %[v13], %%r13\n\t"
            "mov %[v14], %%r14\n\t" "mov %[v15], %%r15\n\t"
            "1:\n\t" "dec %[cnt]\n\t" "jnz 1b\n\t"
            "mov %%r12, %[o12]\n\t" "mov %%r13, %[o13]\n\t"
            "mov %%r14, %[o14]\n\t" "mov %%r15, %[o15]\n\t"
            : [cnt]"+r"(cnt), [o12]"=&r"(o12), [o13]"=&r"(o13), [o14]"=&r"(o14), [o15]"=&r"(o15)
            : [v12]"r"(V12), [v13]"r"(V13), [v14]"r"(V14), [v15]"r"(V15)
            : "r12","r13","r14","r15","cc");
        if (o12!=V12 || o13!=V13 || o14!=V14 || o15!=V15) bad++;
    }
    return enif_make_long(env, bad);
}

/* RFLAGS/DF survival: set DF, hold it across a call-free spin, verify still set.
 * Tyn history: DF-not-preserved corrupted beam_load. Sets DF during the span, so
 * run it in its own boot (a preemption landing while DF=1 exercises the kernel's
 * cld/popfq path). */
static ERL_NIF_TERM rflags_probe_nif(ErlNifEnv *env, int argc, const ERL_NIF_TERM argv[]) {
    long outer, spin;
    if (argc != 2 || !enif_get_long(env, argv[0], &outer) || !enif_get_long(env, argv[1], &spin))
        return enif_make_badarg(env);
    long bad = 0;
    for (long i = 0; i < outer; i++) {
        uint64_t fl = 0; long cnt = spin;
        __asm__ __volatile__(
            "std\n\t"
            "1:\n\t" "dec %[cnt]\n\t" "jnz 1b\n\t"
            "pushfq\n\t" "pop %[fl]\n\t" "cld\n\t"
            : [cnt]"+r"(cnt), [fl]"=r"(fl) : : "cc","memory");
        if (!(fl & (1ULL << 10))) bad++;   /* DF = bit 10 */
    }
    return enif_make_long(env, bad);
}

/* Hardened XMM survival: hold knowns in ALL xmm0-15 (the old xmm_probe used only
 * xmm0/xmm1) across a call-free spin, verify. Tests whether sched.rs:1184's LATE
 * fxsave (saves kernel-clobbered XMM) corrupts registers a memcpy would use. */
static ERL_NIF_TERM xmm_probe_nif(ErlNifEnv *env, int argc, const ERL_NIF_TERM argv[]) {
    long outer, spin;
    if (argc != 2 || !enif_get_long(env, argv[0], &outer) || !enif_get_long(env, argv[1], &spin))
        return enif_make_badarg(env);
    long bad = 0;
    for (long i = 0; i < outer; i++) {
        uint64_t kn[16], ob[16]; long cnt = spin;
        for (int j = 0; j < 16; j++) { kn[j] = 0xA5A5000000000000ULL + (uint64_t)(j+1); ob[j] = 0; }
        __asm__ __volatile__(
            "movq   0(%[k]), %%xmm0\n\t"  "movq   8(%[k]), %%xmm1\n\t"
            "movq  16(%[k]), %%xmm2\n\t"  "movq  24(%[k]), %%xmm3\n\t"
            "movq  32(%[k]), %%xmm4\n\t"  "movq  40(%[k]), %%xmm5\n\t"
            "movq  48(%[k]), %%xmm6\n\t"  "movq  56(%[k]), %%xmm7\n\t"
            "movq  64(%[k]), %%xmm8\n\t"  "movq  72(%[k]), %%xmm9\n\t"
            "movq  80(%[k]), %%xmm10\n\t" "movq  88(%[k]), %%xmm11\n\t"
            "movq  96(%[k]), %%xmm12\n\t" "movq 104(%[k]), %%xmm13\n\t"
            "movq 112(%[k]), %%xmm14\n\t" "movq 120(%[k]), %%xmm15\n\t"
            "1:\n\t" "dec %[cnt]\n\t" "jnz 1b\n\t"
            "movq %%xmm0,   0(%[o])\n\t"  "movq %%xmm1,   8(%[o])\n\t"
            "movq %%xmm2,  16(%[o])\n\t"  "movq %%xmm3,  24(%[o])\n\t"
            "movq %%xmm4,  32(%[o])\n\t"  "movq %%xmm5,  40(%[o])\n\t"
            "movq %%xmm6,  48(%[o])\n\t"  "movq %%xmm7,  56(%[o])\n\t"
            "movq %%xmm8,  64(%[o])\n\t"  "movq %%xmm9,  72(%[o])\n\t"
            "movq %%xmm10, 80(%[o])\n\t"  "movq %%xmm11, 88(%[o])\n\t"
            "movq %%xmm12, 96(%[o])\n\t"  "movq %%xmm13,104(%[o])\n\t"
            "movq %%xmm14,112(%[o])\n\t"  "movq %%xmm15,120(%[o])\n\t"
            : [cnt]"+r"(cnt)
            : [k]"r"(kn), [o]"r"(ob)
            : "xmm0","xmm1","xmm2","xmm3","xmm4","xmm5","xmm6","xmm7",
              "xmm8","xmm9","xmm10","xmm11","xmm12","xmm13","xmm14","xmm15","memory","cc");
        for (int j = 0; j < 16; j++) if (ob[j] != kn[j]) { bad++; break; }
    }
    return enif_make_long(env, bad);
}

/* Detection teeth-test for xmm_probe: IDENTICAL to xmm_probe_nif but deliberately
 * zeroes xmm0 (pxor) AFTER the spin, BEFORE the readback — so ob[0] != kn[0] and
 * every span is counted bad. A positive control that proves the readback+compare
 * +count path actually FIRES on a wrong XMM value (the sound-by-construction spin
 * proves the value is held LIVE; this proves the check DETECTS corruption). Must
 * return bad == outer. If this returns 0 the detector is inert — the "measures
 * nothing, reads clean" trap this arc has hit (md5-is-scalar, under-dosed fsbase). */
static ERL_NIF_TERM xmm_poison_nif(ErlNifEnv *env, int argc, const ERL_NIF_TERM argv[]) {
    long outer, spin;
    if (argc != 2 || !enif_get_long(env, argv[0], &outer) || !enif_get_long(env, argv[1], &spin))
        return enif_make_badarg(env);
    long bad = 0;
    for (long i = 0; i < outer; i++) {
        uint64_t kn[16], ob[16]; long cnt = spin;
        for (int j = 0; j < 16; j++) { kn[j] = 0xA5A5000000000000ULL + (uint64_t)(j+1); ob[j] = 0; }
        __asm__ __volatile__(
            "movq   0(%[k]), %%xmm0\n\t"  "movq   8(%[k]), %%xmm1\n\t"
            "movq  16(%[k]), %%xmm2\n\t"  "movq  24(%[k]), %%xmm3\n\t"
            "movq  32(%[k]), %%xmm4\n\t"  "movq  40(%[k]), %%xmm5\n\t"
            "movq  48(%[k]), %%xmm6\n\t"  "movq  56(%[k]), %%xmm7\n\t"
            "movq  64(%[k]), %%xmm8\n\t"  "movq  72(%[k]), %%xmm9\n\t"
            "movq  80(%[k]), %%xmm10\n\t" "movq  88(%[k]), %%xmm11\n\t"
            "movq  96(%[k]), %%xmm12\n\t" "movq 104(%[k]), %%xmm13\n\t"
            "movq 112(%[k]), %%xmm14\n\t" "movq 120(%[k]), %%xmm15\n\t"
            "1:\n\t" "dec %[cnt]\n\t" "jnz 1b\n\t"
            "pxor %%xmm0, %%xmm0\n\t"     /* POISON: corrupt xmm0 -> ob[0] must mismatch */
            "movq %%xmm0,   0(%[o])\n\t"  "movq %%xmm1,   8(%[o])\n\t"
            "movq %%xmm2,  16(%[o])\n\t"  "movq %%xmm3,  24(%[o])\n\t"
            "movq %%xmm4,  32(%[o])\n\t"  "movq %%xmm5,  40(%[o])\n\t"
            "movq %%xmm6,  48(%[o])\n\t"  "movq %%xmm7,  56(%[o])\n\t"
            "movq %%xmm8,  64(%[o])\n\t"  "movq %%xmm9,  72(%[o])\n\t"
            "movq %%xmm10, 80(%[o])\n\t"  "movq %%xmm11, 88(%[o])\n\t"
            "movq %%xmm12, 96(%[o])\n\t"  "movq %%xmm13,104(%[o])\n\t"
            "movq %%xmm14,112(%[o])\n\t"  "movq %%xmm15,120(%[o])\n\t"
            : [cnt]"+r"(cnt)
            : [k]"r"(kn), [o]"r"(ob)
            : "xmm0","xmm1","xmm2","xmm3","xmm4","xmm5","xmm6","xmm7",
              "xmm8","xmm9","xmm10","xmm11","xmm12","xmm13","xmm14","xmm15","memory","cc");
        for (int j = 0; j < 16; j++) if (ob[j] != kn[j]) { bad++; break; }
    }
    return enif_make_long(env, bad);
}

/* Red-zone survival: write a sentinel into the 128-byte SysV red zone
 * [rsp-8 .. rsp-128] below this (non-leaf, so red-zone-unused) frame's rsp, spin
 * call-free (rsp stable), read it back. A timer preempt of the spin makes
 * sched_yield_trampoline write the saved RIP + rax/rcx/r11 to [rsp-8..-32] — the
 * interrupted thread's red zone — so the sentinel at offsets -8/-16/-24/-32 comes
 * back clobbered. This is the memory sibling of the register probes: it can only
 * trip if preemption corrupts red-zone stack memory (the leaf-spill corruption
 * that would hit md5/bincopy hot loops). Returns count of clobbered spans. */
static ERL_NIF_TERM redzone_probe_nif(ErlNifEnv *env, int argc, const ERL_NIF_TERM argv[]) {
    long outer, spin;
    if (argc != 2 || !enif_get_long(env, argv[0], &outer) || !enif_get_long(env, argv[1], &spin))
        return enif_make_badarg(env);
    long bad = 0;
    for (long i = 0; i < outer; i++) {
        uint64_t sent[16], out[16]; long cnt = spin;
        for (int j = 0; j < 16; j++) { sent[j] = 0x5EED000000000000ULL + (uint64_t)(j+1); out[j] = 0; }
        __asm__ __volatile__(
            /* copy 16 sentinels from [s] (frame, above rsp) into red zone below rsp */
            "movq   0(%[s]), %%rax\n\t" "movq %%rax,   -8(%%rsp)\n\t"
            "movq   8(%[s]), %%rax\n\t" "movq %%rax,  -16(%%rsp)\n\t"
            "movq  16(%[s]), %%rax\n\t" "movq %%rax,  -24(%%rsp)\n\t"
            "movq  24(%[s]), %%rax\n\t" "movq %%rax,  -32(%%rsp)\n\t"
            "movq  32(%[s]), %%rax\n\t" "movq %%rax,  -40(%%rsp)\n\t"
            "movq  40(%[s]), %%rax\n\t" "movq %%rax,  -48(%%rsp)\n\t"
            "movq  48(%[s]), %%rax\n\t" "movq %%rax,  -56(%%rsp)\n\t"
            "movq  56(%[s]), %%rax\n\t" "movq %%rax,  -64(%%rsp)\n\t"
            "movq  64(%[s]), %%rax\n\t" "movq %%rax,  -72(%%rsp)\n\t"
            "movq  72(%[s]), %%rax\n\t" "movq %%rax,  -80(%%rsp)\n\t"
            "movq  80(%[s]), %%rax\n\t" "movq %%rax,  -88(%%rsp)\n\t"
            "movq  88(%[s]), %%rax\n\t" "movq %%rax,  -96(%%rsp)\n\t"
            "movq  96(%[s]), %%rax\n\t" "movq %%rax, -104(%%rsp)\n\t"
            "movq 104(%[s]), %%rax\n\t" "movq %%rax, -112(%%rsp)\n\t"
            "movq 112(%[s]), %%rax\n\t" "movq %%rax, -120(%%rsp)\n\t"
            "movq 120(%[s]), %%rax\n\t" "movq %%rax, -128(%%rsp)\n\t"
            "1:\n\t" "dec %[cnt]\n\t" "jnz 1b\n\t"
            /* read the red zone back into [o] (frame, above rsp) */
            "movq   -8(%%rsp), %%rax\n\t" "movq %%rax,   0(%[o])\n\t"
            "movq  -16(%%rsp), %%rax\n\t" "movq %%rax,   8(%[o])\n\t"
            "movq  -24(%%rsp), %%rax\n\t" "movq %%rax,  16(%[o])\n\t"
            "movq  -32(%%rsp), %%rax\n\t" "movq %%rax,  24(%[o])\n\t"
            "movq  -40(%%rsp), %%rax\n\t" "movq %%rax,  32(%[o])\n\t"
            "movq  -48(%%rsp), %%rax\n\t" "movq %%rax,  40(%[o])\n\t"
            "movq  -56(%%rsp), %%rax\n\t" "movq %%rax,  48(%[o])\n\t"
            "movq  -64(%%rsp), %%rax\n\t" "movq %%rax,  56(%[o])\n\t"
            "movq  -72(%%rsp), %%rax\n\t" "movq %%rax,  64(%[o])\n\t"
            "movq  -80(%%rsp), %%rax\n\t" "movq %%rax,  72(%[o])\n\t"
            "movq  -88(%%rsp), %%rax\n\t" "movq %%rax,  80(%[o])\n\t"
            "movq  -96(%%rsp), %%rax\n\t" "movq %%rax,  88(%[o])\n\t"
            "movq -104(%%rsp), %%rax\n\t" "movq %%rax,  96(%[o])\n\t"
            "movq -112(%%rsp), %%rax\n\t" "movq %%rax, 104(%[o])\n\t"
            "movq -120(%%rsp), %%rax\n\t" "movq %%rax, 112(%[o])\n\t"
            "movq -128(%%rsp), %%rax\n\t" "movq %%rax, 120(%[o])\n\t"
            : [cnt]"+r"(cnt)
            : [s]"r"(sent), [o]"r"(out)
            : "rax","memory","cc");
        for (int j = 0; j < 16; j++) if (out[j] != sent[j]) { bad++; break; }
    }
    return enif_make_long(env, bad);
}

static ErlNifFunc nif_funcs[] = {
    {"probe", 2, probe_nif},
    {"gp_probe", 2, gp_probe_nif},
    {"rflags_probe", 2, rflags_probe_nif},
    {"xmm_probe", 2, xmm_probe_nif},
    {"xmm_poison", 2, xmm_poison_nif},
    {"redzone_probe", 2, redzone_probe_nif},
};

/* Module name MUST match the Erlang module (fsbase_probe). */
ERL_NIF_INIT(fsbase_probe, nif_funcs, NULL, NULL, NULL, NULL)
