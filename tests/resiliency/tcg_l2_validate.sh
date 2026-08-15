#!/bin/bash
# TCG mechanics validation of the whole Layer-2 harness BEFORE spending on Nitro
# (the expensive-harness discipline: validate the exact scripts on free TCG
# first). Boots l2app on TCG with hostfwd, confirms the cap probe passes (CC_ on
# serial), then runs the net-attack driver at SMALL scale against localhost.
# NOTE: SLIRP user-networking fakes the attack surface — this proves the scripts
# RUN and the assertions COMPUTE, not a faithful attack measurement. Nitro is the
# standard of evidence for the networking numbers.
set -u
KERNEL="${KERNEL:-/home/ubuntu/kernel/target/x86_64-tyn/release/tyn-kernel}"
BASECPIO="${BASECPIO:-/home/ubuntu/kernel/src/otp-rootfs.cpio}"
REL="${REL:-/home/ubuntu/l2app/_build/prod/rel/l2app}"
WORK="${WORK:-/home/ubuntu/work}"
HERE="$(cd "$(dirname "$0")" && pwd)"
HPORT="${HPORT:-5680}"
TAG=l2_tcg; LOG="$WORK/${TAG}.log"; CPIO="$WORK/${TAG}.cpio"; RAW="$WORK/${TAG}.raw"
rm -f "$LOG"; pkill -9 -f qemu-system 2>/dev/null; sleep 2

/home/ubuntu/kernel/tyn-pack "$REL" --base "$BASECPIO" -o "$CPIO" >/dev/null 2>&1 || { echo "L2: FAIL pack"; exit 1; }
MKDISK="$HERE/../simd/mkdisk.sh"; [ -f "$MKDISK" ] || MKDISK="$WORK/mkdisk.sh"
KERNEL="$KERNEL" bash "$MKDISK" "$CPIO" "$RAW" >/dev/null 2>&1 || { echo "L2: FAIL mkdisk"; exit 1; }

timeout 320 qemu-system-x86_64 -accel tcg -cpu max -m 2560M -machine q35 -smp 1 \
  -drive file="$RAW",format=raw,if=ide -no-reboot -nographic -serial "file:$LOG" \
  -device virtio-net-pci,netdev=n0,disable-legacy=on,disable-modern=off \
  -netdev "user,id=n0,hostfwd=tcp::${HPORT}-:8080" </dev/null >/dev/null 2>&1 &
QP=$!
ready=0
for i in $(seq 1 220); do
  if curl -s --max-time 3 "http://localhost:${HPORT}/health" 2>/dev/null | grep -q L2_OK; then ready=1; echo "listener up at ${i}s"; break; fi
  grep -qaE "#GP|#PF|panic" "$LOG" 2>/dev/null && { echo "CRASH during boot"; break; }
  kill -0 $QP 2>/dev/null || { echo "qemu died at ${i}s"; break; }
  sleep 1
done

clean() { tr -d '\000' < "$LOG" 2>/dev/null; }
if [ "$ready" = 1 ]; then
  echo "=== net attacks (small N — TCG mechanics only) ==="
  SL_N=30 FD_N=200 SL_HOLD=24 FD_HOLD=12 bash "$HERE/drive_net_attacks.sh" localhost "$HPORT"
else
  echo "L2: FAIL (listener never came up)"
fi
# Cap probe prints CC_ ~5s into boot; read it at the END so it has certainly landed.
echo "=== cap probe (CC_ on serial) ==="; clean | grep -aE "^CC_|\[tmpfs\]"
cf() { clean | grep -aE "^$1 " | tail -1 | grep -oE "$2=[^ ]+" | head -1 | cut -d= -f2; }
echo "  cap verdict: within_cap=$(cf CC_TOTAL within_cap) corrupt=$(cf CC_CORRUPT count) contended=$(cf CC_TEETH contended) recovery=$(cf CC_RECOVERY ok) alive=$(cf CC_END node_alive)"
echo "=== post-run fault count on serial ==="; clean | grep -acE "#GP|#PF|panic" | sed 's/^/  faults=/'
kill -9 $QP 2>/dev/null; pkill -9 -f qemu-system 2>/dev/null
echo "=== done ==="
