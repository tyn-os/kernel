#!/bin/bash
# STAGED — dist-stability ladder on real Nitro (2-node). WATCHED window: this is
# interactive diagnosis, not a fixed soak. Forms the pair, then runs the ladder
# whose readings collapse the CONFIG-DRIFT vs KERNEL-REGRESSION fork and
# characterize the connected-phase data path (DIST_STABILITY_HUNT):
#
#   L0 form   — both /health, /connect, confirm peers=1 (else abort cheap).
#   L1 tiny   — /rpc?bytes=100 on node A: does ANY term frame traverse the
#               connected-phase data path? (This IS the step-3-first discriminator:
#               dist_ladder has NO sustained workload, so a tiny rpc traversing
#               means the data path WORKS and the soak's failure was the
#               DistWorker workload = CONFIG DRIFT; a tiny rpc FAILING means the
#               data path is broken minimally = REGRESSION or tyn_epmd config.)
#   L2 1MB    — /rpc?bytes=1048576: large-term byte-exact? (any-data vs large-data).
#   L3 idle   — poll /diststat both nodes ~90s at net_ticktime=8s: does the peer
#               DROP (~8s, tick can't traverse → data path dead) or STAY (idle
#               fine; only the workload breaks it)?
#
# Leak-proof trap + deadman; formation gate aborts cheap (~5min) if it can't form.
set -u
REGION=us-east-1; export AWS_PAGER=""
INSTANCE_TYPE="${INSTANCE_TYPE:-c5.xlarge}"
IMAGE="${IMAGE:-/dev/shm/dl2-disk.raw}"
KDIR=/home/ubuntu/kernel
REL="${REL:-/home/ubuntu/dist_ladder/_build/prod/rel/dist_ladder}"
KERNEL="${KERNEL:-$KDIR/target/x86_64-tyn/release/tyn-kernel}"
COOKIE="${COOKIE:-tyn_spike_cookie}"
SG_ID="${SG_ID:?set SG_ID (8080 + dist 9100 between nodes)}"
BUCKET="tyn-images-$(aws sts get-caller-identity --query Account --output text)"
TS="$(date +%Y%m%d-%H%M%S)"
S3_KEY=""; SNAP=""; AMI=""; IID1=""; IID2=""; DEADMAN=""
cleanup() {
  echo "=== CLEANUP ==="; [ -n "$DEADMAN" ] && kill "$DEADMAN" 2>/dev/null
  for I in "$IID1" "$IID2"; do [ -n "$I" ] && aws ec2 terminate-instances --region $REGION --instance-ids "$I" >/dev/null 2>&1 && echo "  term $I"; done
  [ -n "$AMI" ]  && aws ec2 deregister-image --region $REGION --image-id "$AMI" >/dev/null 2>&1
  [ -n "$SNAP" ] && { sleep 4; aws ec2 delete-snapshot --region $REGION --snapshot-id "$SNAP" >/dev/null 2>&1; }
  [ -n "$S3_KEY" ] && aws s3 rm "s3://$BUCKET/$S3_KEY" --region $REGION >/dev/null 2>&1
}
trap cleanup EXIT INT TERM
( sleep 1800; echo "!! DEADMAN"; kill -TERM -$$ 2>/dev/null ) & DEADMAN=$!

if [ "${REBUILD:-1}" = 1 ]; then
  echo "=== build disk (dist kernel + dist_ladder + --cookie) ==="
  CPIO=/home/ubuntu/work/dl2.cpio
  "$KDIR/tyn-pack" "$REL" --base "$KDIR/src/otp-rootfs.cpio" --cookie "$COOKIE" -o "$CPIO" >/dev/null 2>&1 || { echo FAIL pack; exit 1; }
  ( cd "$KDIR" && CPIO="$CPIO" IMAGE="$IMAGE" KERNEL="$KERNEL" ./build-disk.sh ) >/tmp/dl2_bd.log 2>&1 || { echo FAIL build-disk; tail -5 /tmp/dl2_bd.log; exit 1; }
fi

echo "=== S3 + import + register ==="
S3_KEY="tyn-dl2-${TS}.raw"
aws s3 cp "$IMAGE" "s3://$BUCKET/$S3_KEY" --region $REGION >/dev/null || { echo FAIL s3; exit 1; }
TASK=$(aws ec2 import-snapshot --region $REGION --disk-container "Format=RAW,UserBucket={S3Bucket=$BUCKET,S3Key=$S3_KEY}" --query ImportTaskId --output text) || exit 1
for i in $(seq 1 90); do st=$(aws ec2 describe-import-snapshot-tasks --region $REGION --import-task-ids "$TASK" --query 'ImportSnapshotTasks[0].SnapshotTaskDetail.Status' --output text 2>/dev/null); [ "$st" = completed ] && break; { [ "$st" = deleted ] || [ "$st" = error ]; } && { echo "import $st"; exit 1; }; sleep 15; done
SNAP=$(aws ec2 describe-import-snapshot-tasks --region $REGION --import-task-ids "$TASK" --query 'ImportSnapshotTasks[0].SnapshotTaskDetail.SnapshotId' --output text)
AMI=$(aws ec2 register-image --region $REGION --name "tyn-dl2-${TS}" --architecture x86_64 --root-device-name /dev/xvda --block-device-mappings "DeviceName=/dev/xvda,Ebs={SnapshotId=$SNAP,VolumeSize=1,DeleteOnTermination=true,VolumeType=gp3}" --virtualization-type hvm --boot-mode legacy-bios --ena-support --query ImageId --output text) || exit 1

echo "=== launch 2 ==="
launch() { aws ec2 run-instances --region $REGION --image-id "$AMI" --instance-type "$INSTANCE_TYPE" --security-group-ids "$SG_ID" --tag-specifications "ResourceType=instance,Tags=[{Key=Name,Value=tyn-dl2-${TS}-$1}]" --query 'Instances[0].InstanceId' --output text; }
IID1=$(launch a) || exit 1; IID2=$(launch b) || exit 1
aws ec2 wait instance-running --region $REGION --instance-ids "$IID1" "$IID2"
addr() { aws ec2 describe-instances --region $REGION --instance-ids "$1" --query "Reservations[0].Instances[0].$2" --output text; }
IP1=$(addr "$IID1" PublicIpAddress); IP2=$(addr "$IID2" PublicIpAddress)
PRIV1=$(addr "$IID1" PrivateIpAddress); PRIV2=$(addr "$IID2" PrivateIpAddress)
echo "  a: $IP1 (n@$PRIV1) | b: $IP2 (n@$PRIV2)"

echo "=== L0: form + confirm ==="
for IP in "$IP1" "$IP2"; do ok=0; for i in $(seq 1 75); do curl -s --max-time 4 "http://$IP:8080/health" | grep -q L2_OK && { ok=1; break; }; sleep 4; done; [ "$ok" != 1 ] && { echo "ABORT: $IP never served"; exit 1; }; done
curl -s -X POST "http://$IP1:8080/connect?node=n@$PRIV2&cookie=$COOKIE"; echo
curl -s -X POST "http://$IP2:8080/connect?node=n@$PRIV1&cookie=$COOKIE"; echo
sleep 3
P=$(curl -s "http://$IP1:8080/diststat" | grep -oE '"peer_count":[0-9]+' | grep -oE '[0-9]+$')
[ "${P:-0}" -ge 1 ] 2>/dev/null || { echo "ABORT: cluster did not form (peer_count=$P)"; exit 1; }
echo "  FORMED (peer_count=$P)"

echo "=== L1: tiny rpc (100B) — does ANY term frame traverse? [step-3-first discriminator] ==="
curl -s --max-time 8 "http://$IP1:8080/rpc?bytes=100&timeout=6000"; echo
echo "=== L2: 1MB rpc — large-term byte-exact? ==="
curl -s --max-time 12 "http://$IP1:8080/rpc?bytes=1048576&timeout=10000"; echo
echo "=== L2b: SIZE SWEEP (the scaling test — throughput vs size) ==="
echo "    rising-with-size => window/buffer-bound (#4); flat => poll-cadence/per-RT (#3/#2)"
for B in 10000 100000 500000 1000000 2000000 4000000; do
  R=$(curl -s --max-time 120 "http://$IP1:8080/rpc?bytes=$B&timeout=115000")
  RTT=$(echo "$R" | grep -oE '"rtt_ms":[0-9]+' | grep -oE '[0-9]+$')
  BX=$(echo "$R" | grep -oE '"byte_exact":true')
  if [ -n "$RTT" ] && [ "$RTT" -gt 0 ]; then
    KBPS=$(( 2 * B / RTT ))   # 2*B bytes (send+echo) / RTT ms  ~=  KB/s
    echo "  bytes=$B rtt=${RTT}ms  ~${KBPS} KB/s  byte_exact=${BX:-NO}"
  else
    echo "  bytes=$B -> $R"
  fi
done
echo "=== L3: idle stability at ticktime=8s (poll peer_count ~100s) ==="
for i in $(seq 1 20); do
  pc1=$(curl -s --max-time 4 "http://$IP1:8080/diststat" | grep -oE '"peer_count":[0-9]+' | grep -oE '[0-9]+$')
  pc2=$(curl -s --max-time 4 "http://$IP2:8080/diststat" | grep -oE '"peer_count":[0-9]+' | grep -oE '[0-9]+$')
  echo "  t=$((i*5))s peer_count a=$pc1 b=$pc2"
  sleep 5
done
echo "=== LADDER DONE — interpret: L1 pass+L3 stay=workload(config); L1 fail=data-path(regression/tyn_epmd); L1 pass+L2 fail=large-term only ==="
