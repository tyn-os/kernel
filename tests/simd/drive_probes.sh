#!/bin/bash
# Layer-0 register / red-zone probe suite (Tyn test suite).
#
# Guards: "a preemptive timer context switch preserves the interrupted thread's
# register state (FS_BASE, GP r12-15, RFLAGS/DF, XMM) and its red zone." Runs every
# survival probe in ONE boot plus the xmm_poison TEETH, and asserts:
#   - each survival probe returns bad=0 (clean kernel), and
#   - xmm_poison returns bad>0 (proves the spin+readback+compare detection harness
#     actually fires — a probe that can't fail is inert).
# Labels distinguish register (FSBASE/GP/RFLAGS/XMM) from memory (REDZONE), so the
# md5-is-scalar conflation can't recur. See tests/simd/README.md and BUGS.md (BUG-1).
#
# TCG-valid (register/memory, not timing/networking) — slow TCG only raises
# preemption pressure. Needs a probe beam (fsbase_probe static NIF), which is a
# gitignored host artifact; point PROBE_BEAM at it. Behaviour-based: asserts counts.
#
# Env: KDIR (kernel repo), PROBE_BEAM (beam with fsbase_probe NIF), AMPAPP,
#      BASECPIO, WORK, WINDOW_MS (per-probe window ms, default 4000).
set -u
KDIR="${KDIR:-$HOME/kernel}"
PROBE_BEAM="${PROBE_BEAM:-$HOME/work/beam_probes_poison.smp}"
BASECPIO="${BASECPIO:-$KDIR/src/otp-rootfs.cpio}"
AMPAPP="${AMPAPP:-$KDIR/tests/simd/ampapp}"
WORK="${WORK:-$HOME/work}"
WINDOW_MS="${WINDOW_MS:-4000}"
source "$HOME/.asdf/asdf.sh" 2>/dev/null || export PATH="$HOME/.asdf/shims:$PATH"
TAG=probe_suite
LOG="$WORK/${TAG}.log"; CPIO="$WORK/${TAG}.cpio"; RAW="$WORK/${TAG}.raw"; PK="$WORK/tyn-kernel-${TAG}"

[ -f "$PROBE_BEAM" ] || { echo "PROBE-SUITE: FAIL (no probe beam at $PROBE_BEAM — build via beam-build/build-beam.sh)"; exit 1; }

# --- build the probe kernel: swap the probe beam in, build, ALWAYS restore the tree ---
cd "$KDIR" || { echo "PROBE-SUITE: FAIL (no KDIR $KDIR)"; exit 1; }
cp src/beam.smp.elf "$WORK/ps_beam_orig.elf" || exit 1
restore(){ cp "$WORK/ps_beam_orig.elf" "$KDIR/src/beam.smp.elf" 2>/dev/null; }
trap restore EXIT
cp "$PROBE_BEAM" src/beam.smp.elf
cargo build --release --target x86_64-tyn.json \
  -Zbuild-std=core,alloc,compiler_builtins -Zbuild-std-features=compiler-builtins-mem \
  > "$WORK/ps-build.log" 2>&1
RC=$?
[ $RC -eq 0 ] && cp target/x86_64-tyn/release/tyn-kernel "$PK"
restore; trap - EXIT
[ $RC -ne 0 ] && { echo "PROBE-SUITE: FAIL (kernel build: $(tail -1 "$WORK/ps-build.log"))"; exit 1; }

# --- pack ampapp in suite mode + make a disk ---
( cd "$AMPAPP" && MIX_ENV=prod mix release --overwrite ) > "$WORK/ps-rel.log" 2>&1 \
  || { echo "PROBE-SUITE: FAIL (ampapp release: $(tail -1 "$WORK/ps-rel.log"))"; exit 1; }
"$KDIR/tyn-pack" "$AMPAPP/_build/prod/rel/ampapp" --base "$BASECPIO" \
  --env TYN_PROBE=suite --env TYN_SUITE_WINDOW_MS="$WINDOW_MS" --env TYN_AMP_WORKERS=16 \
  -o "$CPIO" >/dev/null 2>&1 || { echo "PROBE-SUITE: FAIL (pack)"; exit 1; }
MKDISK="$(dirname "$0")/mkdisk.sh"; [ -f "$MKDISK" ] || MKDISK="$WORK/mkdisk.sh"
KERNEL="$PK" bash "$MKDISK" "$CPIO" "$RAW" >/dev/null 2>&1 || { echo "PROBE-SUITE: FAIL (mkdisk)"; exit 1; }

# --- boot (TCG -smp1; -display none so a backgrounded qemu can't EOF its monitor) ---
rm -f "$LOG"; pkill -9 -f qemu-system 2>/dev/null; sleep 1
timeout 240 qemu-system-x86_64 -accel tcg -cpu max -m 2560M -machine q35 -smp 1 -snapshot \
  -drive file="$RAW",format=raw,if=ide -no-reboot -display none -serial "file:$LOG" \
  </dev/null >/dev/null 2>&1 &
QP=$!
for i in $(seq 1 220); do
  [ -f "$LOG" ] && tr -d '\000' <"$LOG" 2>/dev/null | grep -qaE "SUITE_DONE|#GP|#PF|panic" && { sleep 1; break; }
  kill -0 $QP 2>/dev/null || break
  sleep 1
done
kill -9 $QP 2>/dev/null; pkill -9 -f qemu-system 2>/dev/null

# --- assert ---
clean(){ [ -f "$LOG" ] && tr -d '\000' <"$LOG" 2>/dev/null; }
crash=$(clean | grep -acE "#GP|#PF|panic")
done_=$(clean | grep -acE "SUITE_DONE")
echo "--- register / red-zone probe suite (Layer 0, BUG-1 class) ---"
fail=0
for L in FSBASE GP RFLAGS XMM REDZONE; do
  line=$(clean | grep -aoE "${L}_TOTAL bad=[0-9]+ spans=[0-9]+" | head -1)
  if [ -n "$line" ]; then
    bad=$(echo "$line" | grep -oE "bad=[0-9]+" | cut -d= -f2)
    echo "  $line  [$([ "$bad" = 0 ] && echo OK || echo BAD)]"
    [ "$bad" = 0 ] || fail=1
  elif clean | grep -qaE "${L}_UNAVAILABLE"; then
    echo "  ${L}: UNAVAILABLE (not exported by probe beam) — skipped"
  else
    echo "  ${L}: MISSING (no result line)"; fail=1
  fi
done
# TEETH: xmm_poison must fire (bad>0)
pline=$(clean | grep -aoE "XMMPOISON_TOTAL bad=[0-9]+ spans=[0-9]+" | head -1)
pbad=$(echo "$pline" | grep -oE "bad=[0-9]+" | cut -d= -f2)
teeth_ok=0; [ -n "$pbad" ] && [ "$pbad" -gt 0 ] && teeth_ok=1
echo "  TEETH ${pline:-<missing>}  [$([ "$teeth_ok" = 1 ] && echo FIRES || echo INERT)]"
echo "boot(SUITE_DONE)=$done_  crash(#GP/#PF/panic)=$crash"

if [ "$done_" = 1 ] && [ "$crash" = 0 ] && [ "$fail" = 0 ] && [ "$teeth_ok" = 1 ]; then
  echo "PROBE-SUITE: PASS (survival probes bad=0; xmm_poison teeth fires; no fault)"
  exit 0
else
  echo "PROBE-SUITE: FAIL (survival_fail=$fail teeth_ok=$teeth_ok done=$done_ crash=$crash)"
  exit 1
fi
