#!/bin/bash
# KVMCLOCK Step-3 validation on real Nitro (SMP). Deploys the current kernel (with
# kvmclock) + l2app, then checks: (1) [pvclock] activates on real KVM and its
# wall-base UTC matches real UTC; (2) the wall clock (Bandit's HTTP Date header)
# reads correct UTC and advances at the right rate over a measured interval;
# (3) it ran on a multi-vCPU instance (SMP path). Leak-proof cleanup.
set -u
REGION=us-east-1
BUCKET="tyn-images-$(aws sts get-caller-identity --query Account --output text)"
SG_ID="${SG_ID:-sg-0af575aad1ecce29c}"
INSTANCE_TYPE="${INSTANCE_TYPE:-c5.xlarge}"       # 4 vCPU = SMP
KERNEL="${KERNEL:-/home/ubuntu/kernel/target/x86_64-tyn/release/tyn-kernel}"
TS=$(date +%Y%m%d-%H%M%S)
KDIR=/home/ubuntu/kernel
REL=/home/ubuntu/l2app/_build/prod/rel/l2app
BASECPIO=$KDIR/src/otp-rootfs.cpio
CPIO=/home/ubuntu/work/kvmclock.cpio
IMAGE=/dev/shm/tyn-kvmclock-disk.raw
export AWS_PAGER=""
S3_KEY=""; SNAP=""; AMI=""; IID=""
cleanup() {
  echo "=== CLEANUP ==="
  [ -n "$IID" ]    && aws ec2 terminate-instances --region $REGION --instance-ids "$IID" >/dev/null 2>&1 && echo "  terminated $IID"
  [ -n "$AMI" ]    && aws ec2 deregister-image --region $REGION --image-id "$AMI" >/dev/null 2>&1 && echo "  deregistered $AMI"
  [ -n "$SNAP" ]   && { sleep 5; aws ec2 delete-snapshot --region $REGION --snapshot-id "$SNAP" >/dev/null 2>&1 && echo "  deleted $SNAP"; }
  [ -n "$S3_KEY" ] && aws s3 rm "s3://$BUCKET/$S3_KEY" --region $REGION >/dev/null 2>&1 && echo "  rm s3://$BUCKET/$S3_KEY"
}
trap cleanup EXIT

echo "=== pack l2app + build-disk (kvmclock kernel) ==="
"$KDIR/tyn-pack" "$REL" --base "$BASECPIO" -o "$CPIO" >/dev/null 2>&1 || { echo "FAIL pack"; exit 1; }
( cd "$KDIR" && CPIO="$CPIO" IMAGE="$IMAGE" KERNEL="$KERNEL" ./build-disk.sh ) >/tmp/kvmclock_bd.log 2>&1 || { echo "FAIL build-disk"; tail -6 /tmp/kvmclock_bd.log; exit 1; }
S3_KEY="tyn-kvmclock-${TS}.raw"
aws s3 cp "$IMAGE" "s3://$BUCKET/$S3_KEY" --region $REGION >/dev/null || { echo "FAIL s3"; exit 1; }
TASK=$(aws ec2 import-snapshot --region $REGION --description "kvmclock $TS" --disk-container "Format=RAW,UserBucket={S3Bucket=$BUCKET,S3Key=$S3_KEY}" --query 'ImportTaskId' --output text) || { echo "FAIL import"; exit 1; }
for i in $(seq 1 80); do st=$(aws ec2 describe-import-snapshot-tasks --region $REGION --import-task-ids "$TASK" --query 'ImportSnapshotTasks[0].SnapshotTaskDetail.Status' --output text 2>/dev/null); [ "$st" = completed ] && { echo "import done"; break; }; { [ "$st" = deleted ] || [ "$st" = error ]; } && { echo "import $st"; exit 1; }; sleep 15; done
SNAP=$(aws ec2 describe-import-snapshot-tasks --region $REGION --import-task-ids "$TASK" --query 'ImportSnapshotTasks[0].SnapshotTaskDetail.SnapshotId' --output text)
AMI=$(aws ec2 register-image --region $REGION --name "tyn-kvmclock-${TS}" --architecture x86_64 --root-device-name /dev/xvda --block-device-mappings "DeviceName=/dev/xvda,Ebs={SnapshotId=$SNAP,VolumeSize=1,DeleteOnTermination=true,VolumeType=gp3}" --virtualization-type hvm --boot-mode legacy-bios --ena-support --query 'ImageId' --output text) || { echo "FAIL register"; exit 1; }
IID=$(aws ec2 run-instances --region $REGION --image-id "$AMI" --instance-type "$INSTANCE_TYPE" --security-group-ids "$SG_ID" --tag-specifications "ResourceType=instance,Tags=[{Key=Name,Value=tyn-kvmclock-${TS}}]" --query 'Instances[0].InstanceId' --output text) || { echo "FAIL launch"; exit 1; }
aws ec2 wait instance-running --region $REGION --instance-ids "$IID"
IP=$(aws ec2 describe-instances --region $REGION --instance-ids "$IID" --query 'Reservations[0].Instances[0].PublicIpAddress' --output text)
echo "instance $IID ($INSTANCE_TYPE) ip $IP"

echo "=== wait for listener ==="; up=0
for i in $(seq 1 75); do curl -s --max-time 4 "http://$IP:8080/health" 2>/dev/null | grep -q L2_OK && { up=1; echo "listener up ~$((i*4))s"; break; }; sleep 4; done
[ "$up" = 1 ] || { echo "FAIL listener"; aws ec2 get-console-output --region $REGION --instance-id "$IID" --latest --output text 2>/dev/null | tr -d '\000' | tail -20; exit 1; }

echo "=== (1) accuracy: wall clock (HTTP Date header) vs real UTC ==="
REAL1=$(date -u +%s)
DATE1=$(curl -s -I --max-time 8 "http://$IP:8080/health" 2>/dev/null | grep -i '^date:' | sed 's/^[Dd]ate: //; s/\r//')
TYN1=$(date -u -d "$DATE1" +%s 2>/dev/null || echo 0)
echo "  real UTC=$(date -u -d @$REAL1) ; Tyn Date=[$DATE1] (epoch $TYN1)"
skew=$(( TYN1 - REAL1 )); echo "  skew=${skew}s (want |skew| <= a few s)"

echo "=== (2) rate: wall clock advances correctly over ~40s ==="
sleep 40
REAL2=$(date -u +%s)
DATE2=$(curl -s -I --max-time 8 "http://$IP:8080/health" 2>/dev/null | grep -i '^date:' | sed 's/^[Dd]ate: //; s/\r//')
TYN2=$(date -u -d "$DATE2" +%s 2>/dev/null || echo 0)
tyn_adv=$(( TYN2 - TYN1 )); real_adv=$(( REAL2 - REAL1 ))
echo "  Tyn advanced ${tyn_adv}s while real advanced ${real_adv}s (want ~equal — old 2GHz path would be off)"

echo "=== (3) [pvclock] activation on real KVM (SMP single-page STABLE) ==="
CON=$(aws ec2 get-console-output --region $REGION --instance-id "$IID" --latest --output text 2>/dev/null | tr -d '\000')
echo "$CON" | grep -aE "\[pvclock\]|\[clock\]" | head

echo "=== VERDICT ==="
fail=0
echo "$CON" | grep -qaE "\[pvclock\] kvmclock enabled" && echo "  PASS  kvmclock ACTIVE on Nitro (STABLE single-page)" || { echo "  FAIL  kvmclock did not activate ([pvclock] absent/fallback)"; fail=1; }
{ [ "${skew#-}" -le 5 ] 2>/dev/null && echo "  PASS  accuracy: Tyn wall clock within ${skew}s of real UTC"; } || { echo "  FAIL  accuracy: skew=${skew}s"; fail=1; }
d=$(( tyn_adv - real_adv )); { [ "${d#-}" -le 3 ] 2>/dev/null && echo "  PASS  rate: Tyn advanced ${tyn_adv}s vs real ${real_adv}s (delta ${d}s)"; } || { echo "  FAIL  rate off: delta ${d}s"; fail=1; }
[ "$fail" = 0 ] && echo "KVMCLOCK: PASS (active + accurate + correct rate on real SMP)" || echo "KVMCLOCK: FAIL"
