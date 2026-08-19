-module(gen_vectors).
-export([main/1]).

%% Run on the host's OTP-25 REAL crypto (OpenSSL) to produce ground-truth
%% signature vectors in exactly the formats crypto:verify expects, plus the
%% adversarial variants. The A' NIF must accept `valid` and reject all others.
main([OutFile]) ->
    Msg = <<"A-prime verify adversarial suite ground truth 2026">>,
    Vs = ecdsa(Msg, secp256r1, sha256)
         ++ ecdsa(Msg, secp384r1, sha384)
         ++ ed25519(Msg)
         ++ rsa_pss(Msg, sha256),
    ok = file:write_file(OutFile, term_to_binary(Vs)),
    io:format("wrote ~p vectors to ~s~n", [length(Vs), OutFile]),
    halt(0).

flip(<<B, R/binary>>) -> <<(B bxor 1), R/binary>>.
trunc2(B) -> binary:part(B, 0, max(0, byte_size(B) - 2)).

ecdsa(Msg, Curve, Hash) ->
    {Pub, Priv} = crypto:generate_key(ecdsa, Curve),
    {WrongPub, _} = crypto:generate_key(ecdsa, Curve),
    Sig = crypto:sign(ecdsa, Hash, Msg, [Priv, Curve]),
    K = [Pub, Curve],
    [{valid,     ecdsa, Hash, Msg, Sig, K, []},
     {tampered,  ecdsa, Hash, Msg, flip(Sig), K, []},
     {wrongkey,  ecdsa, Hash, Msg, Sig, [WrongPub, Curve], []},
     {badmsg,    ecdsa, Hash, <<Msg/binary, 0>>, Sig, K, []},
     {malformed, ecdsa, Hash, Msg, trunc2(Sig), K, []},
     {garbage,   ecdsa, Hash, Msg, <<1, 2, 3, 4, 5>>, K, []}].

ed25519(Msg) ->
    {Pub, Priv} = crypto:generate_key(eddsa, ed25519),
    {WrongPub, _} = crypto:generate_key(eddsa, ed25519),
    Sig = crypto:sign(eddsa, none, Msg, [Priv, ed25519]),
    K = [Pub, ed25519],
    [{valid,     eddsa, none, Msg, Sig, K, []},
     {tampered,  eddsa, none, Msg, flip(Sig), K, []},
     {wrongkey,  eddsa, none, Msg, Sig, [WrongPub, ed25519], []},
     {badmsg,    eddsa, none, <<Msg/binary, 0>>, Sig, K, []},
     {malformed, eddsa, none, Msg, trunc2(Sig), K, []}].

rsa_pss(Msg, Hash) ->
    {Pub, Priv} = crypto:generate_key(rsa, {2048, 65537}),
    {WrongPub, _} = crypto:generate_key(rsa, {2048, 65537}),
    Opts = [{rsa_padding, rsa_pkcs1_pss_padding}, {rsa_pss_saltlen, -1}, {rsa_mgf1_md, Hash}],
    Sig = crypto:sign(rsa, Hash, Msg, Priv, Opts),
    VOpts = [{rsa_padding, rsa_pkcs1_pss_padding}],
    [{valid,     rsa, Hash, Msg, Sig, Pub, VOpts},
     {tampered,  rsa, Hash, Msg, flip(Sig), Pub, VOpts},
     {wrongkey,  rsa, Hash, Msg, Sig, WrongPub, VOpts},
     {badmsg,    rsa, Hash, <<Msg/binary, 0>>, Sig, Pub, VOpts},
     {malformed, rsa, Hash, Msg, trunc2(Sig), Pub, VOpts}].
