defmodule SoakApp.Diag do
  @moduledoc """
  Bounded-quantities snapshot for the sustained TLS+cluster soak.

  Everything here is a *quantity that must stay bounded over the run* — not a
  liveness flag. The soak driver scrapes `/diag` on an interval and asserts each
  of these stays within a band; a slow leak shows up as monotone creep, which a
  point-in-time "still up?" check can't catch.

  Grouped to match the soak assertion (SUSTAINED_TLS_CLUSTER_SOAK):

    * mem.*       — TLS session heap accretion. `binary` is the load-bearing one:
                    rustls/`:ssl` session state and large terms land in the BEAM
                    binary heap, so unbounded `binary` growth under TLS churn is
                    the BUG-8-class regression this hunt exists to catch. Reported
                    alongside kernel-heap `heap_free` (from the `[diag]` serial
                    trace) so we can see *which* heap moves.
    * counts.*    — fd/socket/process drift (process_count, port_count = the fd
                    analogue on BEAM, atom_count = the classic unbounded-atom leak).
    * dist.*      — distribution stability: connected peers + the worker's running
                    tally of inter-node round-trips and any mismatch/error.
    * clock.*     — kvmclock long-run drift. `system_ms` is os:system_time; the
                    driver compares it to host UTC at scrape time to measure drift.
                    `monotonic_ms` is the exact monotonic reference.
    * work.*      — throughput proxy (reductions) so latency/throughput creep is
                    visible in the same snapshot.
  """

  def snapshot do
    %{
      # BEAM wall-clock uptime in ms (first element of the {total, since_last} pair).
      t_uptime_ms: elem(:erlang.statistics(:wall_clock), 0),
      mem: %{
        total: :erlang.memory(:total),
        processes: :erlang.memory(:processes),
        binary: :erlang.memory(:binary),
        ets: :erlang.memory(:ets),
        atom_used: :erlang.memory(:atom_used)
      },
      counts: %{
        process_count: :erlang.system_info(:process_count),
        port_count: :erlang.system_info(:port_count),
        atom_count: :erlang.system_info(:atom_count),
        ets_count: length(:ets.all())
      },
      dist: SoakApp.DistWorker.stats(),
      clock: %{
        system_ms: :os.system_time(:millisecond),
        monotonic_ms: :erlang.monotonic_time(:millisecond)
      },
      work: %{
        reductions: elem(:erlang.statistics(:reductions), 0),
        run_queue: :erlang.statistics(:total_run_queue_lengths)
      }
    }
  end

  @doc "Compact JSON without a JSON dep — the driver parses this line."
  def to_json(map) when is_map(map) do
    "{" <> Enum.map_join(map, ",", fn {k, v} -> "#{enc(k)}:#{enc(v)}" end) <> "}"
  end

  defp enc(v) when is_map(v), do: to_json(v)
  defp enc(v) when is_atom(v), do: "\"#{v}\""
  defp enc(v) when is_binary(v), do: "\"#{v}\""
  defp enc(v) when is_list(v), do: "[" <> Enum.map_join(v, ",", &enc/1) <> "]"
  defp enc(v) when is_integer(v), do: Integer.to_string(v)
  defp enc(v), do: "\"#{inspect(v)}\""
end
