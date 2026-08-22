%% Minimal EPMD-less `-epmd_module` for Tyn.
%%
%% Tyn can't exec the epmd daemon, and — the bug this fixes — stock `erl_epmd`
%% still *connects* to 127.0.0.1:4369 to register/resolve even under
%% `-start_epmd false`. On Nitro that loopback connect HANGS, stalling
%% net_kernel before the app ever starts (`[connect] fd=501 -> 127.0.0.1:4369`
%% in the serial log, then the boot never reaches phoenix_listening).
%%
%% This module replaces erl_epmd so distribution never touches epmd: every node
%% binds its dist listener on a FIXED port (9100, pinned via
%% `-kernel inet_dist_listen_min/max 9100`), registration is a no-op, and peer
%% resolution parses the IP-literal node host directly. Wired via the kernel
%% boot args: `-start_epmd false -epmd_module tyn_epmd`.
-module(tyn_epmd).
-export([start_link/0, register_node/2, register_node/3,
         port_please/2, port_please/3, address_please/3,
         listen_port_please/2, names/1]).

-define(DIST_PORT, 9100).
-define(VERSION, 5).

%% No epmd daemon to supervise.
start_link() -> ignore.

%% Accept the registration without contacting epmd. Creation is a small fixed
%% value — fine for a static cluster (incarnation-restart detection is the only
%% thing it affects).
register_node(Name, Port) -> register_node(Name, Port, inet_tcp).
register_node(_Name, _Port, _Driver) -> {ok, 1}.

%% Every node listens on the fixed dist port; report it without an epmd lookup.
port_please(Name, Host) -> port_please(Name, Host, infinity).
port_please(_Name, _Host, _Timeout) -> {port, ?DIST_PORT, ?VERSION}.

%% Resolve the peer host (IP-literal "172.31.x.y" parses directly) and hand back
%% the fixed port + version so net_kernel connects with no epmd round-trip.
address_please(_Name, Host, AddressFamily) ->
    case inet:getaddr(Host, AddressFamily) of
        {ok, Addr} -> {ok, Addr, ?DIST_PORT, ?VERSION};
        {error, _} = E -> E
    end.

%% The port this node's own dist listener binds.
listen_port_please(_Name, _Host) -> {ok, ?DIST_PORT}.

names(_Host) -> {error, address}.
