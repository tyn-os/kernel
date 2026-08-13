defmodule Ampweb.Manager do
  # Owns the ETS counter table and the pool of amplifier workers. Traps worker
  # exits: a worker that dies (an Elixir-level exception in the hash loop) is
  # counted as :worker_exits and respawned, so the amplifier keeps running and
  # the crash is observable in /chk. A node-level crash (e.g. BUG-1's naive-fix
  # "size_object: bad tag") kills the whole VM instead — observable as /health
  # going dark. Both failure modes are therefore visible over HTTP on Nitro.

  def start_link do
    pid = spawn_link(&init/0)
    {:ok, pid}
  end

  defp init do
    Process.flag(:trap_exit, true)
    Ampweb.Amp.init_table()
    n = Ampweb.Amp.workers()
    mode = Ampweb.Amp.probe_mode()
    deadline = Ampweb.Amp.mono_ms() + Ampweb.Amp.xmm_window_ms()
    pids = for i <- 1..n, into: %{}, do: {Ampweb.Amp.start_worker(i, deadline), i}

    case mode do
      "mixed" ->
        xw = Ampweb.Amp.xmm_workers()
        IO.puts("AMPWEB_BEGIN mode=mixed workers=#{n} xmm_workers=#{xw} md5_workers=#{n - xw} window_ms=#{Ampweb.Amp.xmm_window_ms()}")
        batch_loop(map_size(pids), mode)

      m when m in ["xmm", "xmm_poison"] ->
        IO.puts("AMPWEB_BEGIN mode=#{mode} workers=#{n} window_ms=#{Ampweb.Amp.xmm_window_ms()}")
        batch_loop(map_size(pids), mode)

      _ ->
        IO.puts("AMPWEB_BEGIN mode=md5 workers=#{n} churn_kb=#{Ampweb.Amp.churn_kb()} churn_type=#{Ampweb.Amp.churn_type()}")
        loop(pids)
    end
  end

  # Bounded batch (xmm modes): workers stop at the deadline; when the last one
  # exits, mark phase=done so /chk (now un-starved) reports the final total.
  # No respawn — the point is the scheduler goes idle so the reading is readable.
  defp batch_loop(0, mode) do
    Ampweb.Amp.set_phase("done")
    s = Ampweb.Amp.snapshot()

    if mode == "mixed" do
      # Airtight Cut-1 close: large_md5 is the LIVE positive control (corruption is
      # happening in THIS -smp2 process), xmm_bad the measurement (XMM clean under
      # that same live corruption). large_md5>0 AND xmm_bad=0 => XMM refuted airtight.
      IO.puts("AMPWEB_MIXED_DONE large_md5=#{s[:large_md5]} small_md5=#{s[:small_md5]} iters=#{s[:iters]} input_corrupt=#{s[:input_corrupt]} ref_bad=#{s[:ref_bad]} worker_exits=#{s[:worker_exits]} xmm_bad=#{s[:xmm_bad]} xmm_spans=#{s[:xmm_spans]} nif_ok=#{s[:nif_ok]} mode=mixed")
    else
      IO.puts("AMPWEB_XMM_DONE xmm_bad=#{s[:xmm_bad]} xmm_spans=#{s[:xmm_spans]} nif_ok=#{s[:nif_ok]} mode=#{s[:mode]}")
    end
  end

  defp batch_loop(n, mode) do
    receive do
      {:EXIT, _pid, _reason} -> batch_loop(n - 1, mode)
    end
  end

  # Continuous md5 amplifier: respawn on exit so corruption/crashes stay visible.
  defp loop(pids) do
    receive do
      {:EXIT, pid, _reason} ->
        pids =
          case Map.pop(pids, pid) do
            {nil, p} ->
              p

            {i, p} ->
              _ = bump_exit()
              Map.put(p, Ampweb.Amp.start_worker(i, 0), i)
          end

        loop(pids)
    end
  end

  # Count a worker exit; never let a counter hiccup kill the manager.
  defp bump_exit do
    :ets.update_counter(Ampweb.Amp.table(), :worker_exits, 1)
  rescue
    _ -> 0
  end
end
