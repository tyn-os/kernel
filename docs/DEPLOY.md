# Deploying Tyn

Three paths, shortest first: run the public demo on AWS, run locally under QEMU/KVM, or
package and deploy **your own** Phoenix/Elixir app.

---

## 1. Try the public demo on AWS (no build)

The public AMI carries a stock `mix phx.new` app — landing page, a **LiveView counter**, and
static assets served through the kernel's `sendfile(2)` — so it demonstrates the capability
claims directly.

**AMI:** `ami-0c13cb4a868a6e441` (us-east-1)

```bash
# One-time: a security group with port 8080 open
SG_ID=$(aws ec2 create-security-group --group-name tyn-demo \
    --description "Tyn demo - HTTP on 8080" --region us-east-1 \
    --query 'GroupId' --output text)
aws ec2 authorize-security-group-ingress --group-id $SG_ID \
    --protocol tcp --port 8080 --cidr 0.0.0.0/0 --region us-east-1

# Launch
INSTANCE_ID=$(aws ec2 run-instances --image-id ami-0c13cb4a868a6e441 \
    --instance-type c5.large --security-group-ids $SG_ID --region us-east-1 \
    --query 'Instances[0].InstanceId' --output text)
aws ec2 wait instance-running --region us-east-1 --instance-ids $INSTANCE_ID
IP=$(aws ec2 describe-instances --instance-ids $INSTANCE_ID --region us-east-1 \
    --query 'Reservations[0].Instances[0].PublicIpAddress' --output text)
echo "Tyn is at http://$IP:8080"
```

Wait ~5 s for boot (Tyn boots in ~5 s once the instance is running; the rest is EC2 provisioning), then:

```bash
curl http://$IP:8080/                         # Phoenix landing page (HTML)
curl -s http://$IP:8080/assets/app.js | wc -c  # nonzero — a static asset via kernel sendfile(2)
# then open http://$IP:8080/counter in a browser: the LiveView counter increments live
```

**Terminate when done** (instances accrue hourly charges):

```bash
aws ec2 terminate-instances --region us-east-1 --instance-ids $INSTANCE_ID
```

The security group persists for future launches (no recurring cost). Any Nitro instance type
works (c5, m5, t3, r5, …) — the ENA driver auto-detects the NIC.

### Serial console (interactive BEAM shell, no open port)

A live Erlang/Elixir eval shell over the EC2 Serial Console — IAM-authenticated:

```bash
aws ec2 modify-serial-console-access --serial-console-access-enabled --region us-east-1
SSH_KEY=~/.ssh/id_ed25519   # adjust to your key
aws ec2-instance-connect send-serial-console-ssh-public-key \
    --instance-id $INSTANCE_ID --serial-port 0 \
    --ssh-public-key file://${SSH_KEY}.pub --region us-east-1 && \
ssh -i $SSH_KEY -o StrictHostKeyChecking=no \
    $INSTANCE_ID.port0@serial-console.ec2-instance-connect.us-east-1.aws
```

```
>> erlang:system_info(emu_flavor).
jit
>> erlang:memory().
[{total,18124768}, ...]
```

`Enter` then `~.` to disconnect.

---

## 2. Run locally under QEMU/KVM

**Prerequisites:** Rust nightly with `rust-src`, and QEMU with KVM. A prebuilt `beam.smp` +
OTP/Elixir rootfs are committed, so the kernel builds out of the box (to rebuild them, see
[`BUILDING_ERTS.md`](BUILDING_ERTS.md)).

```bash
cargo build --release --target x86_64-tyn.json \
  -Zbuild-std=core,alloc,compiler_builtins -Zbuild-std-features=compiler-builtins-mem

qemu-system-x86_64 -kernel target/x86_64-tyn/release/tyn-kernel \
  -m 2560M -machine q35 -cpu host -enable-kvm -smp 8 \
  -nographic -no-reboot -serial mon:stdio \
  -device virtio-net-pci,netdev=net0,disable-legacy=on,disable-modern=off \
  -netdev user,id=net0,hostfwd=tcp::5555-:8080,hostfwd=tcp::5567-:9090
```

> **Use KVM (`-enable-kvm`), not TCG.** Software emulation (`-accel tcg`) deterministically
> `#PF`s at boot on some images; real hardware / KVM is unaffected. See the README Limitations.

Once it prints `phoenix_listening`, from another terminal: `curl http://localhost:5555/`, and
`nc localhost 5567` for the eval shell.

---

## 3. Deploy your own Phoenix/Elixir app

One command turns a Mix release into a running Tyn instance on AWS:

```bash
./tyn deploy my_app/ --env SECRET_KEY_BASE=$(openssl rand -hex 40) --env PHX_SERVER=true
```

`tyn deploy` takes a Mix release dir (or a project root with `mix.exs` — it runs `mix release`
for you), packs it, builds a bootable image, imports it as an AMI (polling the slow snapshot
import for you), launches an instance, waits until the app answers, and prints the public IP,
the port, and the serial-console hint. `tyn build` stops at the registered AMI id.

- **`--env KEY=VALUE` / `--env-file`** land config in the image's boot config (feeding
  `runtime.exs`), so `DATABASE_URL` and friends reach the app without baking them into source.
- **Hash-reuse** — the build is fingerprinted from its inputs (release contents + env + kernel);
  an unchanged redeploy reuses the existing AMI and skips the ~10-minute snapshot import.
- **IAM** — `./tyn iam-policy` prints the exact permission set (a copy-pasteable policy JSON).
  The tool preflights your credentials and fails early, naming any missing permission.
- **No kernel rebuild** — the committed kernel + base rootfs are reused; only your app is packed.

`tyn deploy` is a **v1 bash wrapper** over the pieces below (`tyn-pack` + `build-disk.sh` + the
AWS CLI) — deliberately thin and easy to change, not a packaged CLI. Region defaults to
`$AWS_REGION` (or `us-east-1`) and is overridable with `--region`; the S3 import bucket is
derived from your account id.

### Prerequisites (verified on a clean Ubuntu 24.04 box)

- **Elixir 1.15–1.18 on OTP ≤ 27.** The distro packages are too old (Ubuntu 24.04 ships Elixir
  1.14 / OTP 25, which can't compile a modern Phoenix app — `hpax` needs Elixir ≥ 1.15). Install a
  pinned toolchain, e.g. with [asdf](https://asdf-vm.com):

  ```bash
  sudo apt-get install -y build-essential autoconf m4 libncurses-dev libssl-dev unzip
  git clone https://github.com/asdf-vm/asdf.git ~/.asdf --branch v0.14.1
  . ~/.asdf/asdf.sh
  asdf plugin add erlang && asdf plugin add elixir
  asdf install erlang 27.3.4.2 && asdf global erlang 27.3.4.2      # matches Tyn's base OTP
  asdf install elixir 1.18.3-otp-27 && asdf global elixir 1.18.3-otp-27
  ```

  These exact versions are pinned in the repo's committed [`.tool-versions`](../.tool-versions), so
  from the repo root a bare `asdf install` (after the two `apt`/`asdf plugin add` steps above)
  installs the right toolchain with no version arguments — the machine-readable complement to this
  prose. (The versions must not exceed Tyn's base OTP 27.3.4.2 / ERTS 15.2.7.1; `tyn-pack` rejects a
  release that does.)

- **AWS CLI v2** (for `deploy-ami.sh`) + credentials (`aws configure`, or an instance role):

  ```bash
  curl "https://awscli.amazonaws.com/awscli-exe-linux-x86_64.zip" -o awscliv2.zip
  unzip awscliv2.zip && sudo ./aws/install
  ```

- **Disk-image tools** (`build-disk.sh`): `parted grub-pc-bin e2fsprogs` (present on stock Ubuntu
  server). `build-disk.sh` uses `sudo` internally — run it as your normal user, **not** under `sudo`.

### Build a release (starting from scratch)

```bash
mix archive.install hex phx_new 1.7.14 --force
mix phx.new my_app --no-ecto        # or your existing app
cd my_app && mix deps.get
MIX_ENV=prod mix assets.deploy
MIX_ENV=prod mix release            # OTP <= 27; older is fine, newer is rejected
```

### Under the hood — the manual pieces

`tyn deploy` runs these for you; reach for them directly for a local QEMU boot or a custom flow.

**Pack** a Mix release into a Tyn cpio (config via `--env`):

```bash
./tyn-pack _build/prod/rel/my_app -o my_app.cpio --app my_app --port 8080 \
    --env SECRET_KEY_BASE=$(openssl rand -hex 40) \
    --env PHX_SERVER=true --env PORT=8080
```

`tyn-pack` emits the release layout, records code paths, ships `runtime.exs`, and sets boot-time
env vars. Core OTP/Elixir apps come from Tyn's base image; everything else (Phoenix, Bandit,
your app, its deps) comes from your release — **unmodified**. `tyn_boot` evaluates `runtime.exs`
at boot and deep-merges it, so a stock app's config Just Works.

**Build the image**, then boot locally or take the AWS path (`tyn deploy` automates the AWS one):

```bash
CPIO=my_app.cpio ./build-disk.sh          # -> a bootable raw disk image
# local: qemu ... -drive file=<image>,format=raw,if=virtio ...
# AWS (what `tyn deploy` automates): S3 -> import-snapshot -> register AMI -> launch (deploy-ami.sh)
```

### Three things every real deployment needs

- **TLS: in-guest works, or terminate at a load balancer.** In-guest TLS (inbound *and* outbound)
  now works — HTTPS both terminates and originates inside the guest via the `tyn_tls`/RustCrypto
  NIFs (see the README and [`IN_GUEST_TLS.md`](IN_GUEST_TLS.md)). But that crypto/TLS surface is
  **unreviewed**, so for production you may still prefer to terminate HTTPS at an ALB/NLB and serve
  plain HTTP in-guest (`scheme: "http"`) — keeping crypto outside the trust boundary until the NIF
  is review-cleared.
- **Reach a TLS-required database through a sidecar** *(the outbound analogue of the LB above)*. Tyn
  speaks **plaintext** Postgres fine (Ecto/Postgrex confirmed), and in-guest outbound TLS now works
  (`Postgrex ssl: true` via the RustCrypto NIF) — but that surface is **unreviewed**, so to keep
  crypto out of the guest until it's review-cleared you can terminate DB TLS in a small proxy beside the instance — **stunnel**, or **pgbouncer** with a
  TLS upstream — in the same VPC/trust boundary. The app connects plaintext to the proxy; the proxy
  originates TLS to the database and validates its certificate with a real clock:

  ```elixir
  # runtime.exs — point Ecto at the TLS-terminating sidecar, not the DB directly
  config :my_app, MyApp.Repo,
    hostname: "10.0.x.y", port: 6432,     # the stunnel/pgbouncer listener (in-VPC)
    ssl: false                            # plaintext hop to the sidecar; sidecar does TLS upstream
  ```

  **Hard requirement — the plaintext hop must never leave the VPC/trust boundary.** The app→proxy leg
  is unencrypted Postgres; it is safe *only* because it stays inside the private network (co-located
  sidecar, or a proxy on a private subnet with a security group that admits only the app's subnet).
  Never point `ssl: false` at a proxy across the public internet. This is the same trust model as
  inbound "terminate TLS at the LB": ciphertext on the public leg, plaintext only inside the boundary.

  **stunnel config** (client mode, Postgres-aware — Postgres TLS is an `SSLRequest` negotiation, *not*
  raw TLS, so `protocol = pgsql` is required; a plain `socat OPENSSL`/raw-TLS proxy will not handshake):

  ```ini
  [pg-sidecar]
  client = yes
  accept = 0.0.0.0:6432                    ; plaintext, from the in-VPC app only (lock down via SG)
  connect = your-db-host:5432              ; the real TLS-required Postgres
  protocol = pgsql
  ```

  **⚠️ SCRAM + a *dumb* TLS wrapper (stunnel/socat) do not compose — use pgbouncer.** When the
  proxy→DB leg is TLS, Postgres offers `SCRAM-SHA-256-PLUS` (channel binding). A transparent TLS
  wrapper (stunnel `protocol=pgsql`, socat) forwards that `-PLUS` offer *through* to the app, whose own
  leg is plaintext — which looks exactly like a TLS-stripping downgrade attack. `libpq`/`psql` refuse
  outright ("server offered SCRAM-SHA-256-PLUS authentication over a non-SSL connection", even with
  `channel_binding=disable`); Postgrex's connection is dropped. **Verified on Nitro:** a Tyn instance
  reached an stunnel sidecar in-VPC and stunnel originated TLS to the DB (SCRAM bytes flowed both
  ways), but the auth handshake did not complete for this reason — a proxy/SCRAM issue, *not* a Tyn
  limitation (Tyn's symmetric crypto shim covers SCRAM; plaintext-direct `[[42]]` is confirmed).

  The robust sidecar for SCRAM-authenticated Postgres is a **protocol-aware** proxy — **pgbouncer**
  with `server_tls_sslmode=require`: pgbouncer speaks the Postgres wire protocol on both sides, auths
  the app on its own (plaintext) leg and re-auths to the DB itself, so the DB's `-PLUS` offer never
  reaches the app. (Alternatives: an `md5`-auth DB user, which sidesteps SCRAM entirely; or a client
  that both sends `channel_binding=disable` *and* is offered plain `SCRAM-SHA-256`.)

  > ✅ **Validated on Nitro.** A Tyn app (no in-guest TLS) connected through a pgbouncer sidecar
  > (`auth_type=any` on the plaintext leg; `server_tls_sslmode=require` to the DB) to a SCRAM-required
  > Postgres endpoint, ran `select 42` and got **42**, with the DB reporting the backend connection as
  > **`ssl=true, TLSv1.3`** (`pg_stat_ssl`). pgbouncer performs the DB-side SCRAM itself, so the
  > `-PLUS` offer never reaches the app. stunnel remains *confirmed broken* for this (SCRAM passthrough,
  > above). **Use pgbouncer, not stunnel, for TLS-required Postgres from Tyn.**
  >
  > Minimal pgbouncer.ini that worked:
  > ```ini
  > [databases]
  > mydb = host=DB_HOST port=5432 dbname=mydb user=APP_USER password=APP_PASSWORD
  > [pgbouncer]
  > listen_addr = 0.0.0.0            ; in-VPC only — lock down with a security group
  > listen_port = 6432
  > auth_type = any                  ; client leg is plaintext+trusted (in-VPC); pgbouncer auths upstream
  > server_tls_sslmode = require     ; originate TLS to the DB
  > pool_mode = session
  > ```
  > The app then uses `hostname: PGBOUNCER_HOST, port: 6432, ssl: false`.

  **RDS Proxy does not remove this requirement** — it still demands TLS from *its* clients, so the
  sidecar (the actual TLS originator) is what satisfies RDS, not RDS Proxy. This keeps all crypto out
  of the guest and sidesteps the epoch-clock cert-date problem entirely. (In-guest TLS is a separate,
  larger investment — a real crypto NIF *and* a real wall clock; see `docs/CAPABILITY_MAP.md` Probe 5b
  and `docs/WALL_CLOCK.md`.)
- **Set `check_origin` for LiveView.** On a bare IP or any host that doesn't match the endpoint's
  configured URL host, Phoenix returns `403` on the LiveView WebSocket. In `runtime.exs`:

  ```elixir
  config :my_app, MyAppWeb.Endpoint,
    check_origin: ["//myapp.example.com"]   # your real host(s)
    # check_origin: false   # ONLY for a throwaway IP demo — it disables CSWSH protection
  ```

See the README **Limitations** for the full list — RTC-seeded second-resolution wall clock
(real UTC, drifts, not epoch), in-memory-only writable tmpfs (`/tmp` + `/dev/shm`, 4 MiB cap,
volatile), IPv4-only, no in-guest TLS (the two sidecar notes above), and KVM/Nitro-only (not
QEMU-TCG).
