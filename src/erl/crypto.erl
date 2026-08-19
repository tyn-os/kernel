%% Tyn's replacement `crypto` module (Option A). Replaces OTP's crypto.beam in
%% the cpio. on_load resolves the static Rust crypto NIF (crypto_nif_init); the
%% NIF replaces the stubs below. Implements exactly the surface Phoenix/Plug use
%% (Step 3). Capability probes are pure Erlang so libraries that call
%% crypto:supports/0,1 or info_lib/0 don't crash on missing exports.
%%
%% NOTE: Tyn's crypto is new and unreviewed; do not trust it for production
%% session security until it has had outside review.
-module(crypto).

-export([strong_rand_bytes/1, hash/2, mac/4, pbkdf2_hmac/5, exor/2, hash_equals/2,
         crypto_one_time_aead/6, crypto_one_time_aead/7,
         generate_key/2, compute_key/4,
         verify/5, verify/6, ecdsa_verify/4, ed25519_verify/3, rsa_verify/6,
         supports/0, supports/1, info_lib/0, version/0]).

-on_load(on_load/0).

on_load() ->
    erlang:load_nif("crypto", 0).

%% --- NIF-backed (stubs replaced on successful load) ---
strong_rand_bytes(_N) -> erlang:nif_error(nif_not_loaded).
hash(_Type, _Data) -> erlang:nif_error(nif_not_loaded).
mac(_Type, _SubType, _Key, _Data) -> erlang:nif_error(nif_not_loaded).
pbkdf2_hmac(_Digest, _Pass, _Salt, _Iter, _Len) -> erlang:nif_error(nif_not_loaded).
exor(_A, _B) -> erlang:nif_error(nif_not_loaded).
hash_equals(_A, _B) -> erlang:nif_error(nif_not_loaded).
crypto_one_time_aead(_C, _K, _IV, _In, _AAD, _TagOrFlag) -> erlang:nif_error(nif_not_loaded).
crypto_one_time_aead(_C, _K, _IV, _In, _AAD, _Tag, _Flag) -> erlang:nif_error(nif_not_loaded).

%% A' outbound: ECDHE for OTP :ssl. Curves implemented in the NIF: x25519,
%% secp256r1, secp384r1 (OTP encodings — raw for x25519, uncompressed point for NIST).
generate_key(_Type, _Curve) -> erlang:nif_error(nif_not_loaded).
compute_key(_Type, _Others, _My, _Curve) -> erlang:nif_error(nif_not_loaded).

%% Signature verify for OTP :ssl cert-chain + CertificateVerify. Digest is
%% computed here (crypto:hash); the NIF does pure verification and MUST return
%% false on any invalid/tampered/malformed input (a false-positive = silent MITM).
ecdsa_verify(_Curve, _Point, _Digest, _Sig) -> erlang:nif_error(nif_not_loaded).
ed25519_verify(_Pub, _Msg, _Sig) -> erlang:nif_error(nif_not_loaded).
rsa_verify(_Pad, _Type, _E, _N, _Digest, _Sig) -> erlang:nif_error(nif_not_loaded).

verify(Alg, Type, Msg, Sig, Key) -> verify(Alg, Type, Msg, Sig, Key, []).

verify(ecdsa, Type, Msg, Sig, [Point, Curve], _Opts) ->
    ecdsa_verify(curve_atom(Curve), to_bin(Point), do_digest(Type, Msg), to_bin(Sig));
verify(eddsa, _Type, Msg, Sig, [Pub, ed25519], _Opts) ->
    ed25519_verify(to_bin(Pub), raw_msg(Msg), to_bin(Sig));
verify(rsa, Type, Msg, Sig, [E, N], Opts) ->
    Pad = proplists:get_value(rsa_padding, Opts, rsa_pkcs1_padding),
    rsa_verify(Pad, Type, to_bin(E), to_bin(N), do_digest(Type, Msg), to_bin(Sig));
verify(_, _, _, _, _, _) -> false.

%% Digest for ECDSA/RSA (Ed25519 takes the full message). {digest,D} is pre-hashed.
do_digest(_Type, {digest, D}) -> D;
do_digest(Type, Msg) -> hash(Type, Msg).
raw_msg({digest, D}) -> D;
raw_msg(Msg) -> Msg.

%% OTP passes RSA E/N as integers, EC point/sig as binaries — normalise to binary.
to_bin(X) when is_integer(X) -> binary:encode_unsigned(X);
to_bin(X) when is_binary(X) -> X;
to_bin(X) when is_list(X) -> iolist_to_binary(X).

%% Named curve may arrive as an atom or an OID tuple; map to the NIF's atom.
curve_atom(secp256r1) -> secp256r1;
curve_atom(secp384r1) -> secp384r1;
curve_atom(prime256v1) -> secp256r1;
curve_atom({1,2,840,10045,3,1,7}) -> secp256r1;
curve_atom({1,3,132,0,34}) -> secp384r1;
curve_atom(Other) -> Other.

%% --- pure-Erlang capability probes ---
%% Report the asymmetric surface the NIF now backs so :ssl can build a
%% signature-alg set (Wall 1) and pick an ECDHE group. `verify` (ecdsa/rsa/eddsa)
%% is the next NIF to land (Step 2) — until then :ssl advances to the verify wall.
supports() ->
    [{hashs, [sha, sha224, sha256, sha384, sha512]},
     {ciphers, [aes_gcm, aes_128_gcm, aes_256_gcm, chacha20_poly1305]},
     {macs, [hmac]},
     {public_keys, [ecdsa, rsa, ecdh, eddsa]},
     {curves, [x25519, secp256r1, secp384r1]},
     {rsa_opts, [rsa_pkcs1_pss_padding, rsa_pkcs1_padding]}].

supports(hashs)       -> [sha, sha224, sha256, sha384, sha512];
supports(macs)        -> [hmac];
%% `aes_gcm` (generic) is what tls_record:sufficient_crypto_support/1 checks for
%% TLS 1.3 (TLS_AES_*_GCM_*), alongside the width-specific atoms.
supports(ciphers)     -> [aes_gcm, aes_128_gcm, aes_256_gcm, chacha20_poly1305];
supports(public_keys) -> [ecdsa, rsa, ecdh, eddsa];
supports(curves)      -> [x25519, secp256r1, secp384r1];
supports(rsa_opts)    -> [rsa_pkcs1_pss_padding, rsa_pkcs1_padding];
supports(_)           -> [].

info_lib() -> [{<<"Tyn RustCrypto">>, 1, <<"tyn-crypto-nif 0.1 (unreviewed)">>}].

version() -> "tyn-crypto-nif-0.1".
