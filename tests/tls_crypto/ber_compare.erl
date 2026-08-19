-module(ber_compare).
-export([run/1]).

%% Validate the pure-Erlang decode_ber_tlv against the REAL OTP-27 asn1rt_nif,
%% byte-exact on well-formed certs and error-on-malformed (the teeth: asn1 is on
%% the cert-verification path, so a malformed cert must NOT decode to something).

run(CertPaths) ->
    WF = lists:append([wellformed(P) || P <- CertPaths]),
    Adv = adversarial(hd(CertPaths)),
    Results = [check_wf(C) || C <- WF] ++ [check_adv(A) || A <- Adv],
    Fails = [F || {fail, F} <- Results],
    [io:format("  FAIL: ~P~n", [F, 8]) || F <- Fails],
    io:format("[ber] ~p/~p checks passed~n", [length(Results) - length(Fails), length(Results)]),
    case Fails of
        [] -> io:format("BER-COMPARE: PASS (byte-exact well-formed, error-on-malformed)~n");
        _  -> io:format("BER-COMPARE: FAIL~n")
    end,
    halt(0).

wellformed(Path) ->
    {ok, DER} = file:read_file(Path),
    [{Path, DER}].

adversarial(Path) ->
    {ok, DER} = file:read_file(Path),
    [{truncated, binary:part(DER, 0, 10)},
     {garbage,   <<255, 255, 255, 255>>},
     {empty,     <<>>},
     {badlen,    <<16#30, 16#84, 255, 255, 255, 255>>},   %% length overruns
     {indefinite,<<16#30, 16#80, 2, 1, 1, 0, 0>>},        %% BER indefinite (not DER)
     {trailing,  <<DER/binary, 1, 2, 3>>}].                %% extra bytes after

check_wf({Name, DER}) ->
    Real = asn1rt_nif:decode_ber_tlv(DER),
    Mine = try my_decode(DER) catch C:E -> {my_error, C, E} end,
    case Mine =:= Real of
        true  -> {ok, Name};
        false -> {fail, {wf_mismatch, Name, {real, Real}, {mine, Mine}}}
    end.

check_adv({Name, Bin}) ->
    Real = (catch asn1rt_nif:decode_ber_tlv(Bin)),
    Mine = (catch my_decode(Bin)),
    %% For adversarial input: both must NOT return a clean {TLV,Rest} unless they
    %% AGREE. Real erroring + mine erroring = pass. Real ok + mine ok + equal = pass
    %% (e.g. `trailing` decodes the head, both return same {TLV, <<1,2,3>>}).
    RealErr = is_err(Real), MineErr = is_err(Mine),
    if
        RealErr andalso MineErr -> {ok, Name};
        (not RealErr) andalso (not MineErr) andalso (Real =:= Mine) -> {ok, Name};
        true -> {fail, {adv_divergence, Name, {real, Real}, {mine, Mine}}}
    end.

is_err({'EXIT', _}) -> true;
is_err({my_error, _, _}) -> true;
is_err(_) -> false.

%% ---------------- pure-Erlang decode_ber_tlv ----------------
my_decode(Bin) when is_binary(Bin) -> dec_one(Bin, 0).

dec_one(Bin, Pos) ->
    {TagInt, Constructed, R1, P1} = dec_tag(Bin, Pos),
    {Len, R2, P2} = dec_len(R1, P1),
    case R2 of
        <<Val:Len/binary, Rest/binary>> ->
            V = case Constructed of
                    true  -> dec_list(Val, P2);
                    false -> Val
                end,
            {{TagInt, V}, Rest};
        _ -> erlang:error({asn1, {invalid_value, P2}})
    end.

dec_list(<<>>, _Pos) -> [];
dec_list(Bin, Pos) ->
    {Tlv, Rest} = dec_one(Bin, Pos),
    [Tlv | dec_list(Rest, Pos + (byte_size(Bin) - byte_size(Rest)))].

dec_tag(<<B, Rest/binary>>, Pos) ->
    Class = B bsr 6,
    Constructed = (B band 16#20) =/= 0,
    case B band 16#1F of
        16#1F ->
            {TagNo, R2, P2} = dec_high_tag(Rest, 0, Pos + 1),
            {(Class bsl 16) bor TagNo, Constructed, R2, P2};
        TagNo ->
            {(Class bsl 16) bor TagNo, Constructed, Rest, Pos + 1}
    end;
dec_tag(<<>>, Pos) -> erlang:error({asn1, {invalid_tag, Pos}}).

dec_high_tag(<<B, Rest/binary>>, Acc, Pos) ->
    Acc1 = (Acc bsl 7) bor (B band 16#7F),
    case B band 16#80 of
        0 -> {Acc1, Rest, Pos + 1};
        _ -> dec_high_tag(Rest, Acc1, Pos + 1)
    end;
dec_high_tag(<<>>, _, Pos) -> erlang:error({asn1, {invalid_tag, Pos}}).

dec_len(<<L, Rest/binary>>, Pos) when L < 128 -> {L, Rest, Pos + 1};
dec_len(<<16#80, _/binary>>, Pos) -> erlang:error({asn1, {invalid_value, Pos}});
dec_len(<<L, Rest/binary>>, Pos) ->
    N = L band 16#7F,
    case Rest of
        <<LenBytes:N/binary, R2/binary>> -> {binary:decode_unsigned(LenBytes), R2, Pos + 1 + N};
        _ -> erlang:error({asn1, {invalid_value, Pos}})
    end;
dec_len(<<>>, Pos) -> erlang:error({asn1, {invalid_value, Pos}}).
