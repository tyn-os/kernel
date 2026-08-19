-module(verify_suite).
-export([run/1]).

%% Runs on the A' beam (MY crypto NIF). The teeth are the FALSE cases: a
%% tampered/wrong-key/malformed input that verifies TRUE is a silent MITM.
run([VecFile]) ->
    {ok, Bin} = file:read_file(VecFile),
    Vs = binary_to_term(Bin),
    Results = [check(V) || V <- Vs],
    Fails = [F || {fail, F} <- Results],
    [io:format("  FAIL: ~p~n", [F]) || F <- Fails],
    Pass = length(Vs) - length(Fails),
    io:format("[suite] ~p/~p checks passed~n", [Pass, length(Vs)]),
    %% Summarise by kind so the valid-accepts and adversarial-rejects are visible.
    ByKind = lists:foldl(fun({K, R}, Acc) ->
                                 orddict:append(K, R, Acc)
                         end, orddict:new(),
                         [{element(1, V), classify(V)} || V <- Vs]),
    [io:format("[suite] ~p: ~p~n", [K, Rs]) || {K, Rs} <- ByKind],
    case Fails of
        [] -> io:format("VERIFY-SUITE: PASS (valid accepted, ALL adversarial rejected)~n");
        _  -> io:format("VERIFY-SUITE: FAIL~n")
    end,
    halt(0).

classify({_Kind, Alg, Type, Msg, Sig, Key, Opts}) ->
    try crypto:verify(Alg, Type, Msg, Sig, Key, Opts) of
        R -> R
    catch
        _:_ -> caught
    end.

check({Kind, Alg, Type, Msg, Sig, Key, Opts}) ->
    Expected = case Kind of valid -> true; _ -> false end,
    Got = try crypto:verify(Alg, Type, Msg, Sig, Key, Opts) of
              R -> R
          catch
              _:_ -> caught  %% clean error on garbage input is acceptable (not a MITM)
          end,
    Ok = case {Expected, Got} of
             {true, true}   -> true;                 %% valid MUST verify
             {false, false} -> true;                 %% adversarial MUST reject
             {false, caught} -> true;                %% clean rejection-by-error is fine
             _ -> false                              %% valid->false, or adversarial->true (MITM!)
         end,
    case Ok of
        true  -> {ok, {Kind, Alg}};
        false -> {fail, {Kind, Alg, Type, {expected, Expected}, {got, Got}}}
    end.
