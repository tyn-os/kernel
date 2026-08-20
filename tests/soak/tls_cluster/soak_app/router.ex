defmodule SoakApp.Router do
  @moduledoc """
  Minimal Plug router for the soak workload. Content, not status codes:

    * `GET /health`  — tiny known body (`L2_OK`) so the driver can assert the
      plaintext path stays live during/after the restart+blip probes (the
      recovery control), identical to the resiliency harness's health check.
    * `GET /diag`    — the bounded-quantities JSON snapshot the driver scrapes on
      an interval (SoakApp.Diag). This is the soak's actual instrument.
    * `GET /work`    — forces one inter-node large-term round-trip on demand and
      reports ok/mismatch/error, so the driver can drive dist traffic from the
      client side too (not only the worker's own tick).
  """
  use Plug.Router

  plug(:match)
  plug(:dispatch)

  get "/health" do
    send_resp(conn, 200, "L2_OK")
  end

  get "/diag" do
    body = SoakApp.Diag.snapshot() |> SoakApp.Diag.to_json()
    conn
    |> put_resp_content_type("application/json")
    |> send_resp(200, body)
  end

  get "/work" do
    body =
      case SoakApp.DistWorker.stats() do
        %{peers_connected: 0} -> "WORK: no peer connected"
        %{roundtrips: n, mismatches: 0, errors: e} -> "WORK: ok roundtrips=#{n} errors=#{e}"
        %{mismatches: m} -> "WORK: MISMATCH count=#{m}"
      end

    send_resp(conn, 200, body)
  end

  # Host-driven cluster formation (the proven dist-spike approach: static,
  # IP-literal node names, no in-guest DNS). The Nitro orchestrator, once both
  # instances are up and their DHCP IPs are known, POSTs each node its peer:
  #   curl -X POST "http://<ip>:8080/connect?node=n@<peer-ip>&cookie=<c>"
  # Idempotent; the DistWorker keeps the link alive from there.
  post "/connect" do
    conn = fetch_query_params(conn)
    peer = conn.query_params["node"]
    cookie = conn.query_params["cookie"]
    cookie && :erlang.set_cookie(String.to_atom(cookie))

    result =
      case peer && Node.connect(String.to_atom(peer)) do
        true -> "CONNECT: ok peer=#{peer} nodes=#{inspect(Node.list())}"
        other -> "CONNECT: fail peer=#{inspect(peer)} -> #{inspect(other)}"
      end

    send_resp(conn, 200, result)
  end

  match _ do
    send_resp(conn, 404, "not found")
  end
end
