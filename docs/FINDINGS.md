# Program-level findings — where defects live

A running log of defects that turned up **and why prior testing missed them** —
kept here, not only in commit history, so the *pattern* (where to look next) is
visible in one place. Each entry is a data point on where bugs concentrate.

## 2026-08-14 — latent cpio OOB / underflow, found by extraction + boundary testing

**Defect.** `vfs.rs::cpio_lookup` computed `name_start + namesize - 1` and
`data_start + filesize` with unchecked arithmetic and unchecked slicing. A
malformed archive with `namesize == 0` underflows `namesize - 1` (panic in a
debug build, wrap in release); oversized name/file size fields overflow the adds
*before* the `> data.len()` bounds check, defeating it. Result on hostile input:
a panic or an out-of-bounds read — the classic remote-input parser hazard, on the
path that loads every `.beam`.

**Why every prior test missed it.** The embedded cpio archive is always
well-formed. No boot, no capability test, no probe ever handed the parser a
malformed header — so the happy path was exercised constantly and the failure
path never once. A green suite that only feeds valid input proves the parser
parses, not that it *refuses* garbage safely.

**How it surfaced.** Phase-2 Layer-1: extracting the pure parser into
`src/cpio.rs` and writing boundary tests (truncated / bad-magic / zero-namesize /
oversized-name / huge-filesize) against it. The zero-namesize and size-overflow
cases are the teeth. Fixed with `checked_add`/`checked_sub` + `slice::get()` + a
`namesize == 0` guard — malformed input now returns `None`, never panics/OOBs.
Boot-verified behavior-preserving on the real (well-formed) archive
(`AMP_BEGIN=1`, `small_md5=0 large_md5=0`, no fault).

**Crash eliminated, not relocated — caller trace.** Returning `None` on malformed
input only helps if callers don't then `.unwrap()` it. Audited: `cpio_lookup` has
exactly two callers, neither of which unwraps — `vfs::exists` maps `None → false`
(`.is_some()`), and `vfs::open` maps `None → return -2` (-ENOENT). No `unwrap` or
`expect` on the lookup exists anywhere in `src/`. So the malformed path traces
end to end: malformed archive → `lookup` returns `None` (unit-tested) → `open`
returns `-ENOENT` / `exists` returns `false` → no panic at the parser *or* the
caller. (The parser's boundary tests cover the parser half; this audit covers the
caller half — the well-formed boot-verify only exercises the happy path.)

Note the caller-side `None` arm is not merely audited — it is exercised at *every*
boot: the kernel does many `cpio_lookup`s for paths that are legitimately absent
(the newc format has always returned `None` on not-found), and each takes the
same `None → -ENOENT / false` arm a malformed archive now takes. The change only
moved malformed input from "panic in the parser" onto that already-boot-proven
`None` path.

**The data point.** Defects concentrate on the paths real workloads never take
(cf. BUG-1: the SMP-only IPI vector, exercised by nothing until a probe forced
it). The Layer-1/2/3 program — unit boundaries, adversarial input, syscall fuzz —
is aimed squarely at those unreached paths, because that is where the next one
will be.
