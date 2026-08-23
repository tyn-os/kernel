#!/bin/bash
# FREE TCG mechanics gate for the /big bulk endpoint (run on build host BEFORE any
# Nitro spend). Confirms /big serves the exact requested byte count over real Tyn
# HTTP — throughput on TCG is meaningless (emulated), but "does the route return
# N MB byte-exact" is exactly what TCG validates. Plaintext only (TLS is Nitro-
# authoritative). Self-safe qemu kill (this cmdline is "bash <path>").
set -u
KDIR=/home/ubuntu/kernel
KERNEL="${KERNEL:-$KDIR/target/x86_64-tyn/release/tyn-kernel}"
BASE="${BASE:-$KDIR/src/otp-rootfs.cpio}"
REL="${REL:-/home/ubuntu/soak_app/_build/prod/rel/soak_app}"
WORK="${WORK:-/home/ubuntu/work}"
CPIO=$WORK/big_tcg.cpio; RAW=$WORK/big_tcg.raw; LOG=$WORK/big_tcg.log
mkdir -p "$WORK"; rm -f "$LOG"
pkill -9 -f qemu-system-x86_64 2>/dev/null; sleep 2

"$KDIR/tyn-pack" "$REL" --base "$BASE" -o "$CPIO" >/dev/null 2>&1 || { echo "FAIL pack"; exit 1; }
( cd "$KDIR" && CPIO="$CPIO" IMAGE="$RAW" KERNEL="$KERNEL" ./build-disk.sh ) >/tmp/big_bd.log 2>&1 \
  || { echo "FAIL build-disk"; tail -6 /tmp/big_bd.log; exit 1; }

timeout 360 qemu-system-x86_64 -accel tcg -cpu max -m 2560M -machine q35 \
  -drive file="$RAW",format=raw,if=ide -no-reboot -nographic -serial "file:$LOG" \
  -device virtio-net-pci,netdev=n0,disable-legacy=on,disable-modern=off \
  -netdev user,id=n0,hostfwd=tcp::5680-:8080 </dev/null >/dev/null 2>&1 &
QP=$!

up=0
for i in $(seq 1 240); do
  curl -s --max-time 3 http://localhost:5680/health 2>/dev/null | grep -q L2_OK && { up=1; echo "node up ~${i}s"; break; }
  kill -0 $QP 2>/dev/null || { echo "qemu died — see $LOG"; tail -20 "$LOG"; break; }
  sleep 1
done
[ "$up" != 1 ] && { echo "FAIL: node never served /health"; kill -9 $QP 2>/dev/null; exit 1; }

RC=0
for MB in 1 5; do
  WANT=$((MB * 1024 * 1024))
  GOT=$(curl -s --max-time 60 -o /dev/null -w '%{size_download}' "http://localhost:5680/big?mb=$MB")
  if [ "$GOT" = "$WANT" ]; then echo "  /big?mb=$MB -> $GOT bytes (byte-exact OK)"; else echo "  /big?mb=$MB -> $GOT bytes WANT $WANT  MISMATCH"; RC=1; fi
done
kill -9 $QP 2>/dev/null
echo "== tcg_big_check exit=$RC (0=/big serves byte-exact; mechanics only, not throughput) =="
exit $RC
