defmodule GpApp.Application do
  @moduledoc """
  Boot-runs the GP_HUNT reproducer (concurrent async tmpfs I/O under SMP) and
  prints the result to serial, so the Nitro harness reads a verdict off the
  console with no eval-shell driving. Also serves /health (boot detection) and
  /gp (the stored result).

  The reproducer runs on the REAL scheduler set — on a 4-vCPU Nitro instance
  that's `schedulers_online=4`, so concurrent large writes get preempted by the
  SMP wakeup IPI: exactly the window BUG-1 lived in. Params come from env:
    GP_PROCS (default 3), GP_SIZE (default 1048576), GP_ITERS (default 500).
  """
  use Application
  require Logger

  @impl true
  def start(_type, _args) do
    children = [
      {Bandit, plug: GpApp.Router, scheme: :http, port: 8080},
      %{id: GpApp.Repro, start: {Task, :start_link, [&run_repro/0]}, restart: :temporary}
    ]

    Supervisor.start_link(children, strategy: :one_for_one, name: GpApp.Supervisor)
  end

  def run_repro do
    # Let the network + all schedulers come fully up first, so the run happens
    # under steady SMP concurrency (the race window), not during single-threaded
    # boot.
    Process.sleep(6000)
    procs = env_int("GP_PROCS", 3)
    size = env_int("GP_SIZE", 1_048_576)
    iters = env_int("GP_ITERS", 500)

    Logger.info("GP_REPRO: launching (procs=#{procs} size=#{size} iters=#{iters})")
    result = GpHunt.run(procs, size, iters)
    :persistent_term.put(:gp_result, result)
    Logger.info("GP_REPRO_RESULT: #{inspect(result)}")
  end

  defp env_int(k, d) do
    case System.get_env(k) do
      nil -> d
      s -> case Integer.parse(s) do
             {n, _} -> n
             _ -> d
           end
    end
  end
end
