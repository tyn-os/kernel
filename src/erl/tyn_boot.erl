%% Generic app launcher for Tyn (Track 1, Phase 1c).
%%
%% The kernel's -eval is now the fixed, app-agnostic `tyn_boot:start()`. What
%% actually boots is decided by `boot.config` *inside the cpio* — so one kernel
%% binary boots whatever app it's handed. This is the whole point of Track 1:
%% deploy a different app by swapping the cpio, not by rebuilding the kernel.
%%
%% boot.config is a file of Erlang terms at the cpio root:
%%
%%   {apps, [telemetry, jason]}.        %% ensure_all_started, in order (optional)
%%   {plug, bench_plug}.                %% App A: hand this Plug to Bandit
%%   {port, 8080}.                      %% listener port (optional, default 8080)
%%
%% or, for an app that isn't a Bandit Plug (App B — a plain gen_tcp responder):
%%
%%   {apps, []}.
%%   {start_mfa, {appb_responder, start, [8080]}}.   %% apply(M,F,A) to start it
%%   {port, 8080}.
%%
%% Order of operations (matters):
%%   1. Start the serial + TCP eval shells FIRST, so a failing app still leaves
%%      a debuggable instance (a deploy tool whose failure mode is "unreachable
%%      brick" is unusable).
%%   2. Read boot.config with erl_prim_loader:get_file/1 — the *proven* loader
%%      path (it's how every .beam already loads here); not file:consult/1.
%%   3. Parse with erl_scan + erl_parse (both already proven by the eval shell).
%%   4/5. ensure_all_started each app, then start the listener.
%%   6. Print `tyn_boot: started <App> on port <Port>` and keep printing the
%%      existing `phoenix_listening` marker (deploy/AMI/sweep scripts grep it).
%%
%% Failure: print `tyn_boot: FAILED <reason>` and DO NOT halt — the shells stay
%% up so the instance is debuggable. Absent boot.config (the embedded-cpio
%% fallback from 1b, which ships no boot.config) falls back to the demo defaults
%% and prints `tyn_boot: no boot.config, using defaults`, so that path is
%% unchanged.

-module(tyn_boot).
-export([start/0]).

start() ->
    %% 1. Shells first — reachable even if the app fails to start. The
    %%    "serial_shell ready" quiet-mode marker is deliberately withheld until
    %%    the very end (on success) so kernel logs stay visible through app
    %%    startup and, on failure, stay visible for debugging.
    start_shells(),
    %% 1b. Apply the release's sys.config (app environment) if the cpio carries
    %%    one — a Mix release keeps its compile-time config there, and e.g. a
    %%    Phoenix Endpoint needs its adapter/port/secret_key_base from it.
    apply_sys_config(),
    %% 1c. Configure the pure-Erlang DNS resolver (inet_res) from the DHCP
    %%    nameservers BEFORE any app can resolve a name — otherwise ERTS's
    %%    default inet_gethost port program is tried (Tyn can't spawn it) and
    %%    hangs. Must precede ensure_all_started.
    configure_dns(),
    %% 2/3/4/5. Config-driven app + listener, guarded so a failure can't take
    %%    down the boot process (and with it, cleanliness of the shells).
    try
        apply_config(load_config()),
        %% Success: final quiet-mode marker (see src/syscall.rs — printing
        %% "serial_shell ready" flips the serial console quiet).
        io:format("serial_shell ready~n")
    catch
        Class:Reason:Stk ->
            io:format("tyn_boot: FAILED ~p:~p~n  stack: ~p~n",
                      [Class, Reason, lists:sublist(Stk, 5)])
            %% No halt, no quiet marker: shells remain up and verbose.
    end,
    %% Park the boot process forever; Bandit and the shells run in their own
    %% processes. (-eval completing would not stop the node, but parking keeps
    %% this process around as an obvious anchor.)
    receive _ -> ok after infinity -> ok end.

start_shells() ->
    catch tcp_shell:start(9090),
    catch serial_shell:start(),
    %% Standardized, app-independent status endpoint (BEAM health as JSON) on a
    %% dedicated port — every image exposes it, no app route needed. `catch` so a
    %% status-listener failure can never take down the shells or the boot process.
    catch tyn_status:start(9091),
    io:format("shell_listening 9090~n").

%% Configure ERTS's resolver to use the kernel-synthesized nameservers via the
%% pure-Erlang inet_res (gen_udp), never the native inet_gethost port program
%% (which Tyn can't spawn — any 'native' lookup is fatal).
%%
%% Two things matter, both learned the hard way:
%%
%%  1. set_lookup([file, dns]) — drop 'native' from the lookup order entirely,
%%     so no code path can ever spawn inet_gethost after boot ('file' first for
%%     localhost/hosts, then 'dns' via inet_res).
%%
%%  2. set_resolv_conf("/tyn/resolv.conf") — point inet at the kernel's file and
%%     let inet own the parsing. This is NOT interchangeable with the obvious
%%     inet_db:add_ns/1: inet_res calls inet_db:res_update_conf() on *every*
%%     query, which reloads nameservers from the configured resolv.conf file and
%%     WIPES anything add_ns put there. add_ns "works" until the first resolve,
%%     then silently reverts to empty -> nxdomain. Pointing at the file makes the
%%     nameservers survive every reload.
%%
%% Deliberately NOT /etc/resolv.conf: ERTS's inet_config reads that path during
%% kernel-app startup; here we hand inet our private copy after boot instead.
%% (The boot-time native-lookup hazard is handled separately by the kernel
%% giving the node a dotted hostname — see src/syscall.rs sys_uname.)
configure_dns() ->
    %% [file, dns] — never `native`. `native` would spawn ERTS's inet_gethost
    %% port program, which Tyn cannot exec (ebadf) and which crashes the node the
    %% moment any app resolves a host (Postgrex's default connect path hit this).
    %% `file` first lets localhost/loopback and any /etc/hosts entries resolve
    %% without a DNS round-trip; `dns` handles everything else via inet_res. This
    %% is the boot-time default so no app has to set it manually.
    inet_db:set_lookup([file, dns]),
    case try_paths(["/tyn/resolv.conf"]) of
        {ok, Bin} ->
            case has_nameserver(Bin) of
                true ->
                    inet_db:set_resolv_conf("/tyn/resolv.conf"),
                    io:format("tyn_boot: DNS -> [file, dns] via /tyn/resolv.conf ~p~n",
                              [inet_db:res_option(nameservers)]);
                false ->
                    io:format("tyn_boot: /tyn/resolv.conf has no nameservers; "
                              "DNS lookups will fail (no upstream)~n")
            end;
        error ->
            io:format("tyn_boot: /tyn/resolv.conf absent; DNS lookups will fail~n")
    end.

has_nameserver(Bin) ->
    case binary:match(Bin, <<"nameserver ">>) of
        nomatch -> false;
        _ -> true
    end.

%% Read sys.config (one Erlang term: [{App,[{K,V}...]}...]) from the cpio and
%% application:set_env each key, so a release's compile-time config takes effect.
apply_sys_config() ->
    case try_paths(["sys.config", "/sys.config", "./sys.config"]) of
        {ok, Bin} ->
            case parse_terms(binary_to_list(Bin)) of
                {ok, [Cfg | _]} when is_list(Cfg) ->
                    _ = [ [ application:set_env(App, K, V) || {K, V} <- KVs ]
                          || {App, KVs} <- Cfg ],
                    io:format("tyn_boot: sys.config applied (~p apps)~n", [length(Cfg)]);
                _ ->
                    io:format("tyn_boot: sys.config present but not parseable, skipping~n")
            end;
        error ->
            ok
    end.

%% Returns a proplist of config terms. Absent boot.config -> demo defaults.
%% Malformed boot.config raises (caught by start/0 -> FAILED, no halt).
load_config() ->
    case get_config_bin() of
        {ok, Bin} ->
            case parse_terms(binary_to_list(Bin)) of
                {ok, Terms} ->
                    io:format("tyn_boot: boot.config loaded (~p terms)~n",
                              [length(Terms)]),
                    Terms;
                {error, R} ->
                    error({boot_config_parse, R})
            end;
        error ->
            io:format("tyn_boot: no boot.config, using defaults~n"),
            defaults()
    end.

%% The demo defaults reproduce the pre-1c hardcoded -eval: start telemetry, then
%% Bandit with bench_plug on 8080. (The old eval also *called*
%% ensure_all_started(jason), but ignored its result — jason cannot start as an
%% application here because its `elixir` app-dependency has no findable
%% elixir.app, yet 'Elixir.Jason':encode! works fine as a plain module call. So
%% jason is intentionally NOT in the app list; see start_apps/1's lenient
%% handling. The old eval likewise did not start a `phoenix` application —
%% bench_plug is a plain Plug — so neither do we.)
defaults() ->
    [{apps, [telemetry]}, {plug, bench_plug}, {port, 8080}].

get_config_bin() -> try_paths(["boot.config", "/boot.config", "./boot.config"]).

try_paths([]) -> error;
try_paths([P | Ps]) ->
    case erl_prim_loader:get_file(P) of
        {ok, Bin, _Full} -> {ok, Bin};
        error -> try_paths(Ps)
    end.

%% Scan the whole file, then split the token stream on each dot and parse_term
%% every chunk. file:consult/1 would be shorter but routes through the unproven
%% file-server path; erl_scan/erl_parse are the proven primitives.
parse_terms(Str) ->
    case erl_scan:string(Str) of
        {ok, Tokens, _} -> parse_tokens(Tokens, []);
        {error, Info, _} -> {error, {scan, Info}}
    end.

parse_tokens([], Acc) ->
    {ok, lists:reverse(Acc)};
parse_tokens(Tokens, Acc) ->
    case take_term(Tokens, []) of
        {ok, TermToks, Rest} ->
            case erl_parse:parse_term(TermToks) of
                {ok, Term} -> parse_tokens(Rest, [Term | Acc]);
                {error, R} -> {error, {parse, R}}
            end;
        {more, _} ->
            {error, missing_dot}
    end.

%% Collect tokens up to and including the next dot (parse_term wants the dot).
take_term([{dot, _} = D | Rest], Acc) -> {ok, lists:reverse([D | Acc]), Rest};
take_term([T | Rest], Acc) -> take_term(Rest, [T | Acc]);
take_term([], Acc) -> {more, lists:reverse(Acc)}.

%% Add release lib/<app>-<vsn>/ebin dirs to the code path so code:lib_dir/1
%% resolves (see apply_config/1). add_pathz is lenient — a bad path returns
%% {error,bad_directory} rather than raising — so a stray entry can't break boot.
add_code_paths([]) -> ok;
add_code_paths(Paths) ->
    _ = [ io:format("tyn_boot: code path ~ts -> ~p~n", [P, code:add_pathz(P)])
          || P <- Paths, is_list(P) ],
    ok.

%% Set OS env vars from boot.config {env,[{"K","V"}...]} before runtime.exs reads
%% them. Tyn has no environment otherwise; this is the recompile-free way to
%% supply SECRET_KEY_BASE / PHX_HOST / PORT (and the precursor to EC2 user-data).
apply_env([]) -> ok;
apply_env(Env) ->
    N = length([ ok || {K, V} <- Env, is_list(K), is_list(V),
                       true =:= os:putenv(K, V) ]),
    io:format("tyn_boot: env set (~p vars)~n", [N]).

%% Evaluate the release's runtime.exs and merge the result into the application
%% environment, before ensure_all_started. Elixir + the config beams are already
%% in the image, so Config.Reader can read it directly. Guarded: a missing or
%% raising runtime.exs is reported, not fatal (the shells stay up either way).
eval_runtime_config() ->
    case runtime_exs_path() of
        undefined ->
            io:format("tyn_boot: no runtime.exs (nothing to evaluate)~n");
        Path ->
            try
                Runtime = 'Elixir.Config.Reader':'read!'(
                            list_to_binary(Path), [{env, prod}]),
                %% MERGE runtime.exs into the existing (sys.config/config.exs)
                %% environment — do NOT replace. put_all_env replaces per {app,key},
                %% so a bare put would wipe compile-time Endpoint keys that
                %% runtime.exs doesn't restate (notably `adapter: Bandit.PhoenixAdapter`
                %% — losing it makes Phoenix fall back to the Cowboy2 adapter and the
                %% listener never comes up). Config.Reader.merge/2 is Mix's own
                %% deep keyword-list merge (runtime wins on conflicts).
                Existing = [ {App, 'Elixir.Application':get_all_env(App)}
                             || {App, _} <- Runtime ],
                Merged0 = 'Elixir.Config.Reader':merge(Existing, Runtime),
                %% Stock Phoenix runtime.exs binds http/https on IPv6-any
                %% ({0,0,0,0,0,0,0,0}) with the comment "bind on all interfaces".
                %% Tyn's stack is IPv4-only, so a `::` bind isn't reachable. Rewrite
                %% any {ip, <8-tuple>} to IPv4-any {0,0,0,0} — same intent, and the
                %% only place `ip:` appears in Phoenix config is the bind address.
                Merged = fix_ipv6_bind(Merged0),
                'Elixir.Application':put_all_env(Merged),
                io:format("tyn_boot: runtime.exs evaluated + merged (~p apps)~n",
                          [length(Merged)])
            catch
                Class:Reason:Stk ->
                    io:format("tyn_boot: runtime.exs eval FAILED ~p:~p~n  ~p~n",
                              [Class, Reason, lists:sublist(Stk, 5)])
            end
    end.

%% runtime.exs is placed at the cpio root by tyn-pack. Config.Reader reads the
%% file itself (via the file server), so return a path erl_prim_loader can find
%% to confirm it exists before handing the name to Config.Reader.
runtime_exs_path() ->
    case try_paths(["runtime.exs", "/runtime.exs", "./runtime.exs"]) of
        {ok, _} -> "runtime.exs";
        error   -> undefined
    end.

%% Recursively rewrite {ip, <IPv6 8-tuple>} -> {ip, {0,0,0,0}} in a config tree,
%% so an IPv4-only Tyn serves a stock app that defaults to binding IPv6-any.
fix_ipv6_bind({ip, T}) when tuple_size(T) =:= 8 -> {ip, {0, 0, 0, 0}};
fix_ipv6_bind({K, V}) -> {K, fix_ipv6_bind(V)};
fix_ipv6_bind(L) when is_list(L) -> [ fix_ipv6_bind(E) || E <- L ];
fix_ipv6_bind(Other) -> Other.

apply_config(Terms) ->
    Port = proplists:get_value(port, Terms, 8080),
    %% Part C — make a STOCK app work unmodified. The runtime configuration a
    %% `mix release` normally evaluates on the target (secret_key_base, the
    %% Endpoint http port, PHX_HOST, ...) lives in runtime.exs, which raises if
    %% its env vars are missing. Order matters and follows Mix's own:
    %%   sys.config (applied earlier in start/0) -> env -> runtime.exs -> apps.
    %% 1. Set env vars from boot.config {env,[{"K","V"}...]} FIRST, so
    %%    System.get_env in runtime.exs sees them (this is also how a user
    %%    supplies SECRET_KEY_BASE without recompiling).
    apply_env(proplists:get_value(env, Terms, [])),
    %% 2. Evaluate runtime.exs (Config.Reader) and put_all_env the result, before
    %%    any app starts and reads its (now-complete) environment.
    eval_runtime_config(),
    %% Put each release app's lib/<app>-<vsn>/ebin on the code path BEFORE
    %% starting apps. This is what makes code:lib_dir/1 (and therefore
    %% Application.app_dir/2 -> priv/static) resolve: without an ebin dir named
    %% <app>-<vsn> on the path, there is no priv to find and Plug.Static /
    %% Phoenix's static handlers fail. The beams themselves still load from the
    %% flat cpio root (the proven path); these entries exist for lib_dir + priv.
    %% tyn-pack emits both the nested layout and this {code_paths,...} list.
    add_code_paths(proplists:get_value(code_paths, Terms, [])),
    start_apps(proplists:get_value(apps, Terms, [])),
    case proplists:get_value(start_mfa, Terms) of
        {M, F, A} when is_list(A) ->
            io:format("tyn_boot: starting via ~p:~p/~p~n", [M, F, length(A)]),
            apply(M, F, A),
            started(M, Port);
        undefined ->
            case proplists:get_value(plug, Terms) of
                undefined ->
                    %% No explicit listener directive. This is the preferred
                    %% form for a Mix release (Phase 2): {apps,[my_app]} started
                    %% above, and my_app's own supervision tree brings up its
                    %% listener (e.g. Bandit). Nothing more for tyn_boot to start.
                    started(apps, Port);
                Plug ->
                    %% 1c shorthand: hand a bare Plug module to Bandit ourselves.
                    {ok, _} = 'Elixir.Bandit':start_link(
                                [{plug, Plug}, {port, Port}, {scheme, http}]),
                    started(Plug, Port)
            end
    end.

started(What, Port) ->
    io:format("tyn_boot: started ~p on port ~p~n", [What, Port]),
    %% Keep the historical marker that every deploy/AMI/sweep script greps for.
    io:format("phoenix_listening~n").

%% Lenient on purpose. The pre-1c -eval called ensure_all_started/1 but ignored
%% the result, and the demo served fine even when a dependency app (jason ->
%% elixir) could not start, because the actual work is done by module calls, not
%% by a running application. So a failed app is a WARNING, not fatal — mirroring
%% that tolerance. The thing that must truly come up is the listener, and its
%% failure (in apply_config/1) is what raises -> tyn_boot: FAILED.
start_apps([]) -> ok;
start_apps([App | Rest]) ->
    case application:ensure_all_started(App) of
        {ok, _}    -> io:format("tyn_boot: app ~p started~n", [App]);
        {error, R} -> io:format("tyn_boot: warning: app ~p did not start: ~p~n",
                                 [App, R])
    end,
    start_apps(Rest).
