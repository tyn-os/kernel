%% FS_BASE-preemption detector — Erlang loader for the static NIF built into
%% beam.smp (beam-build/nifs/fsbase_probe.c, linked via --enable-static-nifs).
%% See directions/SCALAR_STATE_ELIM.md.
%%
%% probe(Outer, Spin) keeps the thread's own FS_BASE (musl: %fs:0) live across
%% `Outer` call-free spins (each `Spin` GP-counter iterations, long enough to be
%% timer-preempted) and returns how many spans came back with a changed FS_BASE.
%% Non-zero => a preemptive context switch resumed the thread without restoring
%% FS_BASE (it read another thread's TLS base). This is a true FS_BASE detector,
%% the scalar sibling of xmm_probe.
-module(fsbase_probe).
-export([probe/2, gp_probe/2, rflags_probe/2, xmm_probe/2, xmm_poison/2, redzone_probe/2, available/0]).
-on_load(init/0).

init() ->
    case erlang:load_nif("fsbase_probe", 0) of
        ok -> ok;
        {error, Reason} ->
            io:format("fsbase_probe: load_nif FAILED: ~p~n", [Reason]),
            ok
    end.

probe(_Outer, _Spin) -> erlang:nif_error(nif_not_loaded).
gp_probe(_Outer, _Spin) -> erlang:nif_error(nif_not_loaded).
rflags_probe(_Outer, _Spin) -> erlang:nif_error(nif_not_loaded).
xmm_probe(_Outer, _Spin) -> erlang:nif_error(nif_not_loaded).
xmm_poison(_Outer, _Spin) -> erlang:nif_error(nif_not_loaded).
redzone_probe(_Outer, _Spin) -> erlang:nif_error(nif_not_loaded).

available() ->
    try probe(1, 1) of
        N when is_integer(N) -> true
    catch
        _:_ -> false
    end.
