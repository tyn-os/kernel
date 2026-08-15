#!/bin/bash
# TCG mechanics check for fd_flood.py BEFORE spending on Nitro (validate the exact
# script on free TCG first). Boots l2app on TCG, runs the ramping flood at small
# scale against localhost, and confirms: baseline_health=True (the health() fix),
# the ramp climbs, and the client terminates cleanly without hanging. SLIRP won't
# reproduce the panic (not enough pressure / conn caps) — this is a mechanics gate,
# not the measurement.
set -u
KERNEL=/home/ubuntu/kernel/target/x86_64-tyn/release/tyn-kernel
BASE=/home/ubuntu/kernel/src/otp-rootfs.cpio
REL=/home/ubuntu/l2app/_build/prod/rel/l2app
WORK=/home/ubuntu/work
HERE="$(cd "$(dirname "$0")" && pwd)"
LOG=$WORK/flood_tcg.log; CPIO=$WORK/flood_tcg.cpio; RAW=$WORK/flood_tcg.raw
rm -f "$LOG"
# Kill any leftover qemu (pgrep -x can't match the >15-char name; -f matches the
# full cmdline. Safe in a file script: this process's cmdline is "bash <path>",
# which does not contain "qemu-system").
pkill -9 -f qemu-system-x86_64 2>/dev/null; sleep 2

/home/ubuntu/kernel/tyn-pack "$REL" --base "$BASE" -o "$CPIO" >/dev/null 2>&1 || { echo "FAIL pack"; exit 1; }
KERNEL=$KERNEL bash "$WORK/mkdisk.sh" "$CPIO" "$RAW" >/dev/null 2>&1 || { echo "FAIL mkdisk"; exit 1; }

timeout 220 qemu-system-x86_64 -accel tcg -cpu max -m 2560M -machine q35 -smp 1 \
  -drive file="$RAW",format=raw,if=ide -no-reboot -nographic -serial "file:$LOG" \
  -device virtio-net-pci,netdev=n0,disable-legacy=on,disable-modern=off \
  -netdev user,id=n0,hostfwd=tcp::5680-:8080 </dev/null >/dev/null 2>&1 &
QP=$!
up=0
for i in $(seq 1 180); do
  curl -s --max-time 3 http://localhost:5680/health 2>/dev/null | grep -q L2_OK && { up=1; echo "listener up ${i}s"; break; }
  kill -0 $QP 2>/dev/null || { echo "qemu died"; break; }
  sleep 1
done
if [ "$up" = 1 ]; then
  echo "== fd_flood mechanics (max 800 batch 200) =="
  ulimit -n 65535 2>/dev/null
  timeout 120 python3 "$HERE/fd_flood.py" localhost 5680 800 200; echo "flood exit=$?"
else
  echo "FAIL listener never up"
fi
kill -9 $QP 2>/dev/null
echo "== host responsive: alive =="
