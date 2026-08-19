-module(out_probe).
-export([run/1]).

%% Full outbound proof on CLEAN OTP-27 (asdf) with the A' beam: :ssl.connect with
%% verify_peer + a real CA bundle to a public HTTPS endpoint. Every crypto op
%% (ECDHE + cert-chain/CertificateVerify signature checks) runs through MY NIF.
run([Host]) ->
    {ok, _} = application:ensure_all_started(ssl),
    Opts = [{verify, verify_peer},
            {cacertfile, "/etc/ssl/certs/ca-certificates.crt"},
            {versions, ['tlsv1.3', 'tlsv1.2']},
            {active, false}, {depth, 10},
            {server_name_indication, Host}],
    case ssl:connect(Host, 443, Opts, 20000) of
        {ok, Sock} ->
            {ok, Info} = ssl:connection_information(Sock, [protocol, selected_cipher_suite]),
            ok = ssl:send(Sock, ["GET / HTTP/1.1\r\nHost: ", Host, "\r\nConnection: close\r\n\r\n"]),
            First = case ssl:recv(Sock, 0, 10000) of
                        {ok, D} -> hd(string:split(binary_to_list(iolist_to_binary(D)), "\r\n"));
                        E -> E
                    end,
            catch ssl:close(Sock),
            io:format("[out] connected + cert VERIFIED; ~p~n", [Info]),
            io:format("[out] server said: ~s~n", [First]),
            io:format("OUT-PROBE: PASS (outbound HTTPS, verify_peer, via my crypto NIF)~n");
        {error, Reason} ->
            io:format("[out] ssl:connect -> ~p~n", [Reason]),
            io:format("OUT-PROBE: FAIL~n")
    end,
    halt(0).
