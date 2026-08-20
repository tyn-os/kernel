defmodule SoakApp.DistWorker do
  @moduledoc """
  Keeps the inter-node distribution connection *used*, not just idle-pinged.

  The soak's dist-stability assertion is only meaningful if the connection
  actually carries work over the hours. On an interval this worker, when a peer
  is connected:

    * `rpc:call`s the peer (both directions get exercised because every node runs
      this worker),
    * ships a ~1 MB term across the interconnect and verifies it came back
      byte-exact with `=:=` — deliberately NOT `erlang:md5`, which is
      intermittently non-deterministic for large binaries on Tyn (see
      docs/DIST_ACCEPT_HUNT.md; `=:=` is the honest instrument),
    * counts round-trips and, separately, any *mismatch* or *error*.

  `stats/0` feeds `/diag`. The invariants the soak asserts:
    * `mismatches == 0` and `errors` stays flat (a rising error count = the
      connection is flapping or terms are corrupting),
    * `roundtrips` climbs steadily (the connection stays usable),
    * `peers` matches the configured cluster (no spurious nodedown).

  Peer discovery is static (the direction says static mapping is fine for v1):
  the peer node name(s) come from the `SOAK_PEERS` env (comma-separated,
  IP-literal names like `n@10.0.0.3`) — no in-guest DNS involved.
  """
  use GenServer
  require Logger

  @interval_ms 3_000
  @term_bytes 1_048_576

  def start_link(_), do: GenServer.start_link(__MODULE__, nil, name: __MODULE__)
  def stats, do: GenServer.call(__MODULE__, :stats, 5_000)

  @impl true
  def init(nil) do
    peers = parse_peers(System.get_env("SOAK_PEERS"))
    payload = :crypto.strong_rand_bytes(@term_bytes)
    # Try to bring the static peers up front; keep retrying on the tick if down.
    Enum.each(peers, &Node.connect/1)
    schedule()

    {:ok,
     %{
       peers: peers,
       payload: payload,
       roundtrips: 0,
       mismatches: 0,
       errors: 0,
       last_error: nil,
       last_rtt_ms: 0
     }}
  end

  @impl true
  def handle_call(:stats, _from, s) do
    reply = %{
      peers_configured: length(s.peers),
      peers_connected: length(Node.list()),
      roundtrips: s.roundtrips,
      mismatches: s.mismatches,
      errors: s.errors,
      last_rtt_ms: s.last_rtt_ms,
      last_error: to_string(s.last_error || "none")
    }

    {:reply, reply, s}
  end

  @impl true
  def handle_info(:tick, s) do
    # Reconnect any statically-configured peer that dropped (survives the
    # network-blip / node-restart probes without turnkey discovery).
    Enum.each(s.peers, fn p -> p in Node.list() or Node.connect(p) end)

    s =
      case Node.list() do
        [] ->
          s

        peers ->
          peer = Enum.random(peers)
          t0 = :erlang.monotonic_time(:millisecond)

          try do
            # rpc both exercises the dist channel and returns a value we can
            # check byte-exact against what we sent.
            echoed = :rpc.call(peer, __MODULE__, :echo, [s.payload], 4_000)
            rtt = :erlang.monotonic_time(:millisecond) - t0

            cond do
              echoed == {:badrpc, :timeout} or match?({:badrpc, _}, echoed) ->
                %{s | errors: s.errors + 1, last_error: inspect(echoed)}

              echoed === s.payload ->
                %{s | roundtrips: s.roundtrips + 1, last_rtt_ms: rtt}

              true ->
                # Came back but NOT byte-exact — a real corruption signal.
                %{s | mismatches: s.mismatches + 1, last_error: "term_mismatch"}
            end
          catch
            kind, reason ->
              %{s | errors: s.errors + 1, last_error: inspect({kind, reason})}
          end
      end

    schedule()
    {:noreply, s}
  end

  @doc "Remote entry point for the round-trip: echo the term straight back."
  def echo(term), do: term

  defp schedule, do: Process.send_after(self(), :tick, @interval_ms)

  defp parse_peers(nil), do: []
  defp parse_peers(""), do: []

  defp parse_peers(csv) do
    csv
    |> String.split(",", trim: true)
    |> Enum.map(&(&1 |> String.trim() |> String.to_atom()))
  end
end
