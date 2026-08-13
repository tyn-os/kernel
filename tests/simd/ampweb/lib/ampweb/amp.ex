defmodule Ampweb.Amp do
  # Continuous md5-preemption amplifier. Same measured workload as
  # tests/simd/ampapp/lib/ampapp/hash_amp.ex — each worker holds a reference
  # md5 of a known small (32 B) and large (64 KiB) binary, recomputes them in a
  # tight loop under binary-allocator churn, and counts GENUINE transient
  # mismatches (input intact + both recomputes now match the reference =>
  # the digest was momentarily wrong, i.e. BUG-1's red-zone clobber) — with the
  # same input/reference/recompute disambiguation so a corrupted input or a bad
  # reference is never miscounted as a transient.
  #
  # Differences from ampapp: runs forever (no deadline) and accumulates into a
  # shared ETS counter table (:ampstats) instead of collecting to a serial line,
  # so Ampweb.Http can report the running totals over HTTP on Nitro.
  #
  # Env: TYN_AMP_WORKERS (16), TYN_AMP_CHURN_KB (128), TYN_CHURN_TYPE (binary).

  @table :ampstats
  @keys [:iters, :small_md5, :large_md5, :input_corrupt, :ref_bad, :worker_exits,
         :xmm_bad, :xmm_spans]
  @small_bytes 32
  @large_kb 64
  @default_workers 16
  @default_churn_kb 128
  # Call-free spin per span, sized so a ~10 ms timer tick reliably lands WHILE
  # xmm0-15 are held live (preemption mid-span → real cross-thread switch → the
  # exact bug trigger). ~5M iters ≈ 0.5 s on TCG (many ticks), ~15 ms on KVM (>1
  # tick). Each span is one NIF call; the worker LOOPS spans over a bounded window
  # then STOPS, so /chk (read AFTER the window) isn't starved — bounded-batch, not
  # a continuous acceptor-starving probe. See directions/XMM_PROBE_REDESIGN.md.
  @xmm_spin 5_000_000
  @default_xmm_window_ms 90_000

  def table, do: @table
  def keys, do: @keys

  # TYN_PROBE selects the workload: "xmm" runs the hardened XMM-survival probe
  # (fsbase_probe:xmm_probe, all xmm0-15 across a call-free spin — a preemption
  # that loses/garbles XMM is caught); "xmm_poison" runs the detection teeth-test
  # (fsbase_probe:xmm_poison, deliberately corrupts xmm0 -> must report xmm_bad>0,
  # proving the detector FIRES on a wrong value, not just that it runs); anything
  # else runs the md5 amplifier.
  def probe_mode, do: System.get_env("TYN_PROBE") || "md5"

  # 1 iff the probe NIF is actually bound (md5 mode: n/a -> 1). A 0 here means the
  # xmm reading is INERT (NIF not loaded) — so /chk can never present an inert
  # probe as a clean xmm_bad=0.
  def nif_ok do
    case probe_mode() do
      m when m in ["xmm", "xmm_poison"] ->
        (try do
           if :fsbase_probe.available(), do: 1, else: 0
         rescue _ -> 0 catch _, _ -> 0 end)

      _ -> 1
    end
  end

  @doc "Create the shared counter table. Idempotent; call once before workers."
  def init_table do
    case :ets.info(@table) do
      :undefined ->
        :ets.new(@table, [:set, :public, :named_table, write_concurrency: true])
        for k <- @keys, do: :ets.insert(@table, {k, 0})
        :ets.insert(@table, {:workers, workers()})
        :ets.insert(@table, {:mode, probe_mode()})
        :ets.insert(@table, {:nif_ok, nif_ok()})
        # phase: "running" while the bounded batch runs, "done" after it stops.
        # md5 mode never stops, so it stays "running".
        :ets.insert(@table, {:phase, "running"})
        :ok

      _ ->
        :ok
    end
  end

  def xmm_window_ms, do: env_int("TYN_XMM_WINDOW_MS", @default_xmm_window_ms, 1)
  def mono_ms, do: System.monotonic_time(:millisecond)
  def set_phase(p), do: :ets.insert(@table, {:phase, p})

  @doc "Snapshot the counters as a keyword list."
  def snapshot do
    for k <- @keys ++ [:workers, :mode, :nif_ok, :phase] do
      {k, (case :ets.lookup(@table, k) do
             [{^k, v}] -> v
             _ -> 0
           end)}
    end
  end

  def workers, do: env_int("TYN_AMP_WORKERS", @default_workers, 1)
  def churn_kb, do: env_int("TYN_AMP_CHURN_KB", @default_churn_kb, 0)
  def churn_type, do: System.get_env("TYN_CHURN_TYPE") || "binary"

  @doc """
  Start worker `i`. In xmm/xmm_poison mode it runs BOUNDED (loops XMM spans until
  `deadline`, then the process exits :normal so the scheduler frees and /chk can
  answer); in md5 mode it runs continuously (deadline ignored).
  """
  def start_worker(i, deadline) do
    case probe_mode() do
      "xmm" -> spawn_link(fn -> xmm_worker(:xmm_probe, deadline) end)
      "xmm_poison" -> spawn_link(fn -> xmm_worker(:xmm_poison, deadline) end)
      _ -> spawn_link(fn -> worker(i) end)
    end
  end

  # XMM-survival worker: loop holding xmm0-15 live across a long call-free spin
  # (each ~1+ timer tick, so a preemption lands mid-span → real cross-thread
  # switch — the bug trigger) and count spans that came back wrong. Runs until
  # `deadline`, then returns (process exits normally → scheduler free → /chk
  # reports the stored total un-starved). bad>0 => a preemption lost/garbled XMM.
  defp xmm_worker(fun, deadline) do
    if mono_ms() >= deadline do
      :done
    else
      bad =
        try do
          apply(:fsbase_probe, fun, [1, @xmm_spin])
        rescue
          _ -> 0
        catch
          _, _ -> 0
        end

      if is_integer(bad) and bad > 0, do: bump(:xmm_bad, bad)
      bump(:xmm_spans, 1)
      xmm_worker(fun, deadline)
    end
  end

  defp worker(i) do
    ctype = churn_type()
    ckb = churn_kb()
    su = <<i, 0xC3, 0x3C, rem(i * 11, 256)>>
    small = :binary.copy(su, div(@small_bytes, 4))
    lu = <<i, 0x5A, 0xA5, rem(i * 7, 256)>>
    lreps = div(@large_kb * 1024, 4)
    large = :binary.copy(lu, lreps)
    creps = div(max(ckb, 1) * 1024, 4)

    r_small = :erlang.md5(small)
    r_large = :erlang.md5(large)
    loop(i, su, small, r_small, lu, lreps, large, r_large, creps, ctype)
  end

  # One ETS bump of :iters per iteration keeps /chk progress always visible
  # (the md5(64KiB)+churn work dominates a single counter increment by orders of
  # magnitude, so the leaf code the red zone lives in stays the hot preemption
  # target). Genuine mismatches — rare — are counted the moment they occur.
  defp loop(i, su, small, r_small, lu, lreps, large, r_large, creps, ctype) do
    if :erlang.md5(small) != r_small do
      if genuine(i, :SMALL, div(@small_bytes, 4), su, small, r_small) == 1, do: bump(:small_md5, 1)
    end

    if :erlang.md5(large) != r_large do
      if genuine(i, :LARGE, lreps, lu, large, r_large) == 1, do: bump(:large_md5, 1)
    end

    churn(ctype, lu, creps)
    bump(:iters, 1)

    loop(i, su, small, r_small, lu, lreps, large, r_large, creps, ctype)
  end

  defp churn("binary", lu, creps), do: (_ = :binary.copy(lu, creps); :ok)
  defp churn("heap", _lu, creps), do: (_ = heap_junk(div(max(creps, 1), 4), []); :ok)
  defp churn(_none, _lu, _creps), do: :ok

  defp heap_junk(0, acc), do: acc
  defp heap_junk(n, acc), do: heap_junk(n - 1, [rem(n, 256) | acc])

  # Disambiguate a raw md5 mismatch exactly as ampapp does: only a genuine
  # transient (input still intact, reference still valid) counts as 1.
  defp genuine(i, tag, reps, unit, bin, ref) do
    expected = :binary.copy(unit, reps)
    cond do
      bin != expected ->
        bump(:input_corrupt, 1)
        _ = tag
        _ = i
        0

      :erlang.md5(bin) == ref and :erlang.md5(expected) == ref ->
        1

      true ->
        bump(:ref_bad, 1)
        0
    end
  end

  defp bump(key, n) do
    :ets.update_counter(@table, key, n)
  rescue
    _ -> 0
  end

  defp env_int(name, default, min) do
    case System.get_env(name) do
      nil -> default
      s ->
        case Integer.parse(s) do
          {v, _} when v >= min -> v
          _ -> default
        end
    end
  end
end
