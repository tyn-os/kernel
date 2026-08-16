#!/bin/bash
# BUG-8 recovery DIAGNOSTIC run (one run, discriminates all four hypotheses).
# Deploy l2app (kernel built with the [diag] instrumentation in net::poll), flood
# it (sustain, past the panic point), then OBSERVE a long post-flood window while
# the kernel logs [diag] free-heap + TCP-state histogram every ~2s. Read the full
# serial console and dump the [diag] timeline: does heap recover or stay pinned
# (H1/fix-strands), does the listener pool return to Listen (H2), does the kernel
# keep logging while HTTP won't serve (H3, BEAM wedge), and does /health return
# late (H4, slow-not-wedged). Leak-proof cleanup on any exit.
set -u
REGION=us-east-1
BUCKET="tyn-images-$(aws sts get-caller-identity --query Account --output text)"
SG_ID="${SG_ID:-sg-0af575aad1ecce29c}"
INSTANCE_TYPE="${INSTANCE_TYPE:-c5.xlarge}"
FLOOD_MAX="${FLOOD_MAX:-4000}"; FLOOD_BATCH="${FLOOD_BATCH:-250}"
OBSERVE_SECS="${OBSERVE_SECS:-240}"      # long post-flood window (minutes, not 18s)
TS=$(date +%Y%m%d-%H%M%S)
KDIR=/home/ubuntu/kernel
REL=/home/ubuntu/l2app/_build/prod/rel/l2app
BASECPIO=$KDIR/src/otp-rootfs.cpio
CPIO=/home/ubuntu/work/l2_diag.cpio
IMAGE=/dev/shm/tyn-diag-disk.raw
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

echo "=== pack + build-disk (l2app, [diag] kernel) ==="
"$KDIR/tyn-pack" "$REL" --base "$BASECPIO" -o "$CPIO" >/dev/null 2>&1 || { echo "FAIL pack"; exit 1; }
( cd "$KDIR" && CPIO="$CPIO" IMAGE="$IMAGE" ./build-disk.sh ) >/tmp/diag_builddisk.log 2>&1 || { echo "FAIL build-disk"; tail -8 /tmp/diag_builddisk.log; exit 1; }

echo "=== S3 + import-snapshot ==="
S3_KEY="tyn-diag-${TS}.raw"
aws s3 cp "$IMAGE" "s3://$BUCKET/$S3_KEY" --region $REGION >/dev/null || { echo "FAIL s3"; exit 1; }
TASK=$(aws ec2 import-snapshot --region $REGION --description "diag $TS" \
  --disk-container "Format=RAW,UserBucket={S3Bucket=$BUCKET,S3Key=$S3_KEY}" --query 'ImportTaskId' --output text) || { echo "FAIL import"; exit 1; }
for i in $(seq 1 80); do
  st=$(aws ec2 describe-import-snapshot-tasks --region $REGION --import-task-ids "$TASK" --query 'ImportSnapshotTasks[0].SnapshotTaskDetail.Status' --output text 2>/dev/null)
  [ "$st" = completed ] && { echo "import done ~$((i*15))s"; break; }
  { [ "$st" = deleted ] || [ "$st" = error ]; } && { echo "import $st"; exit 1; }
  sleep 15
done
SNAP=$(aws ec2 describe-import-snapshot-tasks --region $REGION --import-task-ids "$TASK" --query 'ImportSnapshotTasks[0].SnapshotTaskDetail.SnapshotId' --output text)
[ -n "$SNAP" ] && [ "$SNAP" != None ] || { echo "FAIL no snap"; exit 1; }

AMI=$(aws ec2 register-image --region $REGION --name "tyn-diag-${TS}" \
  --description "Tyn BUG-8 diag (throwaway)" --architecture x86_64 --root-device-name /dev/xvda \
  --block-device-mappings "DeviceName=/dev/xvda,Ebs={SnapshotId=$SNAP,VolumeSize=1,DeleteOnTermination=true,VolumeType=gp3}" \
  --virtualization-type hvm --boot-mode legacy-bios --ena-support --query 'ImageId' --output text) || { echo "FAIL register"; exit 1; }
IID=$(aws ec2 run-instances --region $REGION --image-id "$AMI" --instance-type "$INSTANCE_TYPE" \
  --security-group-ids "$SG_ID" --tag-specifications "ResourceType=instance,Tags=[{Key=Name,Value=tyn-diag-${TS}}]" \
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

echo "=== flood (sustain, max=$FLOOD_MAX) ==="
ulimit -n 65535 2>/dev/null
python3 "$HERE/fd_flood.py" "$IP" 8080 "$FLOOD_MAX" "$FLOOD_BATCH" sustain | tee /tmp/diag_flood.out
echo "=== flood done — OBSERVE post-flood recovery for ${OBSERVE_SECS}s (probe /health every 15s) ==="
t=0
recov_at=""
while [ "$t" -lt "$OBSERVE_SECS" ]; do
  sleep 15; t=$((t+15))
  r=$(curl -s --max-time 6 "http://$IP:8080/health" 2>/dev/null)
  echo "  post-flood t=+${t}s  /health -> [${r:-<none>}]"
  [ -z "$recov_at" ] && echo "$r" | grep -q L2_OK && recov_at=$t
done

echo "=== legit-close churn (80 concurrent complete requests — must serve + not be aborted) ==="
: > /tmp/diag_churn.out
for j in $(seq 1 80); do ( curl -s --max-time 8 "http://$IP:8080/health" 2>/dev/null | grep -q L2_OK && echo Y >> /tmp/diag_churn.out ) & done; wait
churn=$(wc -l < /tmp/diag_churn.out | tr -d ' ')

echo "=== full serial console: [diag] timeline + panic scan ==="
CON=$(aws ec2 get-console-output --region $REGION --instance-id "$IID" --latest --output text 2>/dev/null | tr -d '\000')
echo "$CON" | grep -aE "^\[diag\]" | tail -40
echo "-- panic scan --"; echo "$CON" | grep -aE "KERNEL PANIC|#GP|#PF" | head
echo "$CON" | grep -qaE "KERNEL PANIC|#GP|#PF" && PANIC=yes || PANIC=no
last=$(echo "$CON" | grep -aE "^\[diag\]" | tail -1)
echo "-- last [diag] (final heap/pool state) --"; echo "$last"

# Parse final [diag] for the recovery proof.
fh=$(echo "$last" | grep -oE "heap_free=[0-9]+" | cut -d= -f2)      # KiB
cl=$(echo "$last" | grep -oE "closing=[0-9]+" | cut -d= -f2)
echo "=== BUG-8 recovery acceptance (real SMP) ==="
fail=0; ck(){ if [ "$2" = "$3" ]; then echo "  PASS  $1"; else echo "  FAIL  $1 ($2)"; fail=1; fi; }
ck "(1) no kernel panic under flood" "$PANIC" "no"
# (3a) heap recovered: free climbs well above the 4 MiB (4096 KiB) reserve
if [ "${fh:-0}" -ge 8000 ] 2>/dev/null; then echo "  PASS  (3) heap_free recovered to ${fh}KiB (>>4096 reserve)"; else echo "  FAIL  (3) heap_free still pinned at ${fh}KiB"; fail=1; fi
# (3b) stranded sockets drained
ck "(3) closing sockets drained to 0" "${cl:-x}" "0"
# (3c) node serves again
if [ -n "$recov_at" ]; then echo "  PASS  (3) /health recovered at ~+${recov_at}s post-flood"; else echo "  FAIL  (3) /health never recovered in ${OBSERVE_SECS}s"; fail=1; fi
# (4) legit-close correctness: normal churn serves (not aborted early / no data loss)
if [ "${churn:-0}" -ge 76 ] 2>/dev/null; then echo "  PASS  (4) legit-close churn $churn/80 served cleanly"; else echo "  FAIL  (4) churn only $churn/80"; fail=1; fi
[ "$fail" = 0 ] && echo "BUG8_RECOVERY: PASS" || echo "BUG8_RECOVERY: FAIL"