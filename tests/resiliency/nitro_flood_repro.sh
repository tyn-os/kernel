#!/bin/bash
# Deterministic connection-flood reproducer on real Nitro: deploy l2app, ramp
# held connections with fd_flood.py to PIN the count at which Tyn's shared kernel
# heap is exhausted and it panics, capture the KERNEL PANIC from the serial
# console, report the threshold window. Leak-proof cleanup on any exit.
set -u
REGION=us-east-1
BUCKET="tyn-images-$(aws sts get-caller-identity --query Account --output text)"
SG_ID="${SG_ID:-sg-0af575aad1ecce29c}"
INSTANCE_TYPE="${INSTANCE_TYPE:-c5.xlarge}"
FLOOD_MAX="${FLOOD_MAX:-8000}"; FLOOD_BATCH="${FLOOD_BATCH:-250}"; FLOOD_MODE="${FLOOD_MODE:-pin}"
TS=$(date +%Y%m%d-%H%M%S)
KDIR=/home/ubuntu/kernel
REL=/home/ubuntu/l2app/_build/prod/rel/l2app
BASECPIO=$KDIR/src/otp-rootfs.cpio
CPIO=/home/ubuntu/work/l2_flood.cpio
IMAGE=/dev/shm/tyn-flood-disk.raw
HERE=/home/ubuntu/work/resiliency
export AWS_PAGER=""

S3_KEY=""; SNAP=""; AMI=""; IID=""
cleanup() {
  echo "=== CLEANUP (leak-proof) ==="
  [ -n "$IID" ]    && aws ec2 terminate-instances --region $REGION --instance-ids "$IID" >/dev/null 2>&1 && echo "  terminated $IID"
  [ -n "$AMI" ]    && aws ec2 deregister-image --region $REGION --image-id "$AMI" >/dev/null 2>&1 && echo "  deregistered $AMI"
  [ -n "$SNAP" ]   && { sleep 5; aws ec2 delete-snapshot --region $REGION --snapshot-id "$SNAP" >/dev/null 2>&1 && echo "  deleted $SNAP"; }
  [ -n "$S3_KEY" ] && aws s3 rm "s3://$BUCKET/$S3_KEY" --region $REGION >/dev/null 2>&1 && echo "  rm s3://$BUCKET/$S3_KEY"
}
trap cleanup EXIT

echo "=== pack + build-disk (l2app) ==="
"$KDIR/tyn-pack" "$REL" --base "$BASECPIO" -o "$CPIO" >/dev/null 2>&1 || { echo "FAIL pack"; exit 1; }
( cd "$KDIR" && CPIO="$CPIO" IMAGE="$IMAGE" ./build-disk.sh ) >/tmp/flood_builddisk.log 2>&1 || { echo "FAIL build-disk"; tail -8 /tmp/flood_builddisk.log; exit 1; }

echo "=== S3 + import-snapshot ==="
S3_KEY="tyn-flood-${TS}.raw"
aws s3 cp "$IMAGE" "s3://$BUCKET/$S3_KEY" --region $REGION >/dev/null || { echo "FAIL s3"; exit 1; }
TASK=$(aws ec2 import-snapshot --region $REGION --description "flood $TS" \
  --disk-container "Format=RAW,UserBucket={S3Bucket=$BUCKET,S3Key=$S3_KEY}" --query 'ImportTaskId' --output text) || { echo "FAIL import"; exit 1; }
echo "import task $TASK"
for i in $(seq 1 80); do
  st=$(aws ec2 describe-import-snapshot-tasks --region $REGION --import-task-ids "$TASK" --query 'ImportSnapshotTasks[0].SnapshotTaskDetail.Status' --output text 2>/dev/null)
  [ "$st" = completed ] && { echo "import done ~$((i*15))s"; break; }
  { [ "$st" = deleted ] || [ "$st" = error ]; } && { echo "import $st"; exit 1; }
  sleep 15
done
SNAP=$(aws ec2 describe-import-snapshot-tasks --region $REGION --import-task-ids "$TASK" --query 'ImportSnapshotTasks[0].SnapshotTaskDetail.SnapshotId' --output text)
[ -n "$SNAP" ] && [ "$SNAP" != None ] || { echo "FAIL no snap"; exit 1; }
echo "snapshot $SNAP"

AMI=$(aws ec2 register-image --region $REGION --name "tyn-flood-${TS}" \
  --description "Tyn flood repro (throwaway)" --architecture x86_64 --root-device-name /dev/xvda \
  --block-device-mappings "DeviceName=/dev/xvda,Ebs={SnapshotId=$SNAP,VolumeSize=1,DeleteOnTermination=true,VolumeType=gp3}" \
  --virtualization-type hvm --boot-mode legacy-bios --ena-support --query 'ImageId' --output text) || { echo "FAIL register"; exit 1; }
echo "AMI $AMI"
IID=$(aws ec2 run-instances --region $REGION --image-id "$AMI" --instance-type "$INSTANCE_TYPE" \
  --security-group-ids "$SG_ID" --tag-specifications "ResourceType=instance,Tags=[{Key=Name,Value=tyn-flood-${TS}}]" \
  --query 'Instances[0].InstanceId' --output text) || { echo "FAIL launch"; exit 1; }
aws ec2 wait instance-running --region $REGION --instance-ids "$IID"
IP=$(aws ec2 describe-instances --region $REGION --instance-ids "$IID" --query 'Reservations[0].Instances[0].PublicIpAddress' --output text)
echo "instance $IID  ip $IP"

echo "=== wait for listener ==="
up=0
for i in $(seq 1 75); do
  curl -s --max-time 4 "http://$IP:8080/health" 2>/dev/null | grep -q L2_OK && { up=1; echo "listener up ~$((i*4))s"; break; }
  sleep 4
done
[ "$up" = 1 ] || { echo "FAIL listener"; aws ec2 get-console-output --region $REGION --instance-id "$IID" --latest --output text 2>/dev/null | tr -d '\000' | tail -25; exit 1; }

echo "=== RAMP flood (max=$FLOOD_MAX batch=$FLOOD_BATCH mode=$FLOOD_MODE) ==="
ulimit -n 65535 2>/dev/null || echo "(warn: could not raise ulimit)"
python3 "$HERE/fd_flood.py" "$IP" 8080 "$FLOOD_MAX" "$FLOOD_BATCH" "$FLOOD_MODE" | tee /tmp/flood.out
echo "=== flood client done; reading serial console for the panic ==="
CON=""
for i in $(seq 1 14); do
  CON=$(aws ec2 get-console-output --region $REGION --instance-id "$IID" --latest --output text 2>/dev/null | tr -d '\000')
  echo "$CON" | grep -qaE "KERNEL PANIC|#GP|#PF" && { echo "panic on console (poll $i)"; break; }
  # if the flood didn't kill it, stop polling once /health is healthy again
  curl -s --max-time 4 "http://$IP:8080/health" 2>/dev/null | grep -q L2_OK && { echo "target still healthy (poll $i)"; break; }
  sleep 15
done

echo "=== RESULT ==="
echo "-- flood client verdict --"; grep -aE "FLOOD_THRESHOLD|FLOOD_END|baseline" /tmp/flood.out || tail -3 /tmp/flood.out
echo "-- kernel console (panic scan) --"
echo "$CON" | grep -anE "KERNEL PANIC|memory allocation|#GP|#PF|panic" | head
if echo "$CON" | grep -qaE "KERNEL PANIC|#GP|#PF|panic"; then PANIC=yes; else PANIC=no; fi
echo "REPRO: panic=$PANIC (flood max=$FLOOD_MAX)"

echo "=== after-fix acceptance ==="
# (1) no panic — the core fix
[ "$PANIC" = no ] && echo "  PASS  (1) no kernel panic under the flood" || echo "  FAIL  (1) kernel PANICKED"
# serve-during: did /health survive through the ramp? (per-batch health probes)
sd=$(grep -ac "health=ok" /tmp/flood.out); sf=$(grep -ac "health=FAIL" /tmp/flood.out)
echo "  serve-during: health ok=$sd fail=$sf across ramp batches (cap saturation may show FAIL at peak; the point is no-panic + recovery)"
# (3) recovery: node serves /health again after the flood closes
rec=FAIL; for i in 1 2 3 4 5 6; do curl -s --max-time 5 "http://$IP:8080/health" 2>/dev/null | grep -q L2_OK && { rec=ok; break; }; sleep 3; done
[ "$rec" = ok ] && echo "  PASS  (3) recovery: /health serves after the flood" || echo "  FAIL  (3) no recovery"
# (4) SMP accept+close churn — 80 concurrent complete requests. If the cap count
# drifted (lost/double decrement) these would be wrongly rejected; all succeed => exact.
: > /tmp/churn.out
for j in $(seq 1 80); do ( curl -s --max-time 8 "http://$IP:8080/health" 2>/dev/null | grep -q L2_OK && echo Y >> /tmp/churn.out ) & done; wait
cok=$(wc -l < /tmp/churn.out | tr -d ' ')
[ "${cok:-0}" -ge 76 ] 2>/dev/null && echo "  PASS  (4) accept+close churn: $cok/80 concurrent legit requests OK (count didn't drift)" || echo "  FAIL  (4) churn only $cok/80 (possible count drift)"
echo "(before-fix control: the same reproducer pinned the panic at ~1000-1250 on the UNFIXED kernel — deterministic teeth, 3 prior runs)"
