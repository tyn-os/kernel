#!/bin/bash
# Batched Nitro run for Phase-2 Layer-2. Deploys l2app to a REAL Nitro instance
# (SMP + ENA) and runs all three adversarial checks against it:
#   - tmpfs cap-under-concurrency (real SMP parallelism vs the coarse Mutex) via
#     the CC_ lines on the serial console
#   - slow-loris + fd/socket-exhaustion via HTTP from this build host
# Then LEAK-PROOF cleanup (terminate + deregister + delete snapshot + rm S3) in a
# trap that runs on any exit. Throwaway AMI — never published.
set -u
REGION=us-east-1
BUCKET="tyn-images-$(aws sts get-caller-identity --query Account --output text)"
SG_ID="${SG_ID:-sg-0af575aad1ecce29c}"          # tyn-sg (8080 + 9090 open)
INSTANCE_TYPE="${INSTANCE_TYPE:-c5.xlarge}"      # 4 vCPU: real multi-core SMP
TS=$(date +%Y%m%d-%H%M%S)
KDIR=/home/ubuntu/kernel
REL=/home/ubuntu/l2app/_build/prod/rel/l2app
BASECPIO=$KDIR/src/otp-rootfs.cpio
CPIO=/home/ubuntu/work/l2_nitro.cpio
IMAGE=/dev/shm/tyn-l2-disk.raw
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
( cd "$KDIR" && CPIO="$CPIO" IMAGE="$IMAGE" ./build-disk.sh ) >/tmp/l2_builddisk.log 2>&1 || { echo "FAIL build-disk"; tail -8 /tmp/l2_builddisk.log; exit 1; }
ls -lh "$IMAGE"

echo "=== S3 upload ==="
S3_KEY="tyn-l2-${TS}.raw"
aws s3 cp "$IMAGE" "s3://$BUCKET/$S3_KEY" --region $REGION >/dev/null || { echo "FAIL s3"; exit 1; }

echo "=== import-snapshot (5-10 min) ==="
TASK=$(aws ec2 import-snapshot --region $REGION --description "l2 $TS" \
  --disk-container "Format=RAW,UserBucket={S3Bucket=$BUCKET,S3Key=$S3_KEY}" \
  --query 'ImportTaskId' --output text) || { echo "FAIL import"; exit 1; }
echo "import task $TASK"
for i in $(seq 1 80); do
  st=$(aws ec2 describe-import-snapshot-tasks --region $REGION --import-task-ids "$TASK" --query 'ImportSnapshotTasks[0].SnapshotTaskDetail.Status' --output text 2>/dev/null)
  [ "$st" = completed ] && { echo "import completed at ~$((i*15))s"; break; }
  { [ "$st" = deleted ] || [ "$st" = error ]; } && { echo "import $st"; exit 1; }
  sleep 15
done
SNAP=$(aws ec2 describe-import-snapshot-tasks --region $REGION --import-task-ids "$TASK" --query 'ImportSnapshotTasks[0].SnapshotTaskDetail.SnapshotId' --output text)
[ -n "$SNAP" ] && [ "$SNAP" != None ] || { echo "FAIL no snapshot"; exit 1; }
echo "snapshot $SNAP"

echo "=== register AMI ==="
AMI=$(aws ec2 register-image --region $REGION --name "tyn-l2-${TS}" \
  --description "Tyn L2 resiliency test (throwaway)" --architecture x86_64 --root-device-name /dev/xvda \
  --block-device-mappings "DeviceName=/dev/xvda,Ebs={SnapshotId=$SNAP,VolumeSize=1,DeleteOnTermination=true,VolumeType=gp3}" \
  --virtualization-type hvm --boot-mode legacy-bios --ena-support --query 'ImageId' --output text) || { echo "FAIL register"; exit 1; }
echo "AMI $AMI"

echo "=== launch $INSTANCE_TYPE ==="
IID=$(aws ec2 run-instances --region $REGION --image-id "$AMI" --instance-type "$INSTANCE_TYPE" \
  --security-group-ids "$SG_ID" --tag-specifications "ResourceType=instance,Tags=[{Key=Name,Value=tyn-l2-${TS}}]" \
  --query 'Instances[0].InstanceId' --output text) || { echo "FAIL launch"; exit 1; }
aws ec2 wait instance-running --region $REGION --instance-ids "$IID"
IP=$(aws ec2 describe-instances --region $REGION --instance-ids "$IID" --query 'Reservations[0].Instances[0].PublicIpAddress' --output text)
echo "instance $IID  ip $IP"

echo "=== wait for HTTP listener (:8080/health) ==="
up=0
for i in $(seq 1 75); do
  curl -s --max-time 4 "http://$IP:8080/health" 2>/dev/null | grep -q L2_OK && { up=1; echo "listener up (~$((i*4))s after running)"; break; }
  sleep 4
done
if [ "$up" != 1 ]; then
  echo "FAIL listener never came up — console tail:"
  aws ec2 get-console-output --region $REGION --instance-id "$IID" --latest --output text 2>/dev/null | tr -d '\000' | tail -30
  exit 1
fi

echo "=== NET ATTACKS (full scale) from build host -> $IP ==="
SL_N=300 FD_N=4000 SL_HOLD=45 FD_HOLD=25 bash "$HERE/drive_net_attacks.sh" "$IP" 8080
NET_RC=$?

echo "=== poll serial console for cap probe (CC_END) ==="
CON=""
for i in $(seq 1 14); do
  CON=$(aws ec2 get-console-output --region $REGION --instance-id "$IID" --latest --output text 2>/dev/null | tr -d '\000')
  echo "$CON" | grep -qaE "^CC_END" && { echo "CC_END on console (poll $i)"; break; }
  sleep 15
done
echo "--- CC_ / tmpfs lines ---"; echo "$CON" | grep -aE "^CC_|\[tmpfs\]" || echo "(none)"
cf() { echo "$CON" | grep -aE "^$1 " | tail -1 | grep -oE "$2=[^ ]+" | head -1 | cut -d= -f2; }
within=$(cf CC_TOTAL within_cap); corrupt=$(cf CC_CORRUPT count); contended=$(cf CC_TEETH contended)
recov=$(cf CC_RECOVERY ok); alive=$(cf CC_END node_alive); okc=$(cf CC_RESULTS ok); enc=$(cf CC_RESULTS enospc)
faults=$(echo "$CON" | grep -acE "#GP|#PF|panic")

echo "=== VERDICT (Nitro real SMP = $INSTANCE_TYPE) ==="
fail=0
ck(){ if [ "$2" = "$3" ]; then echo "  PASS  $1"; else echo "  FAIL  $1 (want $3 got '$2')"; fail=1; fi; }
echo "-- cap-under-concurrency (real SMP parallelism vs coarse Mutex) --"
ck "TEETH cap contended (some ok + some ENOSPC)" "$contended" "true"
ck "(2) invariant: total within cap under SMP race" "$within" "true"
ck "(2) no interleave corruption" "$corrupt" "0"
ck "(3) recovery after free" "$recov" "true"
ck "(1) node alive after the storm" "$alive" "true"
echo "     (ok=$okc enospc=$enc)"
echo "-- network adversarial (slow-loris + fd-exhaustion, real ENA) --"
ck "net attacks driver (teeth+serve-during+recovery)" "$NET_RC" "0"
echo "-- kernel integrity across ALL attacks --"
ck "no #GP/#PF/panic on console" "$faults" "0"
if [ "$fail" = 0 ]; then echo "L2_NITRO: PASS"; else echo "L2_NITRO: FAIL"; fi
