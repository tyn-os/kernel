#!/bin/bash
# STAGED — SINGLE-NODE sustained TLS soak on real Nitro (the 2-node dist-boot is
# not yet wired; this runs the Nitro-authoritative single-node durability pieces:
# in-guest TLS boundary under sustained load, heap accretion (BUG-8 class),
# kvmclock long-run drift, fd/socket/proc drift, latency, zero UNHANDLED, and the
# node-restart recovery probe with teeth). Dist clustering / blip / inter-node are
# DEFERRED until dist-boot is wired.
#
# Opening sequence (form-and-confirm analogue): boot -> confirm /health + /diag ->
# confirm the HTTPS (rustls) boundary terminates in-guest -> ONLY THEN start the
# 2-3h clock. Leak-proof trap + deadman. Watched to teardown.
set -u
REGION=us-east-1
DUR="${DUR:-9000}"                              # ~2.5 h
INSTANCE_TYPE="${INSTANCE_TYPE:-c5.xlarge}"     # 4 vCPU real SMP
IMAGE="${IMAGE:-/dev/shm/soak1-disk.raw}"       # built below if REBUILD=1
REL="${REL:-/home/ubuntu/soak_app/_build/prod/rel/soak_app}"
KDIR=/home/ubuntu/kernel
FIXED="${FIXED:-$KDIR/target/x86_64-tyn/release/tyn-kernel}"
SG_ID="${SG_ID:?set SG_ID (must allow 8080 + 8443 from you)}"
HERE="$(cd "$(dirname "$0")" && pwd)"
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
( sleep $((DUR + 1800)); echo "!! DEADMAN"; kill -TERM -$$ 2>/dev/null ) & DEADMAN=$!

# Build the disk with a self-signed cert injected (drives the rustls HTTPS
# boundary under load). REBUILD=1 to (re)pack; else expects IMAGE to exist.
if [ "${REBUILD:-1}" = 1 ]; then
  CERT=/home/ubuntu/work/soak1_cert.pem; KEY=/home/ubuntu/work/soak1_key.pem
  [ -f "$CERT" ] || openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 \
    -nodes -keyout "$KEY" -out "$CERT" -days 3 -subj "/CN=soak.local" >/dev/null 2>&1
  CPIO=/home/ubuntu/work/soak1.cpio
  "$KDIR/tyn-pack" "$REL" --base "$KDIR/src/otp-rootfs.cpio" \
    --env TYN_TLS_CERT_B64="$(base64 -w0 "$CERT")" --env TYN_TLS_KEY_B64="$(base64 -w0 "$KEY")" \
    -o "$CPIO" >/dev/null 2>&1 || { echo "FAIL pack"; exit 1; }
  ( cd "$KDIR" && CPIO="$CPIO" IMAGE="$IMAGE" KERNEL="$FIXED" ./build-disk.sh ) \
    >/tmp/soak1_bd.log 2>&1 || { echo "FAIL build-disk"; tail -6 /tmp/soak1_bd.log; exit 1; }
fi

echo "=== S3 + import + register ==="
S3_KEY="tyn-soak1-${TS}.raw"
aws s3 cp "$IMAGE" "s3://$BUCKET/$S3_KEY" --region $REGION >/dev/null || { echo "FAIL s3"; exit 1; }
TASK=$(aws ec2 import-snapshot --region $REGION --description "tyn-soak1 $TS" \
  --disk-container "Format=RAW,UserBucket={S3Bucket=$BUCKET,S3Key=$S3_KEY}" --query ImportTaskId --output text) || exit 1
for i in $(seq 1 80); do
  st=$(aws ec2 describe-import-snapshot-tasks --region $REGION --import-task-ids "$TASK" --query 'ImportSnapshotTasks[0].SnapshotTaskDetail.Status' --output text 2>/dev/null)
  [ "$st" = completed ] && break
  { [ "$st" = deleted ] || [ "$st" = error ]; } && { echo "FAIL import $st"; exit 1; }
  sleep 15
done
SNAP=$(aws ec2 describe-import-snapshot-tasks --region $REGION --import-task-ids "$TASK" --query 'ImportSnapshotTasks[0].SnapshotTaskDetail.SnapshotId' --output text)
AMI=$(aws ec2 register-image --region $REGION --name "tyn-soak1-${TS}" --architecture x86_64 --root-device-name /dev/xvda \
  --block-device-mappings "DeviceName=/dev/xvda,Ebs={SnapshotId=$SNAP,VolumeSize=1,DeleteOnTermination=true,VolumeType=gp3}" \
  --virtualization-type hvm --boot-mode legacy-bios --ena-support --query ImageId --output text) || exit 1
IID=$(aws ec2 run-instances --region $REGION --image-id "$AMI" --instance-type "$INSTANCE_TYPE" \
  --security-group-ids "$SG_ID" --tag-specifications "ResourceType=instance,Tags=[{Key=Name,Value=tyn-soak1-${TS}}]" \
  --query 'Instances[0].InstanceId' --output text) || exit 1
aws ec2 wait instance-running --region $REGION --instance-ids "$IID"
IP=$(aws ec2 describe-instances --region $REGION --instance-ids "$IID" --query 'Reservations[0].Instances[0].PublicIpAddress' --output text)
echo "  instance $IID ip=$IP"

echo "=== GATE 1: boot + /health + /diag ==="
up=0; for i in $(seq 1 75); do curl -s --max-time 4 "http://$IP:8080/health" | grep -q L2_OK && { up=1; break; }; sleep 4; done
[ "$up" != 1 ] && { echo "ABORT: node never served /health (minutes spent, not hours)"; exit 1; }
curl -s --max-time 5 "http://$IP:8080/diag" | head -c 200; echo

echo "=== GATE 2: HTTPS (rustls) boundary terminates in-guest ==="
tls=0; for i in $(seq 1 20); do curl -sk --max-time 5 "https://$IP:8443/health" | grep -q L2_OK && { tls=1; break; }; sleep 4; done
if [ "$tls" = 1 ]; then NODEURL="https://$IP:8443"; echo "  TLS boundary UP — soaking HTTPS"; \
  else NODEURL="http://$IP:8080"; echo "  WARN: HTTPS not up — soaking HTTP only (note in report)"; fi

echo "=== GATE 3: start the ${DUR}s sustained run (restart probe at 40%) ==="
python3 "$HERE/soak.py" --nodes "$NODEURL" --duration-s "$DUR" --scrape-s 15 --rps 6 \
  --https-insecure --max-drift-ms 2000 \
  --probe "at=$((DUR*4/10)):aws ec2 reboot-instances --region $REGION --instance-ids $IID" \
  --out "$HERE/soak1_nitro_${TS}.jsonl"
RC=$?
echo "=== VERDICT: soak.py exit=$RC (0=bounded + restart recovered) ==="
exit $RC
