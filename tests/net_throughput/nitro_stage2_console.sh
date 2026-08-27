#!/bin/bash
# Isolation Stage 2 — keep-instance-alive capture of the [stage2] serial console
# (the real per-syscall transition-cost number + explicit T1 count under -smp).
# The feature kernel prints [stage2] results at boot; Nitro's console output LAGS
# minutes, so last session the throughput harness tore down before it populated.
# Fix: launch, keep the instance alive long enough for the console to populate,
# read it, THEN teardown. Leak-proof trap + deadman. $0 on abort.
set -u
REGION=us-east-1
INSTANCE_TYPE="${INSTANCE_TYPE:-c5.xlarge}"          # real -smp (4 vCPU)
IMAGE="${IMAGE:-/dev/shm/s2c-disk.raw}"
REL="${REL:-/home/ubuntu/soak_app/_build/prod/rel/soak_app}"
KDIR=/home/ubuntu/kernel
KERNEL="${KERNEL:-/home/ubuntu/work/tyn-kernel-stage2shim}"   # FEATURE build
SG_ID="${SG_ID:-sg-0af575aad1ecce29c}"
BUCKET="tyn-images-$(aws sts get-caller-identity --query Account --output text)"
TS="$(date +%Y%m%d-%H%M%S)"; export AWS_PAGER=""
S3_KEY=""; SNAP=""; AMI=""; IID=""; DEADMAN=""
cleanup() {
  echo "=== CLEANUP ==="; [ -n "$DEADMAN" ] && kill "$DEADMAN" 2>/dev/null
  [ -n "$IID" ]  && aws ec2 terminate-instances --region $REGION --instance-ids "$IID" >/dev/null 2>&1 && echo "  terminated $IID"
  [ -n "$AMI" ]  && aws ec2 deregister-image --region $REGION --image-id "$AMI" >/dev/null 2>&1
  [ -n "$SNAP" ] && { sleep 4; aws ec2 delete-snapshot --region $REGION --snapshot-id "$SNAP" >/dev/null 2>&1; }
  [ -n "$S3_KEY" ] && aws s3 rm "s3://$BUCKET/$S3_KEY" --region $REGION >/dev/null 2>&1
}
trap cleanup EXIT INT TERM
( sleep 1500; echo "!! DEADMAN"; kill -TERM -$$ 2>/dev/null ) & DEADMAN=$!

if [ "${REBUILD:-1}" = 1 ]; then
  CPIO=/home/ubuntu/work/s2c.cpio
  "$KDIR/tyn-pack" "$REL" --base "$KDIR/src/otp-rootfs.cpio" -o "$CPIO" >/dev/null 2>&1 || { echo FAIL pack; exit 1; }
  ( cd "$KDIR" && CPIO="$CPIO" IMAGE="$IMAGE" KERNEL="$KERNEL" ./build-disk.sh ) >/tmp/s2c_bd.log 2>&1 || { echo FAIL build-disk; tail -5 /tmp/s2c_bd.log; exit 1; }
fi

echo "=== S3 + import + register ==="
S3_KEY="tyn-nt-s2c-${TS}.raw"
aws s3 cp "$IMAGE" "s3://$BUCKET/$S3_KEY" --region $REGION >/dev/null || { echo FAIL s3; exit 1; }
TASK=$(aws ec2 import-snapshot --region $REGION --disk-container "Format=RAW,UserBucket={S3Bucket=$BUCKET,S3Key=$S3_KEY}" --query ImportTaskId --output text) || exit 1
for i in $(seq 1 90); do st=$(aws ec2 describe-import-snapshot-tasks --region $REGION --import-task-ids "$TASK" --query 'ImportSnapshotTasks[0].SnapshotTaskDetail.Status' --output text 2>/dev/null); [ "$st" = completed ] && break; { [ "$st" = deleted ] || [ "$st" = error ]; } && { echo "import $st"; exit 1; }; sleep 15; done
SNAP=$(aws ec2 describe-import-snapshot-tasks --region $REGION --import-task-ids "$TASK" --query 'ImportSnapshotTasks[0].SnapshotTaskDetail.SnapshotId' --output text)
AMI=$(aws ec2 register-image --region $REGION --name "tyn-nt-s2c-${TS}" --architecture x86_64 --root-device-name /dev/xvda --block-device-mappings "DeviceName=/dev/xvda,Ebs={SnapshotId=$SNAP,VolumeSize=1,DeleteOnTermination=true,VolumeType=gp3}" --virtualization-type hvm --boot-mode legacy-bios --ena-support --query ImageId --output text) || exit 1
IID=$(aws ec2 run-instances --region $REGION --image-id "$AMI" --instance-type "$INSTANCE_TYPE" --security-group-ids "$SG_ID" --tag-specifications "ResourceType=instance,Tags=[{Key=Name,Value=tyn-nt-s2c-${TS}}]" --query 'Instances[0].InstanceId' --output text) || exit 1
aws ec2 wait instance-running --region $REGION --instance-ids "$IID"
echo "  instance $IID up — keeping alive for console to populate"

# Poll the console up to ~7 min; the [stage2] lines print at boot but lag on Nitro.
for i in $(seq 1 28); do
  sleep 15
  OUT=$(aws ec2 get-console-output --region $REGION --instance-id "$IID" --output text 2>/dev/null)
  if echo "$OUT" | grep -qa "stage2] ACCEPTANCE"; then
    echo "=== [stage2] console (captured at ~$((i*15))s) ==="
    echo "$OUT" | grep -aE "stage2" | head -20
    exit 0
  fi
done
echo "=== console never showed [stage2] ACCEPTANCE; last console tail ==="
aws ec2 get-console-output --region $REGION --instance-id "$IID" --output text 2>/dev/null | tail -20
