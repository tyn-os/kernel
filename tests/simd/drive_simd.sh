#!/bin/bash
# SIMD-preemption regression test (Tyn test suite).
#
# Guards: "a preemptive context switch preserves the interrupted thread's FPU/SSE
# state and RFLAGS." Regresses if src/interrupts.rs::sched_yield_trampoline stops
# bracketing the syscall yield with fxsave64/fxrstor64 (+ pushfq/popfq), or if
# context_switch's late fxsave64 becomes the only SIMD save again.
#
# Method (see tests/simd/README.md): boot the zero-dep amplifier app, which runs
# 16 workers hashing/crunching a KNOWN input in a tight loop under heavy
# preemption and prints AMP_TOTAL with the count of results that differed from a
# reference computed once. A correct kernel yields mismatches=0; the unfixed
# kernel measured ~4% (34/868).
#
# Behaviour-based and exact (the house rule): asserts the mismatch COUNT, never a
# status. This test is valid on the TCG box (unlike the networking tests): it is
# not timing-sensitive, and slow TCG only increases preemption pressure — a better
# amplifier. Runs on Nitro too (the original #GP was seen there).
#
# Env:
#   KERNEL   kernel ELF               (default /home/ubuntu/kernel/target/x86_64-tyn/release/tyn-kernel)
#   BASECPIO base OTP cpio            (default /home/ubuntu/kernel/src/otp-rootfs.cpio)
#   REL      packed release dir       (default /home/ubuntu/ampapp/_build/prod/rel/ampapp)
#   WORK     scratch dir              (default /home/ubuntu/work)
#   RUNTIME_MS amplifier run length   (default 150000)
set -u
KERNEL="${KERNEL:-/home/ubuntu/kernel/target/x86_64-tyn/release/tyn-kernel}"
BASECPIO="${BASECPIO:-/home/ubuntu/kernel/src/otp-rootfs.cpio}"
REL="${REL:-/home/ubuntu/ampapp/_build/prod/rel/ampapp}"
WORK="${WORK:-/home/ubuntu/work}"
RUNTIME_MS="${RUNTIME_MS:-150000}"
TAG=simd_regress
LOG="$WORK/${TAG}.log"; CPIO="$WORK/${TAG}.cpio"; RAW="$WORK/${TAG}.raw"
rm -f "$LOG"
pkill -9 -f qemu-system 2>/dev/null; sleep 2

"$(dirname "$KERNEL")/../../../tyn-pack" "$REL" --base "$BASECPIO" -o "$CPIO" >/dev/null 2>&1 \
  || /home/ubuntu/kernel/tyn-pack "$REL" --base "$BASECPIO" -o "$CPIO" >/dev/null 2>&1 \
  || { echo "SIMD: FAIL (pack)"; exit 1; }
# mkdisk.sh is committed alongside this script (clean-clone buildable); fall back
# to a host scratch copy only if the tracked one is somehow missing.
MKDISK="$(dirname "$0")/mkdisk.sh"; [ -f "$MKDISK" ] || MKDISK="$WORK/mkdisk.sh"
KERNEL="$KERNEL" bash "$MKDISK" "$CPIO" "$RAW" >/dev/null 2>&1 \
  || { echo "SIMD: FAIL (mkdisk)"; exit 1; }

timeout $(( RUNTIME_MS/1000 + 180 )) qemu-system-x86_64 -accel tcg -cpu max -m 2560M \
  -machine q35 -smp 1 -drive file="$RAW",format=raw,if=ide -no-reboot -nographic \
  -serial "file:$LOG" </dev/null >/dev/null 2>&1 &
QP=$!
for i in $(seq 1 $(( RUNTIME_MS/1000 + 170 )) ); do
  grep -qaE "AMP_TOTAL|#GP|#PF|panic" "$LOG" 2>/dev/null && { sleep 2; break; }
  kill -0 $QP 2>/dev/null || break
  sleep 1
done
kill -9 $QP 2>/dev/null; pkill -9 -f qemu-system 2>/dev/null

clean() { tr -d '\000' < "$LOG" 2>/dev/null; }
crash=$(clean | grep -acE "#GP|#PF|panic")
begin=$(clean | grep -acE "AMP_BEGIN")
# TIMEOUT in the AMP_TOTAL line means some workers never reported — an incomplete
# run, not a clean pass.
timeout_seen=$(clean | grep -acE "AMP_TOTAL.*TIMEOUT")
total_line=$(clean | grep -aoE "AMP_TOTAL small_md5=[0-9]+ large_md5=[0-9]+" | head -1)
small=$(echo "$total_line" | grep -oE "small_md5=[0-9]+" | cut -d= -f2)
large=$(echo "$total_line" | grep -oE "large_md5=[0-9]+" | cut -d= -f2)

echo "--- SIMD / red-zone regression (BUG-1 memory-corruption class) ---"
echo "boot(AMP_BEGIN)=$begin  crash(#GP/#PF/panic)=$crash  timeout=$timeout_seen"
echo "${total_line:-<no AMP_TOTAL — run did not complete>}"
clean | grep -aE "AMP_TRANSIENT|AMP_INPUT_CORRUPT|AMP_REF_BAD" | head -5

# PASS: booted, no fault, all workers reported (no timeout), and BOTH the
# trap-continuation large-md5 (control) AND the no-trap small-md5 anchor came back
# 0. A transient wrong digest under preemption is exactly BUG-1's red-zone clobber.
# Behaviour-based and exact: asserts the COUNT, never a status.
if [ "$begin" = "1" ] && [ "$crash" = "0" ] && [ "$timeout_seen" = "0" ] \
   && [ -n "$small" ] && [ -n "$large" ] && [ "$small" = "0" ] && [ "$large" = "0" ]; then
  echo "SIMD: PASS (small_md5=0 large_md5=0, no fault, run completed)"
  exit 0
else
  echo "SIMD: FAIL (small_md5=${small:-?} large_md5=${large:-?}, crash=$crash, begin=$begin, timeout=$timeout_seen)"
  exit 1
fi
