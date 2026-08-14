# Tyn public demo AMI

A one-click AWS image that boots **Tyn** — a bare-metal x86-64 Rust microkernel
that runs unmodified BEAM/OTP at ring 0, no Linux — serving a real Phoenix app.

## Provenance (the AMI *is* the commit)

| | |
|---|---|
| **AMI** | `ami-0c13cb4a868a6e441` (`tyn-20260814-002235`), us-east-1 |
| **Snapshot** | `snap-0e8d5db9a0313c957` |
| **Built from** | kernel commit **`5d6ebb5`** (BUG-1 SMP residual closed), production beam `a9048ee0` |
| **Provenance** | deploy-ami gate, clean tree, **no dirty override** — the image traces to the commit |
| **Status** | validated + **published (public)** in us-east-1 |

## What it demonstrates

A stock **Phoenix 1.7.14 + LiveView 1.0** app (`phx.new --no-ecto`) with a LiveView
counter — i.e. a real stateful web app on a Rust unikernel:

- **Static assets** served (`/assets/app.css`, `/assets/app.js`) — byte-exact vs the
  release, via the kernel's `sendfile(2)` path.
- **LiveView over WebSocket** — `/counter` mounts a LiveView; the `inc` click event
  round-trips and the server pushes back the incremented count (validated 0→1→2→3).
- Runs on **c5.large (2 vCPU = SMP)**, so it exercises the BUG-1 IPI-IST fix in production.

## Validation (behavior, on real Nitro)

- `GET /` → 200, Phoenix landing (CSRF + LiveView markers).
- `GET /counter` → 200, LiveView counter (`phx-click`, `data-phx-main`).
- `GET /assets/app.{css,js}` → 200, correct content-types; **app.css byte-exact** vs release.
- **Interactive**: joined the LiveView channel over `/live/websocket`, sent `inc`, observed
  the diff `{"3":{"0":"1"}}` → `2` → `3` — the counter increments live.

## How to launch

```sh
aws ec2 run-instances --region us-east-1 \
  --image-id ami-0c13cb4a868a6e441 --instance-type c5.large \
  --security-group-ids <sg-with-tcp-8080-open> --associate-public-ip-address
# then browse to:  http://<public-ip>:8080/   and   http://<public-ip>:8080/counter
```

The app listens on **:8080**. Open TCP 8080 in the security group.

## Honest caveats

- **Boot reliability — measured 20/20** booted-and-served in a batch launch of *this* AMI
  (all came up within ~90 s), so no failures in that sample. Tyn's boot has historically shown
  rare (~3%) non-starts, and 20/20 can't rule that out below the sample size — so still wrap the
  launch in a boot-verify-retry (poll `http://<ip>:8080/` for ~90 s; if no serve, terminate and
  relaunch). Cheap insurance; tracked for a post-Phase-2 boot-reliability pass.
- **TLS** is via a sidecar (Path B), **not** included in this single-AMI demo (in-guest TLS
  needs the crypto-NIF/clock work). The demo serves plain HTTP :8080.
- **No database** — the counter is per-LiveView state; this demo deliberately avoids the DB
  sidecar to stay reliable as a single public artifact.
- `SECRET_KEY_BASE` is baked into the public image (demo only — do not reuse for anything
  handling real sessions/data).

## Publish status

**Published (public) in us-east-1** (2026-08-14) — launch permission `all`. `tyn-build-role` was
granted the AMI-lifecycle perms and the account's AMI block-public-access was disabled for the
region to allow it.

**Cleanup done** (2026-08-14): the stale `ami-09619e2d139f2a57d` (`tyn-20260719`, pre-feature-wave)
was deregistered and its backing `snap-02439cf571e0c479c` deleted, so this AMI is now the single
public/official artifact. Two unrelated orphaned VM-import snapshots (`snap-023c8d6c086aef71a`,
`snap-0fd26d7737a17164e`, backing no AMI) were cleared at the same time. Verified non-destructive
first: no instances or launch templates referenced the old AMI, the new AMI was available+public,
and each snapshot was unreferenced by any remaining AMI before deletion. (Still optional: the older
*private* `ami-0286718a83e0f8bfc` / `ami-0a5f9bdbe6527dd7c` + their snapshots. Keep
`ami-03e6489b48f9aea90` — the build-host base.)
