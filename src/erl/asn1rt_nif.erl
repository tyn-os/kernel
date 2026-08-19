%% Tyn's pure-Erlang replacement for asn1's decode_ber_tlv NIF (Tyn has no asn1
%% NIF). Byte-exact with OTP-27 asn1rt_nif on DER (validated), DER-strict
%% (rejects indefinite-length BER, invalid for certs — fails closed). Cert decode
%% is on the verification path, so malformed input MUST error, never mis-decode.
-module(asn1rt_nif).
-export([decode_ber_tlv/1, encode_ber_tlv/1, encode_per_complete/1]).

encode_per_complete(_) -> erlang:error({asn1, per_unsupported}).

%% Inverse of decode_ber_tlv: {TagInt, Value} -> DER binary. Used by OTP-PUB-KEY
%% during DECODE to re-encode sub-structures (e.g. DN AttributeTypeAndValue).
encode_ber_tlv({TagInt, Value}) ->
    Class = TagInt bsr 16,
    TagNo = TagInt band 16#FFFF,
    Constructed = is_list(Value),
    ValBin = case Value of
                 L when is_list(L) -> iolist_to_binary([encode_ber_tlv(T) || T <- L]);
                 B when is_binary(B) -> B
             end,
    Tag = enc_tag(Class, Constructed, TagNo),
    Len = enc_len(byte_size(ValBin)),
    <<Tag/binary, Len/binary, ValBin/binary>>.

enc_tag(Class, Constructed, TagNo) when TagNo < 31 ->
    C = case Constructed of true -> 16#20; false -> 0 end,
    <<((Class bsl 6) bor C bor TagNo)>>;
enc_tag(Class, Constructed, TagNo) ->
    C = case Constructed of true -> 16#20; false -> 0 end,
    <<((Class bsl 6) bor C bor 16#1F), (enc_high_tag(TagNo))/binary>>.

enc_high_tag(N) when N < 128 -> <<N>>;
enc_high_tag(N) -> enc_high_tag(N bsr 7, [N band 16#7F], []).
enc_high_tag(0, [Last | More], Acc) -> list_to_binary([(16#80 bor X) || X <- lists:reverse(More)] ++ Acc ++ [Last]);
enc_high_tag(N, Parts, Acc) -> enc_high_tag(N bsr 7, [N band 16#7F | Parts], Acc).

enc_len(L) when L < 128 -> <<L>>;
enc_len(L) ->
    Bytes = binary:encode_unsigned(L),
    <<(16#80 bor byte_size(Bytes)), Bytes/binary>>.

decode_ber_tlv(Bin) when is_binary(Bin) -> dec_one(Bin, 0).

dec_one(Bin, Pos) ->
    {TagInt, Constructed, R1, P1} = dec_tag(Bin, Pos),
    {Len, R2, P2} = dec_len(R1, P1),
    case R2 of
        <<Val:Len/binary, Rest/binary>> ->
            V = case Constructed of true -> dec_list(Val, P2); false -> Val end,
            {{TagInt, V}, Rest};
        _ -> erlang:error({asn1, {invalid_value, P2}})
    end.

dec_list(<<>>, _Pos) -> [];
dec_list(Bin, Pos) ->
    {Tlv, Rest} = dec_one(Bin, Pos),
    [Tlv | dec_list(Rest, Pos + (byte_size(Bin) - byte_size(Rest)))].

dec_tag(<<B, Rest/binary>>, Pos) ->
    Class = B bsr 6, Constructed = (B band 16#20) =/= 0,
    case B band 16#1F of
        16#1F -> {TagNo, R2, P2} = dec_high_tag(Rest, 0, Pos + 1),
                 {(Class bsl 16) bor TagNo, Constructed, R2, P2};
        TagNo -> {(Class bsl 16) bor TagNo, Constructed, Rest, Pos + 1}
    end;
dec_tag(<<>>, Pos) -> erlang:error({asn1, {invalid_tag, Pos}}).

dec_high_tag(<<B, Rest/binary>>, Acc, Pos) ->
    Acc1 = (Acc bsl 7) bor (B band 16#7F),
    case B band 16#80 of 0 -> {Acc1, Rest, Pos + 1}; _ -> dec_high_tag(Rest, Acc1, Pos + 1) end;
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
