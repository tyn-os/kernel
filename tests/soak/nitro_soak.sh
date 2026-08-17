#!/bin/bash
# Phase-2 Layer-4 SOAK (scheduled tier) — the standing BUG-1 SMP regression guard
# (4a) + general drift (4b), on real Nitro SMP.
#
# 4a: boot the md5 amplifier under real multi-core SMP for a long TYN_AMP_RUNTIME_MS
#     and assert AMP_TOTAL large_md5 == 0 (fixed) — the guard that the IPI-IST
#     red-zone SMP corruption (BUG-1) stays closed. Teeth: run with KERNEL pointed
#     at ~/work/tyn-kernel-poison (IPI-IST reverted) and it must report large_md5>0
#     (the BUG-1 closure record already established this dual acceptance).
# 4b: over the same run, the [diag] trace (heap_free + TCP-state histogram) must
#     stay BOUNDED — no slow heap/fd/socket leak, no drift. NOTE: [diag] is gated
#     behind the default-off VERBOSE flag; 4b needs a VERBOSE-on kernel or the
#     runtime VERBOSE toggle (docs/PAYDOWN.md observability item). 4a (large_md5)
#     works with any kernel.
#
# Leak-proof cleanup on any exit. Run from a clean harness. This is the scheduled
# tier — set TYN_AMP_RUNTIME_MS to hours for a real soak; a shorter value gives a
# representative window.
#
# Env: KERNEL (default the fixed current kernel), TYN_AMP_RUNTIME_MS (default 1h),
#      TYN_AMP_WORKERS (16), INSTANCE_TYPE (c5.xlarge = 4 vCPU SMP).
set -u
REGION=us-east-1
BUCKET="tyn-images-$(aws sts get-caller-identity --query Account --output text)"
SG_ID="${SG_ID:-sg-0af575aad1ecce29c}"
INSTANCE_TYPE="${INSTANCE_TYPE:-c5.xlarge}"          # >=2 vCPU: real SMP (BUG-1 is SMP-only)
KERNEL="${KERNEL:-/home/ubuntu/work/tyn-kernel-fixed-29abbab}"
TYN_AMP_RUNTIME_MS="${TYN_AMP_RUNTIME_MS:-3600000}"  # 1h default; set to hours for a soak
TYN_AMP_WORKERS="${TYN_AMP_WORKERS:-16}"
TS=$(date +%Y%m%d-%H%M%S)
KDIR=/home/ubuntu/kernel
REL=/home/ubuntu/ampapp/_build/prod/rel/ampapp
BASECPIO=$KDIR/src/otp-rootfs.cpio
CPIO=/home/ubuntu/work/soak_amp.cpio
IMAGE=/dev/shm/tyn-soak-disk.raw
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

echo "=== pack amp (runtime=${TYN_AMP_RUNTIME_MS}ms workers=$TYN_AMP_WORKERS) + build-disk (KERNEL=$(basename "$KERNEL")) ==="
"$KDIR/tyn-pack" "$REL" --base "$BASECPIO" \
  --env TYN_AMP_RUNTIME_MS="$TYN_AMP_RUNTIME_MS" --env TYN_AMP_WORKERS="$TYN_AMP_WORKERS" \
  -o "$CPIO" >/dev/null 2>&1 || { echo "FAIL pack"; exit 1; }
( cd "$KDIR" && CPIO="$CPIO" IMAGE="$IMAGE" KERNEL="$KERNEL" ./build-disk.sh ) >/tmp/soak_builddisk.log 2>&1 \
  || { echo "FAIL build-disk"; tail -8 /tmp/soak_builddisk.log; exit 1; }

echo "=== S3 + import-snapshot ==="
S3_KEY="tyn-soak-${TS}.raw"
aws s3 cp "$IMAGE" "s3://$BUCKET/$S3_KEY" --region $REGION >/dev/null || { echo "FAIL s3"; exit 1; }
TASK=$(aws ec2 import-snapshot --region $REGION --description "soak $TS" \
  --disk-container "Format=RAW,UserBucket={S3Bucket=$BUCKET,S3Key=$S3_KEY}" --query 'ImportTaskId' --output text) || { echo "FAIL import"; exit 1; }
for i in $(seq 1 80); do
  st=$(aws ec2 describe-import-snapshot-tasks --region $REGION --import-task-ids "$TASK" --query 'ImportSnapshotTasks[0].SnapshotTaskDetail.Status' --output text 2>/dev/null)
  [ "$st" = completed ] && { echo "import done ~$((i*15))s"; break; }
  { [ "$st" = deleted ] || [ "$st" = error ]; } && { echo "import $st"; exit 1; }
  sleep 15
done
SNAP=$(aws ec2 describe-import-snapshot-tasks --region $REGION --import-task-ids "$TASK" --query 'ImportSnapshotTasks[0].SnapshotTaskDetail.SnapshotId' --output text)
[ -n "$SNAP" ] && [ "$SNAP" != None ] || { echo "FAIL no snap"; exit 1; }
AMI=$(aws ec2 register-image --region $REGION --name "tyn-soak-${TS}" \
  --description "Tyn Layer-4 soak (throwaway)" --architecture x86_64 --root-device-name /dev/xvda \
  --block-device-mappings "DeviceName=/dev/xvda,Ebs={SnapshotId=$SNAP,VolumeSize=1,DeleteOnTermination=true,VolumeType=gp3}" \
  --virtualization-type hvm --boot-mode legacy-bios --ena-support --query 'ImageId' --output text) || { echo "FAIL register"; exit 1; }
IID=$(aws ec2 run-instances --region $REGION --image-id "$AMI" --instance-type "$INSTANCE_TYPE" \
  --security-group-ids "$SG_ID" --tag-specifications "ResourceType=instance,Tags=[{Key=Name,Value=tyn-soak-${TS}}]" \
  --query 'Instances[0].InstanceId' --output text) || { echo "FAIL launch"; exit 1; }
aws ec2 wait instance-running --region $REGION --instance-ids "$IID"
echo "instance $IID ($INSTANCE_TYPE) — amp runs ${TYN_AMP_RUNTIME_MS}ms"

echo "=== poll serial console for AMP_TOTAL (up to runtime + 10 min) ==="
DEADLINE=$(( TYN_AMP_RUNTIME_MS/1000 + 600 ))
CON=""; got=0
for i in $(seq 1 $(( DEADLINE/30 )) ); do
  sleep 30
  CON=$(aws ec2 get-console-output --region $REGION --instance-id "$IID" --latest --output text 2>/dev/null | tr -d '\000')
  echo "$CON" | grep -qaE "AMP_TOTAL" && { got=1; echo "AMP_TOTAL seen at ~$((i*30))s"; break; }
  echo "$CON" | grep -qaE "KERNEL PANIC|#GP|#PF" && { echo "CRASH during soak"; break; }
done

echo "=== RESULT ==="
echo "-- amplifier verdict --"; echo "$CON" | grep -aE "AMP_BEGIN|AMP_TOTAL|AMP_BAD|AMP_INPUT_CORRUPT" | tail -8
echo "-- 4b drift ([diag], if VERBOSE on) --"
echo "$CON" | grep -aE "^\[diag\]" | sed -n '1p;$p'   # first + last: eyeball heap_free/socket drift
lm=$(echo "$CON" | grep -aE "AMP_TOTAL" | tail -1 | grep -oE "large_md5=[0-9]+" | cut -d= -f2)
sm=$(echo "$CON" | grep -aE "AMP_TOTAL" | tail -1 | grep -oE "small_md5=[0-9]+" | cut -d= -f2)
crash=$(echo "$CON" | grep -acE "KERNEL PANIC|#GP|#PF")
echo "=== BUG-1 SMP soak verdict (KERNEL=$(basename "$KERNEL")) ==="
if [ "$got" != 1 ]; then echo "SOAK: INCONCLUSIVE (no AMP_TOTAL — check runtime vs poll window)"; exit 1; fi
echo "  small_md5=$sm large_md5=$lm crash=$crash"
# 4a assertion: fixed kernel must be 0; poison must be >0 (teeth).
case "$KERNEL" in
  *poison*) { [ "${lm:-0}" -gt 0 ] && echo "  TEETH PASS: poison reintroduced BUG-1 (large_md5=$lm > 0)"; } || echo "  TEETH FAIL: poison did not corrupt (large_md5=$lm) — guard would be inert" ;;
  *)        { [ "${lm:-x}" = 0 ] && [ "$crash" = 0 ] && echo "  SOAK: PASS (large_md5=0 over ${TYN_AMP_RUNTIME_MS}ms real SMP, no crash)"; } || echo "  SOAK: FAIL (large_md5=$lm crash=$crash)" ;;
esac
