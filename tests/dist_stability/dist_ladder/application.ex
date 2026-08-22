defmodule DistLadder.Application do
  @moduledoc """
  Minimal dist-stability diagnostic app. Deliberately NO sustained inter-node
  workload (unlike the soak's DistWorker) — a clean baseline so the data path is
  driven only by explicit /rpc probes and the tick heartbeat. That isolates
  "the connected-phase data path works at all" from "my workload breaks it."

  At boot it lowers net_ticktime (env DIST_TICKTIME, default 8 s) so the
  idle-stability test is fast: if the data path is dead, even the tick can't
  traverse and the peer drops in ~ticktime seconds.
  """
  use Application
  require Logger

  @impl true
  def start(_type, _args) do
    children = [
      {Bandit, plug: DistLadder.Router, scheme: :http, port: 8080},
      %{id: DistLadder.Tick, start: {Task, :start_link, [&lower_ticktime/0]}, restart: :temporary}
    ]

    Supervisor.start_link(children, strategy: :one_for_one, name: DistLadder.Supervisor)
  end

  defp lower_ticktime do
    Process.sleep(4000)
    secs =
      case System.get_env("DIST_TICKTIME") do
        nil -> 8
        s -> case Integer.parse(s) do
               {n, _} -> n
               _ -> 8
             end
      end

    if :erlang.is_alive() do
      r = DistLadder.set_ticktime(secs)
      Logger.info("DIST_LADDER: net_ticktime -> #{secs}s (#{inspect(r)}); node=#{node()}")
    else
      Logger.info("DIST_LADDER: not distributed (no tyn_cookie?) — ticktime unchanged")
    end
  end
end
