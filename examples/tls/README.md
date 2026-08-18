# Example: in-guest inbound TLS in a Bandit app

App-side integration for Tyn's `tyn_tls` NIF (see `docs/IN_GUEST_TLS.md`). Drop these
into a Bandit/Plug (or Phoenix) app:

- **`tyn_tls.ex`** — loader for the static `tyn_tls` NIF (`load_nif` resolves it from the
  combined beam's static NIF table).
- **`rustls_tls.ex`** — `Tyn.Transports.RustlsTLS`, a `ThousandIsland.Transport` backed by
  the NIF. Requires `bandit` (pulls `thousand_island`).
- **`application.ex`** — reference supervisor child: an HTTPS Bandit listener wired
  **config-only** (`scheme: :https` + `transport_module: Tyn.Transports.RustlsTLS`), with
  cert/key read from the boot-config env (`TYN_TLS_CERT_B64` / `TYN_TLS_KEY_B64`).

Wire-in is one extra key on an otherwise-standard HTTPS listener config — no Bandit or app
code changes. Inject the key at pack time (never bake it into a public image):

```
tyn-pack <release> --base src/otp-rootfs.cpio -o app.cpio \
  --env TYN_TLS_CERT_B64=$(base64 -w0 cert.pem) \
  --env TYN_TLS_KEY_B64=$(base64 -w0 key.pem)
```

The beam must be built with the NIF: `build-beam.sh --nif-modules "crypto tyn_tls"`.
