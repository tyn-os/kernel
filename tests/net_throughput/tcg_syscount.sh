#!/bin/bash
# Stage-3 P2: measure hot-serving-path syscalls/MB on TCG (free). Boots the
# `syscall_count` kernel (dumps [SYSCT] counters every ~1s), reads the counter
# delta across a large /big serve → syscalls per MB. Self-safe qemu kill.
set -u
KDIR=/home/ubuntu/kernel
KERNEL="${KERNEL:-$KDIR/target/x86_64-tyn/release/tyn-kernel}"
BASE="${BASE:-$KDIR/src/otp-rootfs.cpio}"
REL="${REL:-/home/ubuntu/soak_app/_build/prod/rel/soak_app}"
WORK="${WORK:-/home/ubuntu/work}"
CPIO=$WORK/sc.cpio; RAW=$WORK/sc.raw; LOG=$WORK/sc.log
mkdir -p "$WORK"; rm -f "$LOG"
pkill -9 -f qemu-system-x86_64 2>/dev/null; sleep 2

"$KDIR/tyn-pack" "$REL" --base "$BASE" -o "$CPIO" >/dev/null 2>&1 || { echo FAIL pack; exit 1; }
( cd "$KDIR" && CPIO="$CPIO" IMAGE="$RAW" KERNEL="$KERNEL" ./build-disk.sh ) >/tmp/sc_bd.log 2>&1 || { echo FAIL build-disk; tail -6 /tmp/sc_bd.log; exit 1; }

timeout 360 qemu-system-x86_64 -accel tcg -cpu max -m 2560M -machine q35 \
  -drive file="$RAW",format=raw,if=ide -no-reboot -nographic -serial "file:$LOG" \
  -device virtio-net-pci,netdev=n0,disable-legacy=on,disable-modern=off \
  -netdev user,id=n0,hostfwd=tcp::5680-:8080 </dev/null >/dev/null 2>&1 &
QP=$!

up=0
for i in $(seq 1 240); do
  curl -s --max-time 3 http://localhost:5680/health 2>/dev/null | grep -q L2_OK && { up=1; echo "node up ~${i}s"; break; }
  kill -0 $QP 2>/dev/null || { echo "qemu died"; tail -15 "$LOG"; break; }
  sleep 1
done
[ "$up" != 1 ] && { echo FAIL no-serve; kill -9 $QP 2>/dev/null; exit 1; }

echo "=== idle baseline (5s of SYSCT) ==="; sleep 5
BEFORE=$(grep -a "\[SYSCT\]" "$LOG" | tail -1); echo "before: $BEFORE"
echo "=== serve /big?mb=50 (timed) ==="
SPD=$(curl -s -o /dev/null -w '%{size_download}B @ %{speed_download}Bps in %{time_total}s' --max-time 300 "http://localhost:5680/big?mb=50")
echo "  $SPD"
sleep 2
AFTER=$(grep -a "\[SYSCT\]" "$LOG" | tail -1); echo "after:  $AFTER"
echo "=== all SYSCT lines (for idle-rate vs serve delta) ==="
grep -a "\[SYSCT\]" "$LOG" | tail -20
kill -9 $QP 2>/dev/null
echo "== compute: (after.write - before.write)/50 = writes/MB; likewise epoll; total/50 = syscalls/MB =="
