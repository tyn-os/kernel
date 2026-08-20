#!/bin/bash
# GP_HUNT capture — ONE instance, SAVE THE FULL SERIAL CONSOLE so we get the #GP
# RIP + registers + TIMING (the dual-tree count script discarded them). The RIP is
# the whole game: JIT-code range => the corruption class; kernel/boot range =>
# something deterministic. Timing (before vs after "GP_REPRO: launching") =>
# boot-inherent vs workload-triggered.
#
# KERNEL selects the tree (default fixed-29abbab). Leak-proof + deadman.
set -u
REGION=us-east-1
INSTANCE_TYPE="${INSTANCE_TYPE:-c5.xlarge}"
KDIR=/home/ubuntu/kernel
REL="${REL:-/home/ubuntu/gp_app/_build/prod/rel/gp_app}"
KERNEL="${KERNEL:-/home/ubuntu/work/tyn-kernel-fixed-29abbab}"
LABEL="${LABEL:-fixed}"
GP_PROCS="${GP_PROCS:-3}"; GP_SIZE="${GP_SIZE:-1048576}"; GP_ITERS="${GP_ITERS:-1500}"
OUT="${OUT:-/home/ubuntu/work/gp_capture_${LABEL}.console.txt}"
SG_ID="${SG_ID:?set SG_ID}"
BUCKET="tyn-images-$(aws sts get-caller-identity --query Account --output text)"
TS="$(date +%Y%m%d-%H%M%S)"
export AWS_PAGER=""
S3_KEY=""; SNAP=""; AMI=""; IID=""; DEADMAN=""
cleanup() {
  echo "=== CLEANUP ==="
  [ -n "$DEADMAN" ] && kill "$DEADMAN" 2>/dev/null
  [ -n "$IID" ]  && aws ec2 terminate-instances --region $REGION --instance-ids "$IID" >/dev/null 2>&1 && echo "  terminated $IID"
  [ -n "$AMI" ]  && aws ec2 deregister-image --region $REGION --image-id "$AMI" >/dev/null 2>&1 && echo "  deregistered $AMI"
  [ -n "$SNAP" ] && { sleep 5; aws ec2 delete-snapshot --region $REGION --snapshot-id "$SNAP" >/dev/null 2>&1 && echo "  deleted $SNAP"; }
  [ -n "$S3_KEY" ] && aws s3 rm "s3://$BUCKET/$S3_KEY" --region $REGION >/dev/null 2>&1 && echo "  rm s3://$BUCKET/$S3_KEY"
}
trap cleanup EXIT INT TERM
( sleep 1500; echo "!! DEADMAN"; kill -TERM -$$ 2>/dev/null ) & DEADMAN=$!

CPIO=/home/ubuntu/work/gpc_${LABEL}.cpio; IMAGE=/dev/shm/gpc_${LABEL}.raw
echo "=== [$LABEL] pack + build-disk (kernel=$(basename "$KERNEL")) ==="
"$KDIR/tyn-pack" "$REL" --base "$KDIR/src/otp-rootfs.cpio" \
  --env GP_PROCS="$GP_PROCS" --env GP_SIZE="$GP_SIZE" --env GP_ITERS="$GP_ITERS" \
  -o "$CPIO" >/dev/null 2>&1 || { echo "FAIL pack"; exit 1; }
( cd "$KDIR" && CPIO="$CPIO" IMAGE="$IMAGE" KERNEL="$KERNEL" ./build-disk.sh ) >/tmp/gpc_bd.log 2>&1 \
  || { echo "FAIL build-disk"; tail -6 /tmp/gpc_bd.log; exit 1; }

S3_KEY="tyn-gpc-${LABEL}-${TS}.raw"
aws s3 cp "$IMAGE" "s3://$BUCKET/$S3_KEY" --region $REGION >/dev/null || { echo "FAIL s3"; exit 1; }
TASK=$(aws ec2 import-snapshot --region $REGION --description "gpc-$LABEL $TS" \
  --disk-container "Format=RAW,UserBucket={S3Bucket=$BUCKET,S3Key=$S3_KEY}" --query ImportTaskId --output text) || exit 1
for i in $(seq 1 90); do
  st=$(aws ec2 describe-import-snapshot-tasks --region $REGION --import-task-ids "$TASK" --query 'ImportSnapshotTasks[0].SnapshotTaskDetail.Status' --output text 2>/dev/null)
  [ "$st" = completed ] && break
  { [ "$st" = deleted ] || [ "$st" = error ]; } && { echo "FAIL import $st"; exit 1; }
  sleep 15
done
SNAP=$(aws ec2 describe-import-snapshot-tasks --region $REGION --import-task-ids "$TASK" --query 'ImportSnapshotTasks[0].SnapshotTaskDetail.SnapshotId' --output text)
AMI=$(aws ec2 register-image --region $REGION --name "tyn-gpc-${LABEL}-${TS}" --architecture x86_64 --root-device-name /dev/xvda \
  --block-device-mappings "DeviceName=/dev/xvda,Ebs={SnapshotId=$SNAP,VolumeSize=1,DeleteOnTermination=true,VolumeType=gp3}" \
  --virtualization-type hvm --boot-mode legacy-bios --ena-support --query ImageId --output text) || exit 1
IID=$(aws ec2 run-instances --region $REGION --image-id "$AMI" --instance-type "$INSTANCE_TYPE" \
  --security-group-ids "$SG_ID" --tag-specifications "ResourceType=instance,Tags=[{Key=Name,Value=tyn-gpc-${LABEL}}]" \
  --query 'Instances[0].InstanceId' --output text) || exit 1
aws ec2 wait instance-running --region $REGION --instance-ids "$IID"
IP=$(aws ec2 describe-instances --region $REGION --instance-ids "$IID" --query 'Reservations[0].Instances[0].PublicIpAddress' --output text)
echo "  [$LABEL] $IID ip=$IP"

up=0; for i in $(seq 1 75); do curl -s --max-time 4 "http://$IP:8080/health" | grep -q L2_OK && { up=1; break; }; sleep 4; done
echo "  [$LABEL] health up=$up"
# let the repro finish + serial settle
for i in $(seq 1 40); do
  aws ec2 get-console-output --region $REGION --instance-id "$IID" --latest --output text 2>/dev/null | tr -d '\000' > "$OUT"
  grep -qaE "GP_REPRO_RESULT" "$OUT" && break
  sleep 10
done
echo "=== [$LABEL] full console saved to $OUT ($(wc -l < "$OUT") lines) ==="
echo "--- #GP lines (RIP + regs) ---"; grep -aE "#GP ip=" "$OUT"
echo "--- schedulers / repro markers ---"; grep -aE "schedulers_online|GP_REPRO: (start|launching)|GP_REPRO_RESULT" "$OUT"
echo "--- timing: line numbers of #GP vs 'GP_REPRO: launching' ---"
awk '/GP_REPRO: launching/{print "launching @ line "NR} /#GP ip=/{print "#GP @ line "NR}' "$OUT"
