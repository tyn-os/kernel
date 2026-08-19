# A′ outbound TLS — crypto/asn1 evidence

Adversarial + differential tests for the A′ crypto NIF (`beam-build/crypto-nif`) and
the pure-Erlang asn1 shim (`src/erl/asn1rt_nif.erl`) that back OTP `:ssl` outbound.

- **`gen_vectors.erl`** — on OTP's REAL (OpenSSL) crypto, generates ground-truth
  signature vectors + adversarial variants (tampered/wrong-key/bad-msg/malformed/garbage)
  for ECDSA-P256/P384, Ed25519, RSA-PSS.
- **`verify_suite.erl`** — runs on the Tyn beam; verifies every vector through the NIF.
  Result: **22/22 — valid accepted, ALL adversarial rejected** (the false-cases are the
  teeth: a tampered/wrong sig verifying true = silent MITM).
- **`ber_compare.erl`** — differential test of the pure-Erlang `decode_ber_tlv` vs the real
  OTP-27 `asn1rt_nif`: byte-exact on well-formed certs, error-on-malformed. Decode→encode
  round-trips byte-exact. DER-strict (rejects indefinite-length BER, fails closed).

Full outbound proof: `:ssl.connect` verify_peer to a public HTTPS endpoint completed
(TLS 1.3, cert verified, HTTP 200) with all crypto/asn1 running through this code.
