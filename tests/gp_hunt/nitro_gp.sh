#!/bin/bash
# STAGED — GP_HUNT verification on real Nitro SMP, with dual-acceptance TEETH.
# Fold into tonight's watched window. NOT run unattended.
#
# The "tmpfs large-write #GP" is the SMP red-zone corruption class (BUG-1),
# surfaced via concurrent file I/O — NOT a tmpfs bug (GP_PROBE_A0). BUG-1 is fixed
# in-tree (7270266). This asks, reproduce-first: did that fix close the file-I/O
# surface too? Teeth = run the SAME gp_app repro on BOTH kernels:
#   * POISON (~/work/tyn-kernel-poison, IPI-IST reverted): the class is live here.
#     Expect the repro to surface it — silent corruption (readback mismatch) or a
#     hard "#GP ip=..." on serial. (Historically a RARE/file-I/O-surface trigger,
#     so if poison stays clean, that's a weak-trigger result, NOT proof of a fix —
#     confirm class-live separately with the ampapp md5 amplifier, which faults
#     deterministically on poison: tests/soak/nitro_soak.sh KERNEL=...poison.)
#   * FIXED (default current kernel): expect CLEAN — no mismatch, no #GP, over
#     enough iterations that silence is signal.
# Verdict: fixed-clean AND (poison-faults OR ampapp-confirms-class-live) => closed.
#
# c5.xlarge = 4 vCPU real SMP (the race is SMP-only; TCG will not reproduce it).
# Leak-proof: cleanup trap + deadman. Runs the two trees sequentially, one
# instance at a time.
set -u
REGION=us-east-1
INSTANCE_TYPE="${INSTANCE_TYPE:-c5.xlarge}"     # >=2 vCPU real SMP; 4 is ideal
KDIR=/home/ubuntu/kernel
REL="${REL:-/home/ubuntu/gp_app/_build/prod/rel/gp_app}"
BASECPIO="$KDIR/src/otp-rootfs.cpio"
POISON="${POISON:-/home/ubuntu/work/tyn-kernel-poison}"
FIXED="${FIXED:-$KDIR/target/x86_64-tyn/release/tyn-kernel}"
GP_PROCS="${GP_PROCS:-3}"; GP_SIZE="${GP_SIZE:-1048576}"; GP_ITERS="${GP_ITERS:-1500}"
BUCKET="tyn-images-$(aws sts get-caller-identity --query Account --output text)"
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
( sleep 3600; echo "!! DEADMAN"; kill -TERM -$$ 2>/dev/null ) & DEADMAN=$!

# Run gp_app on one kernel; echo the captured GP_REPRO_RESULT + #GP count.
run_tree() {
  local label="$1" kernel="$2"
  local ts; ts="$(date +%Y%m%d-%H%M%S)"
  local cpio=/home/ubuntu/work/gp_${label}.cpio
  local image=/dev/shm/gp_${label}.raw
  echo "=== [$label] pack + build-disk (kernel=$(basename "$kernel")) ==="
  "$KDIR/tyn-pack" "$REL" --base "$BASECPIO" \
    --env GP_PROCS="$GP_PROCS" --env GP_SIZE="$GP_SIZE" --env GP_ITERS="$GP_ITERS" \
    -o "$cpio" >/dev/null 2>&1 || { echo "[$label] FAIL pack"; return 2; }
  ( cd "$KDIR" && CPIO="$cpio" IMAGE="$image" KERNEL="$kernel" ./build-disk.sh ) \
    >/tmp/gp_${label}_bd.log 2>&1 || { echo "[$label] FAIL build-disk"; tail -5 /tmp/gp_${label}_bd.log; return 2; }

  S3_KEY="tyn-gp-${label}-${ts}.raw"
  aws s3 cp "$image" "s3://$BUCKET/$S3_KEY" --region $REGION >/dev/null || { echo "[$label] FAIL s3"; return 2; }
  local task; task=$(aws ec2 import-snapshot --region $REGION --description "gp-$label $ts" \
    --disk-container "Format=RAW,UserBucket={S3Bucket=$BUCKET,S3Key=$S3_KEY}" --query ImportTaskId --output text) || return 2
  for i in $(seq 1 80); do
    local st; st=$(aws ec2 describe-import-snapshot-tasks --region $REGION --import-task-ids "$task" --query 'ImportSnapshotTasks[0].SnapshotTaskDetail.Status' --output text 2>/dev/null)
    [ "$st" = completed ] && break
    { [ "$st" = deleted ] || [ "$st" = error ]; } && { echo "[$label] import $st"; return 2; }
    sleep 15
  done
  SNAP=$(aws ec2 describe-import-snapshot-tasks --region $REGION --import-task-ids "$task" --query 'ImportSnapshotTasks[0].SnapshotTaskDetail.SnapshotId' --output text)
  AMI=$(aws ec2 register-image --region $REGION --name "tyn-gp-${label}-${ts}" --architecture x86_64 \
    --root-device-name /dev/xvda \
    --block-device-mappings "DeviceName=/dev/xvda,Ebs={SnapshotId=$SNAP,VolumeSize=1,DeleteOnTermination=true,VolumeType=gp3}" \
    --virtualization-type hvm --boot-mode legacy-bios --ena-support --query ImageId --output text) || return 2
  IID=$(aws ec2 run-instances --region $REGION --image-id "$AMI" --instance-type "$INSTANCE_TYPE" \
    --security-group-ids "${SG_ID:?set SG_ID}" --tag-specifications "ResourceType=instance,Tags=[{Key=Name,Value=tyn-gp-${label}}]" \
    --query 'Instances[0].InstanceId' --output text) || return 2
  aws ec2 wait instance-running --region $REGION --instance-ids "$IID"
  local ip; ip=$(aws ec2 describe-instances --region $REGION --instance-ids "$IID" --query 'Reservations[0].Instances[0].PublicIpAddress' --output text)
  echo "  [$label] $IID ip=$ip; waiting for boot + GP_REPRO_RESULT (repro ~ iters-dependent)"

  local up=0
  for i in $(seq 1 75); do curl -s --max-time 4 "http://$ip:8080/health" | grep -q L2_OK && { up=1; break; }; sleep 4; done
  [ "$up" != 1 ] && echo "  [$label] NOTE: /health never came up — check serial (a #GP would crash boot)"

  # Poll serial for the result line OR a #GP (either is a terminal outcome).
  local con="" res="" gp=0
  for i in $(seq 1 60); do
    con=$(aws ec2 get-console-output --region $REGION --instance-id "$IID" --latest --output text 2>/dev/null | tr -d '\000')
    res=$(echo "$con" | grep -aoE "GP_REPRO_RESULT: .*" | tail -1)
    echo "$con" | grep -qaE "#GP ip=0x[0-9a-f]+ rsp=" && gp=1
    { [ -n "$res" ] || [ "$gp" = 1 ]; } && break
    sleep 10
  done
  echo "  [$label] serial #GP-count=$(echo "$con" | grep -acE '#GP ip=0x[0-9a-f]+ rsp=')  result=[$res]"
  # stash into globals for the verdict
  eval "RESULT_${label}=\"\$res\""
  eval "GP_${label}=\"\$gp\""

  # teardown THIS tree before the next (reset globals so cleanup doesn't double-run)
  aws ec2 terminate-instances --region $REGION --instance-ids "$IID" >/dev/null 2>&1; echo "  [$label] terminated $IID"
  aws ec2 deregister-image --region $REGION --image-id "$AMI" >/dev/null 2>&1
  sleep 5; aws ec2 delete-snapshot --region $REGION --snapshot-id "$SNAP" >/dev/null 2>&1
  aws s3 rm "s3://$BUCKET/$S3_KEY" --region $REGION >/dev/null 2>&1
  IID=""; AMI=""; SNAP=""; S3_KEY=""
}

RESULT_poison=""; GP_poison=0; RESULT_fixed=""; GP_fixed=0
run_tree poison "$POISON"
run_tree fixed  "$FIXED"

echo; echo "=== GP_HUNT DUAL-ACCEPTANCE VERDICT ==="
echo "  POISON: #GP=$GP_poison  $RESULT_poison"
echo "  FIXED : #GP=$GP_fixed  $RESULT_fixed"
# fixed must be clean: no #GP, mismatches=0.
fixed_clean=0
[ "$GP_fixed" = 0 ] && echo "$RESULT_fixed" | grep -q "mismatches=0" && fixed_clean=1
poison_surfaced=0
{ [ "$GP_poison" = 1 ] || echo "$RESULT_poison" | grep -qvE "mismatches=0"; } && poison_surfaced=1
echo "  ---"
[ "$fixed_clean" = 1 ] && echo "  FIXED tree CLEAN (no #GP, no corruption)" || echo "  FIXED tree NOT clean — the #GP is NOT closed; capture the RIP/GPRs"
[ "$poison_surfaced" = 1 ] \
  && echo "  POISON surfaced the class via this file-I/O repro — direct teeth hold" \
  || echo "  POISON stayed clean on this repro — WEAK trigger (expected: file-I/O surface is rare). Confirm class-live via ampapp amplifier before concluding."
