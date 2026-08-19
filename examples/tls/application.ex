defmodule L2app.Application do
  use Application
  require Logger

  @impl true
  def start(_type, _args) do
    children =
      [
        # The attack victim: a plain HTTP server. /health returns a tiny known body
        # so an attacker-side client can prove legitimate traffic still flows during
        # and after an attack (the recovery assertion). Also the "plaintext path
        # un-regressed" control for the TLS work.
        {Bandit, plug: L2app.Router, scheme: :http, port: 8080},
        %{id: L2app.Cap, start: {Task, :start_link, [&CapProbe.run/0]}, restart: :temporary},
        # A' OUTBOUND TLS: a real OTP lib (:httpc) makes a verify_peer HTTPS call
        # in-guest through Tyn's RustCrypto NIF + pure-Erlang asn1. Result logged
        # to serial and served at /outbound (byte-exact external response).
        %{id: L2app.Outbound, start: {Task, :start_link, [&outbound_probe/0]}, restart: :temporary}
      ] ++ tls_children()

    Supervisor.start_link(children, strategy: :one_for_one, name: L2app.Supervisor)
  end

  # Outbound HTTPS via OTP :ssl/:httpc (A' path). Runs once at boot after the
  # network is up; verify_peer against the embedded Mozilla CA bundle. The whole
  # crypto path (ECDHE, cert-chain verify, AEAD) + asn1 decode runs in-guest.
  def outbound_probe do
    Process.sleep(5000)
    result =
      try do
        {:ok, _} = Application.ensure_all_started(:inets)
        {:ok, _} = Application.ensure_all_started(:ssl)
        ca_pem = File.read!("/ca-certificates.crt")
        File.write!("/tmp/cacerts.pem", ca_pem)
        opts = [{:ssl, [{:verify, :verify_peer}, {:cacertfile, ~c"/tmp/cacerts.pem"},
                        {:versions, [:"tlsv1.3"]}, {:depth, 10}]}]
        case :httpc.request(:get, {~c"https://example.com/", []}, opts, []) do
          {:ok, {{_v, 200, _r}, _hdrs, body}} ->
            "OUTBOUND-HTTPS: PASS 200 #{length(body)}B cert-verified via A' crypto"
          other ->
            "OUTBOUND-HTTPS: FAIL #{inspect(other)}"
        end
      rescue
        e -> "OUTBOUND-HTTPS: ERROR #{inspect(e)}"
      catch
        k, v -> "OUTBOUND-HTTPS: CATCH #{inspect({k, v})}"
      end

    :persistent_term.put(:outbound_result, result)
    Logger.info(result)
  end

  # In-guest inbound TLS via the tyn_tls rustls NIF. Cert+key are injected via
  # boot-config env (tyn-pack --env TYN_TLS_CERT_B64=.. --env TYN_TLS_KEY_B64=..),
  # base64 of the PEM (single-line, env-safe) — NOT hardcoded, NOT baked into src.
  # Absent env -> no HTTPS listener (graceful; the plaintext path is unaffected).
  defp tls_children do
    with cert_b64 when is_binary(cert_b64) <- System.get_env("TYN_TLS_CERT_B64"),
         key_b64 when is_binary(key_b64) <- System.get_env("TYN_TLS_KEY_B64") do
      cert_pem = Base.decode64!(cert_b64)
      key_pem = Base.decode64!(key_b64)
      File.write!("/tmp/tyn_tls_cert.pem", cert_pem)
      File.write!("/tmp/tyn_tls_key.pem", key_pem)
      Logger.info("[tls] in-guest HTTPS listener on :8443 via Tyn.Transports.RustlsTLS")

      [
        {Bandit,
         plug: L2app.Router,
         scheme: :https,
         port: 8443,
         certfile: "/tmp/tyn_tls_cert.pem",
         keyfile: "/tmp/tyn_tls_key.pem",
         thousand_island_options: [transport_module: Tyn.Transports.RustlsTLS]}
      ]
    else
      _ ->
        Logger.info("[tls] no TYN_TLS_CERT_B64/KEY_B64 in env — HTTPS listener disabled")
        []
    end
  end
end
