#!/bin/bash
# Phase-2 Layer-2 (resiliency/adversarial): the tmpfs 4 MiB byte-cap under
# CONCURRENT writers racing the boundary — the Layer-1 deferral, tested in situ.
# Boots the cap_probe app (16 writers x 512 KiB = 8 MiB demand vs a 4 MiB cap)
# and asserts the three-part Layer-2 verdict from its CC_ output:
#   TEETH  cap genuinely contended (some writers win, some hit ENOSPC)
#   (1) clean handling   no non-ENOSPC error; node alive after the storm
#   (2) no corruption     total never exceeds the cap; no file holds foreign bytes
#   (3) recovery          a fresh write succeeds byte-exact after space is freed
#
# Behaviour-based (the house rule): asserts CC_ content, never a status code.
# Valid on TCG — this is in-guest FS concurrency + preemption, not network
# timing; slow TCG only amplifies the scheduling race. Run on Nitro too for real
# SMP parallelism contending the coarse Mutex.
#
# Env: KERNEL, BASECPIO, REL, WORK (defaults below), same idiom as drive_simd.sh.
set -u
KERNEL="${KERNEL:-/home/ubuntu/kernel/target/x86_64-tyn/release/tyn-kernel}"
BASECPIO="${BASECPIO:-/home/ubuntu/kernel/src/otp-rootfs.cpio}"
REL="${REL:-/home/ubuntu/cap_probe/_build/prod/rel/cap_probe}"
WORK="${WORK:-/home/ubuntu/work}"
TAG=cap_concurrency
LOG="$WORK/${TAG}.log"; CPIO="$WORK/${TAG}.cpio"; RAW="$WORK/${TAG}.raw"
rm -f "$LOG"
pkill -9 -f qemu-system 2>/dev/null; sleep 2

/home/ubuntu/kernel/tyn-pack "$REL" --base "$BASECPIO" -o "$CPIO" >/dev/null 2>&1 \
  || { echo "CAP: FAIL (pack)"; exit 1; }
MKDISK="$(dirname "$0")/../simd/mkdisk.sh"; [ -f "$MKDISK" ] || MKDISK="$WORK/mkdisk.sh"
KERNEL="$KERNEL" bash "$MKDISK" "$CPIO" "$RAW" >/dev/null 2>&1 \
  || { echo "CAP: FAIL (mkdisk)"; exit 1; }

timeout 240 qemu-system-x86_64 -accel tcg -cpu max -m 2560M -machine q35 -smp 1 \
  -drive file="$RAW",format=raw,if=ide -no-reboot -nographic -serial "file:$LOG" \
  </dev/null >/dev/null 2>&1 &
QP=$!
for i in $(seq 1 230); do
  grep -qaE "CC_END|#GP|#PF|panic" "$LOG" 2>/dev/null && { sleep 2; break; }
  kill -0 $QP 2>/dev/null || break
  sleep 1
done
kill -9 $QP 2>/dev/null; pkill -9 -f qemu-system 2>/dev/null

clean() { tr -d '\000' < "$LOG" 2>/dev/null; }
echo "--- Phase-2 Layer-2 / tmpfs cap under concurrency ---"
clean | grep -aE "^CC_|\[tmpfs\]"
echo "----"

crash=$(clean | grep -acE "#GP|#PF|panic")
begin=$(clean | grep -acE "CC_BEGIN")
field() { clean | grep -aE "^$1 " | tail -1 | grep -oE "$2=[^ ]+" | head -1 | cut -d= -f2; }

other=$(field CC_RESULTS other)
within=$(field CC_TOTAL within_cap)
corrupt=$(field CC_CORRUPT count)
teeth=$(field CC_TEETH contended)
recov=$(field CC_RECOVERY ok)
alive=$(field CC_END node_alive)
okc=$(field CC_RESULTS ok); enc=$(field CC_RESULTS enospc); tot=$(field CC_TOTAL bytes)

fail=0
chk() { if [ "$2" = "$3" ]; then echo "  PASS  $1"; else echo "  FAIL  $1 (want $3, got '$2')"; fail=1; fi; }

if [ "${begin:-0}" -ge 1 ] 2>/dev/null; then echo "  PASS  booted + probe ran (CC_BEGIN)"; else echo "  FAIL  no CC_BEGIN"; fail=1; fi
chk "no #GP/#PF/panic" "$crash" "0"
chk "TEETH cap genuinely contended (some ok + some ENOSPC)" "$teeth" "true"
chk "(1) clean handling: no non-ENOSPC errors" "$other" "0"
chk "(1) node alive after the storm" "$alive" "true"
chk "(2) invariant: total within the cap" "$within" "true"
chk "(2) no interleave corruption" "$corrupt" "0"
chk "(3) recovery: fresh write byte-exact after free" "$recov" "true"
echo "  ---- ok=$okc enospc=$enc total_bytes=$tot ----"

if [ "$fail" = 0 ]; then echo "CAP: PASS (contended, clean, uncorrupted, recovered)"; exit 0; fi
echo "CAP: FAIL"; exit 1
