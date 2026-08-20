defmodule SoakApp.Application do
  @moduledoc """
  Soak workload supervisor: a plain-HTTP boundary (recovery control + /diag),
  an in-guest TLS (rustls) HTTPS boundary when a cert is injected, and the
  inter-node dist worker that keeps the cluster connection used.

  This is deliberately the *integrated* workload from SUSTAINED_TLS_CLUSTER_SOAK:
  TLS at the client edge + real distribution traffic + the bounded-quantities
  instrument, so the soak measures them together over hours rather than each
  briefly in isolation.

  TLS wire-in is config-only via Tyn.Transports.RustlsTLS (see examples/tls);
  cert+key come from boot-config env injected at pack time, never baked in.
  Absent the env, the HTTPS listener is simply skipped (plaintext path unaffected).
  """
  use Application
  require Logger

  @impl true
  def start(_type, _args) do
    children =
      [
        {Bandit, plug: SoakApp.Router, scheme: :http, port: 8080},
        SoakApp.DistWorker
      ] ++ tls_children()

    Supervisor.start_link(children, strategy: :one_for_one, name: SoakApp.Supervisor)
  end

  # In-guest inbound TLS via the tyn_tls rustls NIF, wired config-only. Cert+key
  # injected as base64 PEM via boot-config env (tyn-pack --env ...), never in src.
  defp tls_children do
    with cert_b64 when is_binary(cert_b64) <- System.get_env("TYN_TLS_CERT_B64"),
         key_b64 when is_binary(key_b64) <- System.get_env("TYN_TLS_KEY_B64") do
      File.write!("/tmp/soak_cert.pem", Base.decode64!(cert_b64))
      File.write!("/tmp/soak_key.pem", Base.decode64!(key_b64))
      Logger.info("[soak] in-guest HTTPS on :8443 via Tyn.Transports.RustlsTLS")

      [
        {Bandit,
         plug: SoakApp.Router,
         scheme: :https,
         port: 8443,
         certfile: "/tmp/soak_cert.pem",
         keyfile: "/tmp/soak_key.pem",
         thousand_island_options: [transport_module: Tyn.Transports.RustlsTLS]}
      ]
    else
      _ ->
        Logger.info("[soak] no TYN_TLS_CERT_B64/KEY_B64 — HTTPS listener disabled")
        []
    end
  end
end
