# Isolation — kernel↔BEAM copy-site boundary (Stage 3b, enforced + validated)

> **STATUS: ENFORCED.** BEAM runs at ring 3 with its pages US=1; the kernel runs at ring 0
> with SMEP+SMAP (CR4 bits 20/21) set on the BSP and every AP. Under SMAP a raw ring-0
> deref of a US=1 (BEAM) page faults. Every kernel access to BEAM memory therefore goes
> through the **one audited path** in `src/uaccess.rs`, which bounds-checks the pointer
> *before* opening a short `stac`/`clac` window. This is what makes "small TCB" an
> *enforced* boundary, not just a small codebase.

## The one audited path (`src/uaccess.rs`)

- `with_user_access(ptr, len, f)` — validate `ptr..ptr+len` via `paging::user_accessible`,
  then `stac` → run the closure `f` (the in-place access) → `clac`. Returns `Err(EFAULT=-14)`
  if validation fails, so the window never opens on a bad pointer.
- `copy_from_user(dst, src)` / `copy_to_user(dst, src)` — the bulk pair (validate → stac →
  `copy_nonoverlapping` → clac).
- `copy_user_cstr(dst, src)` — page-safe NUL-scan, per-byte validated (bounds the path-string
  scan to the user region + buffer length).
- `read_user_u32` / `write_user_u32` — the futex/tid word primitives.
- `paging::user_accessible(addr, len)` walks PML4→PDPT→PD→PTE and requires **PRESENT+US at
  every level** (US is AND-ed across levels), rejecting `end > 4·GiB`. This is the
  confused-deputy bounds-check — the thing SMAP alone cannot give (SMAP stops a *direct* BEAM
  deref of kernel memory; the bounds-check stops the kernel being *tricked* into dereferencing
  a kernel address on BEAM's behalf).

> ⚠️ **INVARIANT:** `stac`/`clac` must be compiler **memory barriers** (no `options(nomem)`).
> With `nomem` the compiler is free to reorder the guarded access *outside* the AC window, so
> it runs with AC=0 and SMAP-faults — a silent hole across *every* site. This bit us once
> (`sys_writev`'s `copy_nonoverlapping` stayed a fault-site after wrapping); the fix + the
> invariant comment live at the `stac`/`clac` definitions.

## How the list was validated (not just read)

Stage 1 produced this list by *reading* the handlers. Stage 3b *validated* it empirically:

1. **Enumeration (feature `smap_hunt`, free on TCG):** an auto-recover #PF handler logs every
   ring-0 fault to a PRESENT/US=1 page (`[smap-site] ip=… cr2=… ret=…`), sets AC, and retries,
   so **one** boot+serve run enumerates every copy site the workload reaches instead of halting
   at the first. The initial serving run surfaced **35 sites across ~20 functions** — the
   read/write/socket/stat/futex/poll cluster this draft predicted, plus a few reading missed.
2. **Wrap → re-hunt → converge:** each site wrapped through the guard above, tracked by
   **function name** (every wrap recompiles and shifts kernel addresses, so raw IPs are
   useless — resolve with `nm -nC` per run). Converged when a run surfaced nothing new.
3. **Strict-build convergence oracle:** the shipping build (no `smap_hunt`, real guards,
   item-4 halts a ring-0 fault) is the under-wrap *and* over-wrap check — it halts at the
   first unwrapped *exercised* site, and EFAULTs if a kernel-internal op was wrongly guarded.
   It boots + serves clean.

`~30` functions are now wrapped (the tables below).

## Wrapped copy sites

Per-**pointer** classification (not per-callsite): e.g. `sendto` reads the user buffer via
`send_slice` (guard the source read) while the smoltcp buffer is kernel (left alone);
`recvfrom` writes the user buffer (guard the dest). Bulk `memcpy`/`memset` are wrapped at
their **callers**, never inside the shared routine.

### `src/syscall.rs`
`sys_write`, `sys_read` (urandom/resolv/synth/socket), `sys_open`/`openat` (→`copy_user_cstr`),
`sys_readlink`, `sys_writev` + `readv` (→`read_user_iovec`, the double-deref), `sys_getcwd`,
`sys_uname`, `sys_sched_getaffinity`, `sys_getrusage`, `sys_clock_gettime`, `sys_clock_getres`,
`sys_prlimit64`, `sys_getrandom`, `sys_pipe`, `sys_timerfd_settime`, `sys_stat` / `sys_fstat`
(build-then-copy 144 B), `sys_newfstatat` (guarded probe), `sys_ppoll` (timeout + pollfd loop),
`sys_epoll_ctl` (read event) / `sys_epoll_wait` (write events), `sys_futex` (timespec + word).

### `src/net/socket.rs`
`sys_connect`, `sys_bind`, `sys_accept`, `sys_getsockname`, `sys_getpeername` (shared
`read_sockaddr_in` / `write_sockaddr_in`), `sys_getsockopt` (optlen read + optval/optlen
writes), `sys_sendto` (`send_slice` source + dest addr), `sys_recvfrom` (`recv_slice` dest +
src addr).

### elsewhere
`src/tmpfs.rs` `write_at`/`read_at` + `write_stat`; `src/vfs.rs` `read` + `fill_dir_entries`;
`src/pipe.rs` `read`/`write`; `src/sched.rs` `spawn` (parent/child tid), `futex_wait_until`,
`futex_wake`, `watchdog_wake`.

## Sites that needed special care

- **iovec double-deref (`writev`/`readv`):** the `iov` array *and* each `iov_base` are user
  pointers. `read_user_iovec` copies the iovec array in first, then each `base` is validated
  before touching it — a naive single `copy_from_user` would miss the inner pointers.
- **futex `uaddr`:** the word is read/written on a hot blocking path; wrapped via
  `read_user_u32`/`write_user_u32` (single guarded word access, not copy-per-iteration).
- **Path strings (`open`/`stat`/`readlink`/`connect`):** length is a NUL-scan of user memory;
  `copy_user_cstr` bounds it to the user region + a max length (no unbounded read).
- **Hot serving path (`read`/`write`/`recvfrom`/`sendto`):** carries the socket data. The
  per-copy bounds-check cost was **measured negligible** — 16-vCPU Nitro single-conn bulk held
  **~12.5 MB/s across 1/10/50 MB** (matching the pre-guard T-cost baseline).

## Completeness — bounded exactly as validated

The boundary is **enforced everywhere** (SMAP faults any unwrapped ring-0 access to BEAM
memory). Empirical *exercise* coverage is stated honestly, not rounded up:

- **Exercised + strict-validated:** HTTP serving · TLS handshake · 50 MB bulk transfer
  (16-vCPU real SMP, Nitro) · inbound dist listener bind/accept/recv + `net_kernel` · file I/O
  (tmpfs write/read, stat/open/read code-loading path) — all on the shipping strict build with
  **zero SMAP faults / zero halts**.
- **Code-audited, NOT independently exercised:** the outbound-`connect` sockaddr read. It
  shares `read_sockaddr_in` with `bind` (which *is* exercised), and was read-audited — but a
  workload that drives outbound `connect` under the enumerator has not been run. Flagged here
  so that if someone later exercises it and finds an issue, the doc already scoped it.

Threat model v1 = contain a BEAM memory-safety fault from the kernel (both directions proven
by violation: Stage 3a — BEAM can't reach kernel memory directly; Stage 3b — BEAM can't trick
the kernel into it either, confused-deputy → EFAULT, mutation-proven). Out of scope: process-
vs-process inside BEAM, Meltdown/Spectre (needs Stage 4 address-space separation), NIF
sandboxing. The JIT region is user-RWX (named reduced-hardening spot).

## Known-separate (not a boundary gap)

**HTTPS bulk = 0** — the TLS handshake completes and small responses serve, but bulk bodies
over TLS drop. This reproduces on the pre-ring-3-flip baseline, so it is a **pre-existing**
issue, not an isolation regression. It also means the HTTPS bulk-body copy sites are not yet
exercise-covered by the run above; they will be once that follow-up is fixed.
