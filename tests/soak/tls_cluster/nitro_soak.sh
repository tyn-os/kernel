#!/bin/bash
# STAGED — the authoritative 2-node sustained TLS+cluster soak on real Nitro.
# NOT run unattended: launch it in a WATCHED window (SUSTAINED_TLS_CLUSTER_SOAK
# discipline — real multi-hour, multi-node instance-hours). Leak-proof: cleanup
# trap on ANY exit + a deadman timer that force-terminates if the run overruns.
#
# (Distinct from tests/soak/nitro_soak.sh, which is the BUG-1 SMP-regression soak.)
#
# Prereqs (do these in the watched window, one at a time, validated free first):
#   1. tests/soak/tls_cluster/local_validate.sh has PASSED on the build host.
#   2. The soak disk image is built (soak_app release packed with a TLS cert into
#      the A'-capable beam base — see README "Assemble").
#   3. IMAGE points at that raw disk; SG opens 8080 (HTTP/diag), 8443 (HTTPS),
#      and the dist port range between the two instances' private IPs.
#
# Flow: import snapshot -> register AMI -> launch 2 instances -> wait HTTP ->
# host-driven cluster formation via /connect (proven dist-spike approach, IP-
# literal node names, no DNS) -> run soak.py for DUR against BOTH nodes with a
# restart probe -> VERDICT -> teardown (both instances, AMI, snapshot, S3).
# Bounded-quantities assertion lives in soak.py.
set -u
REGION=us-east-1
DUR="${DUR:-9000}"                          # ~2.5 h
INSTANCE_TYPE="${INSTANCE_TYPE:-c5.xlarge}"
IMAGE="${IMAGE:-/dev/shm/soak-disk.raw}"
SG_ID="${SG_ID:?set SG_ID (must allow 8080/8443 from you + dist ports between nodes)}"
COOKIE="${COOKIE:-soak_$(od -An -N4 -tx1 /dev/urandom | tr -d ' ')}"
HERE="$(cd "$(dirname "$0")" && pwd)"
BUCKET="tyn-images-$(aws sts get-caller-identity --query Account --output text)"
TS="$(date +%Y%m%d-%H%M%S)"
export AWS_PAGER=""
S3_KEY=""; SNAP=""; AMI=""; IID1=""; IID2=""; DEADMAN=""

cleanup() {
  echo "=== CLEANUP ==="
  [ -n "$DEADMAN" ] && kill "$DEADMAN" 2>/dev/null
  for I in "$IID1" "$IID2"; do
    [ -n "$I" ] && aws ec2 terminate-instances --region $REGION --instance-ids "$I" >/dev/null 2>&1 && echo "  terminated $I"
  done
  [ -n "$AMI" ]  && aws ec2 deregister-image --region $REGION --image-id "$AMI" >/dev/null 2>&1 && echo "  deregistered $AMI"
  [ -n "$SNAP" ] && { sleep 5; aws ec2 delete-snapshot --region $REGION --snapshot-id "$SNAP" >/dev/null 2>&1 && echo "  deleted $SNAP"; }
  [ -n "$S3_KEY" ] && aws s3 rm "s3://$BUCKET/$S3_KEY" --region $REGION >/dev/null 2>&1 && echo "  rm s3://$BUCKET/$S3_KEY"
}
trap cleanup EXIT INT TERM

# Deadman: hard cap at DUR + 30 min. If the run wedges, this force-kills the
# whole process group so instances never linger silently accruing cost.
( sleep $((DUR + 1800)); echo "!! DEADMAN fired — killing run"; kill -TERM -$$ 2>/dev/null ) &
DEADMAN=$!

echo "=== S3 upload + snapshot import ==="
S3_KEY="tyn-tlssoak-${TS}.raw"
aws s3 cp "$IMAGE" "s3://$BUCKET/$S3_KEY" --region $REGION >/dev/null || { echo "FAIL s3"; exit 1; }
TASK=$(aws ec2 import-snapshot --region $REGION --description "tyn-tlssoak $TS" \
  --disk-container "Format=RAW,UserBucket={S3Bucket=$BUCKET,S3Key=$S3_KEY}" \
  --query 'ImportTaskId' --output text) || { echo "FAIL import"; exit 1; }
for i in $(seq 1 80); do
  st=$(aws ec2 describe-import-snapshot-tasks --region $REGION --import-task-ids "$TASK" \
        --query 'ImportSnapshotTasks[0].SnapshotTaskDetail.Status' --output text 2>/dev/null)
  [ "$st" = completed ] && break
  { [ "$st" = deleted ] || [ "$st" = error ]; } && { echo "FAIL import $st"; exit 1; }
  sleep 15
done
SNAP=$(aws ec2 describe-import-snapshot-tasks --region $REGION --import-task-ids "$TASK" \
  --query 'ImportSnapshotTasks[0].SnapshotTaskDetail.SnapshotId' --output text)
AMI=$(aws ec2 register-image --region $REGION --name "tyn-tlssoak-${TS}" --architecture x86_64 \
  --root-device-name /dev/xvda \
  --block-device-mappings "DeviceName=/dev/xvda,Ebs={SnapshotId=$SNAP,VolumeSize=1,DeleteOnTermination=true,VolumeType=gp3}" \
  --virtualization-type hvm --boot-mode legacy-bios --ena-support --query 'ImageId' --output text) \
  || { echo "FAIL register"; exit 1; }

echo "=== launch 2 instances ==="
launch() { aws ec2 run-instances --region $REGION --image-id "$AMI" --instance-type "$INSTANCE_TYPE" \
  --security-group-ids "$SG_ID" --tag-specifications "ResourceType=instance,Tags=[{Key=Name,Value=tyn-tlssoak-${TS}-$1}]" \
  --query 'Instances[0].InstanceId' --output text; }
IID1=$(launch a) || exit 1
IID2=$(launch b) || exit 1
aws ec2 wait instance-running --region $REGION --instance-ids "$IID1" "$IID2"
addr() { aws ec2 describe-instances --region $REGION --instance-ids "$1" \
  --query "Reservations[0].Instances[0].$2" --output text; }
IP1=$(addr "$IID1" PublicIpAddress);  IP2=$(addr "$IID2" PublicIpAddress)
PRIV1=$(addr "$IID1" PrivateIpAddress); PRIV2=$(addr "$IID2" PrivateIpAddress)
echo "  node a: pub=$IP1 priv=$PRIV1 | node b: pub=$IP2 priv=$PRIV2"

echo "=== wait for both to serve /health ==="
for IP in "$IP1" "$IP2"; do
  ok=0
  for i in $(seq 1 75); do curl -s --max-time 4 "http://$IP:8080/health" | grep -q L2_OK && { ok=1; break; }; sleep 4; done
  [ "$ok" != 1 ] && { echo "FAIL: $IP never served"; exit 1; }
done
echo "  both up"

echo "=== host-driven cluster formation (IP-literal names, no DNS) ==="
# each node connects to the OTHER's private IP; nodes must have booted distributed
# as n@<priv-ip> per the dist-spike recipe (see README / docs/DIST_ACCEPT_HUNT.md).
curl -s -X POST "http://$IP1:8080/connect?node=n@$PRIV2&cookie=$COOKIE"; echo
curl -s -X POST "http://$IP2:8080/connect?node=n@$PRIV1&cookie=$COOKIE"; echo
sleep 3
P=$(curl -s "http://$IP1:8080/diag" | python3 -c 'import sys,json;print(json.load(sys.stdin)["dist"]["peers_connected"])' 2>/dev/null)
[ "$P" = 1 ] || { echo "FAIL: cluster did not form (peers=$P) — check dist boot name + SG dist ports"; exit 1; }
echo "  clustered (peers=1 both sides)"

echo "=== SOAK: ${DUR}s, both nodes, restart probe ==="
# restart probe (~40%): reboot node b; the driver measures reconnect (teeth). A
# network-blip probe can be added as a second --probe that toggles an SG rule on
# the dist port between the two private IPs, then restores it.
python3 "$HERE/soak.py" \
  --nodes "http://$IP1:8080,http://$IP2:8080" \
  --duration-s "$DUR" --scrape-s 15 --rps 5 --max-drift-ms 2000 \
  --probe "at=$((DUR*4/10)):aws ec2 reboot-instances --region $REGION --instance-ids $IID2" \
  --out "$HERE/soak_nitro_${TS}.jsonl"
RC=$?

echo "=== VERDICT: soak.py exit=$RC (0=all quantities bounded + probes recovered) ==="
exit $RC
