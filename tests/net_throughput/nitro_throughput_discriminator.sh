#!/bin/bash
# NET_THROUGHPUT_DISCRIMINATOR — is Tyn's ~400 KB/s a DIST quirk or a SYSTEMIC
# network-stack ceiling that also caps HTTP/TLS (the primary serving path)?
#
# Measures SINGLE-CONNECTION BULK throughput (NOT aggregate request rate) of a
# large response over the SHARED network stack, HTTP and HTTPS separately, on
# real Nitro. One node — no cluster needed. Short watched window, not a soak.
#
# Three-way verdict:
#   HTTP fast AND HTTPS fast  -> dist-specific (pivot to isolation, dist banked).
#   HTTP fast, HTTPS slow     -> TLS-data-path (rustls NIF) — its own hunt.
#   HTTP slow AND HTTPS slow  -> SYSTEMIC stack ceiling — jumps ahead of isolation.
#
# TEETH: same-region EC2 -> instance-public-IP is multi-Gbps, so any single-digit
# MB/s is Tyn's ceiling, not the path. Size sweep (1/10/50 MB): flat MB/s => per-
# round-trip/latency bound; ramp => window bound (same discriminator as dist).
# Leak-proof trap + deadman. $0 on abort.
set -u
REGION=us-east-1
INSTANCE_TYPE="${INSTANCE_TYPE:-c5.xlarge}"
IMAGE="${IMAGE:-/dev/shm/nt-disk.raw}"
REL="${REL:-/home/ubuntu/soak_app/_build/prod/rel/soak_app}"
KDIR=/home/ubuntu/kernel
FIXED="${FIXED:-$KDIR/target/x86_64-tyn/release/tyn-kernel}"
SG_ID="${SG_ID:?set SG_ID (must allow 8080 + 8443 from you)}"
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
( sleep 2400; echo "!! DEADMAN"; kill -TERM -$$ 2>/dev/null ) & DEADMAN=$!

if [ "${REBUILD:-1}" = 1 ]; then
  CERT=/home/ubuntu/work/nt_cert.pem; KEY=/home/ubuntu/work/nt_key.pem
  [ -f "$CERT" ] || openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 \
    -nodes -keyout "$KEY" -out "$CERT" -days 3 -subj "/CN=nt.local" >/dev/null 2>&1
  CPIO=/home/ubuntu/work/nt.cpio
  "$KDIR/tyn-pack" "$REL" --base "$KDIR/src/otp-rootfs.cpio" \
    --env TYN_TLS_CERT_B64="$(base64 -w0 "$CERT")" --env TYN_TLS_KEY_B64="$(base64 -w0 "$KEY")" \
    -o "$CPIO" >/dev/null 2>&1 || { echo "FAIL pack"; exit 1; }
  ( cd "$KDIR" && CPIO="$CPIO" IMAGE="$IMAGE" KERNEL="$FIXED" ./build-disk.sh ) \
    >/tmp/nt_bd.log 2>&1 || { echo "FAIL build-disk"; tail -6 /tmp/nt_bd.log; exit 1; }
fi

echo "=== S3 + import + register ==="
S3_KEY="tyn-nt-${TS}.raw"
aws s3 cp "$IMAGE" "s3://$BUCKET/$S3_KEY" --region $REGION >/dev/null || { echo "FAIL s3"; exit 1; }
TASK=$(aws ec2 import-snapshot --region $REGION --description "tyn-nt $TS" \
  --disk-container "Format=RAW,UserBucket={S3Bucket=$BUCKET,S3Key=$S3_KEY}" --query ImportTaskId --output text) || exit 1
for i in $(seq 1 80); do
  st=$(aws ec2 describe-import-snapshot-tasks --region $REGION --import-task-ids "$TASK" --query 'ImportSnapshotTasks[0].SnapshotTaskDetail.Status' --output text 2>/dev/null)
  [ "$st" = completed ] && break
  { [ "$st" = deleted ] || [ "$st" = error ]; } && { echo "FAIL import $st"; exit 1; }
  sleep 15
done
SNAP=$(aws ec2 describe-import-snapshot-tasks --region $REGION --import-task-ids "$TASK" --query 'ImportSnapshotTasks[0].SnapshotTaskDetail.SnapshotId' --output text)
AMI=$(aws ec2 register-image --region $REGION --name "tyn-nt-${TS}" --architecture x86_64 --root-device-name /dev/xvda \
  --block-device-mappings "DeviceName=/dev/xvda,Ebs={SnapshotId=$SNAP,VolumeSize=1,DeleteOnTermination=true,VolumeType=gp3}" \
  --virtualization-type hvm --boot-mode legacy-bios --ena-support --query ImageId --output text) || exit 1
IID=$(aws ec2 run-instances --region $REGION --image-id "$AMI" --instance-type "$INSTANCE_TYPE" \
  --security-group-ids "$SG_ID" --tag-specifications "ResourceType=instance,Tags=[{Key=Name,Value=tyn-nt-${TS}}]" \
  --query 'Instances[0].InstanceId' --output text) || exit 1
aws ec2 wait instance-running --region $REGION --instance-ids "$IID"
IP=$(aws ec2 describe-instances --region $REGION --instance-ids "$IID" --query 'Reservations[0].Instances[0].PublicIpAddress' --output text)
echo "  instance $IID ip=$IP"

echo "=== GATE: boot + /health (plaintext) ==="
up=0; for i in $(seq 1 75); do curl -s --max-time 4 "http://$IP:8080/health" | grep -q L2_OK && { up=1; break; }; sleep 4; done
[ "$up" != 1 ] && { echo "ABORT: node never served /health"; exit 1; }
echo "  HTTP up"
tls=0; for i in $(seq 1 20); do curl -sk --max-time 5 "https://$IP:8443/health" | grep -q L2_OK && { tls=1; break; }; sleep 4; done
[ "$tls" = 1 ] && echo "  HTTPS (rustls) up" || echo "  WARN: HTTPS not up (HTTP-only verdict)"

measure() {  # $1=base url  $2=label
  # warmup (prime any lazy path), then 2 timed reads per size for consistency
  curl -sk -o /dev/null --max-time 60 "$1/big?mb=1" 2>/dev/null
  for MB in 1 10 50; do
    for rep in 1 2; do
      R=$(curl -sk -o /dev/null --max-time 180 -w '%{speed_download} %{time_total} %{size_download}' "$1/big?mb=$MB" 2>/dev/null)
      BPS=$(echo "$R" | awk '{print $1}')
      MBPS=$(awk -v b="${BPS:-0}" 'BEGIN{printf "%.2f", b/1048576}')
      echo "  [$2] mb=$MB rep=$rep -> ${MBPS} MB/s (speed_bps time_s bytes: $R)"
    done
  done
}

echo "=== MEASURE: single-connection bulk throughput (HTTP :8080) ==="
measure "http://$IP:8080" HTTP
if [ "$tls" = 1 ]; then
  echo "=== MEASURE: single-connection bulk throughput (HTTPS :8443, rustls) ==="
  measure "https://$IP:8443" HTTPS
fi

echo "=== VERDICT GUIDE ==="
echo "  dist reference = ~0.4 MB/s (400 KB/s); healthy = tens-hundreds MB/s."
echo "  HTTP fast & HTTPS fast -> dist-specific (pivot isolation)."
echo "  HTTP fast, HTTPS slow  -> TLS-data-path hunt."
echo "  HTTP slow & HTTPS slow -> SYSTEMIC ceiling (jumps ahead of isolation)."
echo "=== DONE ==="
