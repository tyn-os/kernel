%% Standardized status endpoint for every Tyn image.
%%
%% A tiny, dependency-free HTTP/1.1 responder over gen_tcp (no Bandit, no Plug,
%% no Jason — so it works regardless of what the packaged app ships or which
%% app-start path tyn_boot took). Started by tyn_boot alongside the shells, so
%% EVERY image exposes it without the app adding a route.
%%
%% Any GET returns a JSON snapshot of BEAM health: memory, process counts,
%% scheduler/run-queue, uptime, OTP/ERTS versions. Read-only; no eval, no state.
%%
%% Robustness note: this lives in the BEAM, on a listener independent of the
%% app's own. It therefore survives an app-listener problem (a wedged Phoenix
%% endpoint) — but not a full-VM hang. A kernel-served endpoint that survives a
%% wedged BEAM is the runtime-level follow-on; see TYN_OPERABILITY.md.

-module(tyn_status).
-export([start/1, accept_loop/1, serve/1]).

start(Port) ->
    %% Match tcp_shell's proven listen options under Tyn's socket layer
    %% ({packet, line} rather than raw). serve/1 reads the request line and
    %% ignores it — we don't route, any GET gets the snapshot.
    Opts = [binary, {active, false}, {reuseaddr, true}, {packet, line}],
    case gen_tcp:listen(Port, Opts) of
        {ok, LSock} ->
            spawn(?MODULE, accept_loop, [LSock]),
            io:format("status_listening ~p~n", [Port]),
            {ok, LSock};
        {error, Reason} = Err ->
            io:format("tyn_status listen ~p failed: ~p~n", [Port, Reason]),
            Err
    end.

accept_loop(LSock) ->
    case gen_tcp:accept(LSock) of
        {ok, Sock} ->
            Pid = spawn(?MODULE, serve, [Sock]),
            ok = gen_tcp:controlling_process(Sock, Pid),
            Pid ! {go, self()},
            accept_loop(LSock);
        {error, _} ->
            ok
    end.

serve(Sock) ->
    receive {go, _} -> ok end,
    %% Drain the request line/headers (bounded); we don't route on it — any GET
    %% gets the snapshot. A short recv timeout keeps a silent client from parking
    %% the process.
    _ = gen_tcp:recv(Sock, 0, 2000),
    Body = snapshot_json(),
    Resp = ["HTTP/1.1 200 OK\r\n",
            "Content-Type: application/json\r\n",
            "Content-Length: ", integer_to_list(iolist_size(Body)), "\r\n",
            "Connection: close\r\n",
            "\r\n",
            Body],
    _ = gen_tcp:send(Sock, Resp),
    gen_tcp:close(Sock).

snapshot_json() ->
    Mem = erlang:memory(),
    Get = fun(K) -> proplists:get_value(K, Mem, 0) end,
    {Uptime, _} = erlang:statistics(wall_clock),
    Fields =
        [{"status",        estr("ok")},
         {"node",          estr(atom_to_list(node()))},
         {"otp_release",   estr(erlang:system_info(otp_release))},
         {"erts_version",  estr(erlang:system_info(version))},
         {"uptime_ms",     integer_to_list(Uptime)},
         {"process_count", integer_to_list(erlang:system_info(process_count))},
         {"process_limit", integer_to_list(erlang:system_info(process_limit))},
         {"schedulers",    integer_to_list(erlang:system_info(schedulers))},
         {"run_queue",     integer_to_list(erlang:statistics(run_queue))},
         {"memory_total",     integer_to_list(Get(total))},
         {"memory_processes", integer_to_list(Get(processes))},
         {"memory_atom",      integer_to_list(Get(atom))},
         {"memory_binary",    integer_to_list(Get(binary))},
         {"memory_ets",       integer_to_list(Get(ets))}],
    ["{", join([["\"", K, "\":", V] || {K, V} <- Fields], ","), "}\n"].

%% Quote a string value for JSON (the values here are version strings / node
%% names — no embedded quotes/backslashes in practice, but escape defensively).
estr(S) ->
    ["\"", [esc(C) || C <- lists:flatten(io_lib:format("~s", [S]))], "\""].

esc($") -> "\\\"";
esc($\\) -> "\\\\";
esc(C) -> C.

join([], _Sep) -> [];
join([X], _Sep) -> [X];
join([X | Xs], Sep) -> [X, Sep | join(Xs, Sep)].
