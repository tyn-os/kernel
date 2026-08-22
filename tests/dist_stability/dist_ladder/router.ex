defmodule DistLadder.Router do
  @moduledoc """
  Probe endpoints for the dist-stability ladder — driven interactively from the
  host during the watched Nitro window.

    * GET /health           — L2_OK (boot/liveness).
    * GET /diststat         — node, alive, peers, net_ticktime, uptime. Poll this
                              to watch a formation and a drop over time.
    * POST /connect?node=&cookie=  — host-driven formation (same as the soak).
    * GET /rpc?bytes=N      — THE data-path probe: rpc-echo a fresh N-byte payload
                              to the first peer and report whether a term frame
                              traversed AND came back byte-exact. tiny N vs 1MB N =
                              the any-data-vs-large-data discriminator.
  """
  use Plug.Router

  plug(:match)
  plug(:dispatch)

  get "/health", do: send_resp(conn, 200, "L2_OK")

  get "/diststat" do
    body =
      json(%{
        node: to_string(node()),
        alive: :erlang.is_alive(),
        peers: Enum.map(Node.list(), &to_string/1),
        peer_count: length(Node.list()),
        net_ticktime: safe(fn -> :net_kernel.get_net_ticktime() end),
        uptime_s: div(elem(:erlang.statistics(:wall_clock), 0), 1000)
      })

    conn |> put_resp_content_type("application/json") |> send_resp(200, body)
  end

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

  get "/rpc" do
    conn = fetch_query_params(conn)
    bytes = String.to_integer(conn.query_params["bytes"] || "100")
    timeout = String.to_integer(conn.query_params["timeout"] || "5000")

    result =
      case Node.list() do
        [] ->
          %{ok: false, traversed: false, reason: "no_peer", bytes: bytes}

        [peer | _] ->
          payload = :crypto.strong_rand_bytes(bytes)
          t0 = :erlang.monotonic_time(:millisecond)
          reply = :rpc.call(peer, DistLadder, :echo, [payload], timeout)
          rtt = :erlang.monotonic_time(:millisecond) - t0

          case reply do
            # Pinned exact-match: a real term frame traversed both ways, byte-exact.
            ^payload ->
              %{ok: true, traversed: true, byte_exact: true, bytes: bytes, rtt_ms: rtt, peer: to_string(peer)}

            {:badrpc, r} ->
              %{ok: false, traversed: false, reason: inspect(r), bytes: bytes, rtt_ms: rtt}

            other when is_binary(other) ->
              # Came back but NOT the bytes we sent — real corruption on the path.
              %{ok: false, traversed: true, byte_exact: false, bytes: bytes, got: byte_size(other), rtt_ms: rtt}

            other ->
              %{ok: false, traversed: false, reason: inspect(other), bytes: bytes, rtt_ms: rtt}
          end
      end

    conn |> put_resp_content_type("application/json") |> send_resp(200, json(result))
  end

  match _, do: send_resp(conn, 404, "not found")

  # --- minimal JSON (no dep); escapes strings so no field can break the output.
  defp safe(f) do
    try do
      f.()
    catch
      _, _ -> "err"
    end
  end

  defp json(m) when is_map(m), do: "{" <> Enum.map_join(m, ",", fn {k, v} -> "#{enc(k)}:#{enc(v)}" end) <> "}"
  defp enc(v) when is_map(v), do: json(v)
  defp enc(v) when is_integer(v), do: Integer.to_string(v)
  defp enc(v) when is_boolean(v), do: to_string(v)
  defp enc(v) when is_atom(v), do: enc(to_string(v))
  defp enc(v) when is_list(v), do: "[" <> Enum.map_join(v, ",", &enc/1) <> "]"

  defp enc(v) when is_binary(v) do
    esc = v |> String.replace("\\", "\\\\") |> String.replace("\"", "\\\"")
    "\"" <> esc <> "\""
  end

  defp enc(v), do: enc(inspect(v))
end
