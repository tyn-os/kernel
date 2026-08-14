defmodule Ampapp.HashAmp do
  # Continuation-watch, tractable first cut (directions/CONTINUATION_WATCH.md):
  # the (C) placement / allocator discriminator that attacks "why md5 and not
  # term_to_binary". md5's trap continuation is an ERTS *magic binary* (binary
  # allocator); t2b's is heap. If md5 corrupts ONLY under BINARY-allocator churn
  # (binary.copy) and NOT under HEAP-allocator churn (large heap terms), then the
  # binary allocator over Tyn's mmap is the writer, and the placement difference
  # explains md5-specificity -> confirms mechanism (A) external stray write. If
  # heap churn ALSO corrupts md5, it's not binary-allocator-specific -> argues (B)
  # save/restore. The instrument can return "not A" (heap churn corrupts) — it is
  # built to exclude, not confirm.
  #
  #   TYN_CHURN_TYPE = "binary" (default) | "heap" | "none"
  #
  # Measures large-md5 transients (control) + small-md5 (no-trap anchor, must be 0),
  # with full input/reference/recompute disambiguation.
  #
  # Env: TYN_AMP_RUNTIME_MS (60000), TYN_AMP_WORKERS (16), TYN_AMP_CHURN_KB (128),
  #      TYN_CHURN_TYPE (binary).

  @default_runtime_ms 60_000
  @default_workers 16
  @default_churn_kb 128
  @small_bytes 32
  @large_kb 64

  def run do
    Process.sleep(1500)
    # SCALAR_STATE_ELIM: TYN_PROBE=fsbase runs the FS_BASE survival probe instead
    # of the md5 amplifier — confirms whether Tyn's preemptive context switch
    # preserves the interrupted thread's FS_BASE (leading suspect for BUG-1).
    rt = env_int("TYN_AMP_RUNTIME_MS", @default_runtime_ms)
    wk = env_int("TYN_AMP_WORKERS", @default_workers)
    case System.get_env("TYN_PROBE") do
      "fsbase" -> fsbase_run(:probe, "FSBASE", rt, wk)
      "gp"     -> fsbase_run(:gp_probe, "GP", rt, wk)
      "rflags" -> fsbase_run(:rflags_probe, "RFLAGS", rt, wk)
      "xmm"    -> fsbase_run(:xmm_probe, "XMM", rt, wk)
      "redzone" -> fsbase_run(:redzone_probe, "REDZONE", rt, wk)
      "xmm_poison" -> fsbase_run(:xmm_poison, "XMMPOISON", rt, wk)
      "suite" -> suite_run(wk)
      _ -> amp_run()
    end
  end

  # Layer-0 probe suite: run every register/red-zone survival probe in ONE boot,
  # each for a short window, plus the xmm_poison TEETH. Labels distinguish the
  # class (register: FSBASE/GP/RFLAGS/XMM; memory: REDZONE) so the md5-is-scalar
  # conflation can't recur. A clean kernel yields bad=0 for all survival probes;
  # XMMPOISON MUST yield bad>0 — it deliberately corrupts xmm before readback, so a
  # 0 there means the whole spin+readback+compare detection harness is inert.
  # Each probe is guarded by probe_bound?/1 so the suite runs whatever the linked
  # probe beam actually exports (functions not bound print {LABEL}_UNAVAILABLE).
  defp suite_run(workers) do
    w = env_int("TYN_SUITE_WINDOW_MS", 4_000)
    IO.puts("SUITE_BEGIN workers=#{workers} window_ms=#{w} nif_ok=#{if fsbase_probe_ok?(), do: 1, else: 0}")
    for {fun, label} <- [
          {:probe, "FSBASE"}, {:gp_probe, "GP"}, {:rflags_probe, "RFLAGS"},
          {:xmm_probe, "XMM"}, {:redzone_probe, "REDZONE"},
          {:xmm_poison, "XMMPOISON"}    # teeth — must fire (bad>0)
        ] do
      if probe_bound?(fun),
        do: fsbase_run(fun, label, w, workers),
        else: IO.puts("#{label}_UNAVAILABLE (nif fn not bound)")
    end
    IO.puts("SUITE_DONE")
  end

  defp fsbase_probe_ok? do
    try do :fsbase_probe.available() rescue _ -> false catch _, _ -> false end
  end

  # Is this NIF function actually bound in the linked beam? A tiny probe call that
  # returns an integer => bound; nif_error (stub) or any raise => not bound.
  defp probe_bound?(fun) do
    try do is_integer(apply(:fsbase_probe, fun, [1, 1000])) rescue _ -> false catch _, _ -> false end
  end

  # FS_BASE survival probe: spawn `workers` processes, each repeatedly holds its
  # own FS_BASE across a long call-free spin (`:fsbase_probe.probe/2`) and counts
  # spans where FS_BASE changed across a preemption. bad>0 => the preemptive
  # context switch lost FS_BASE (reads another thread's TLS base).
  @fsbase_spin 20_000_000
  defp fsbase_run(fun, label, runtime, workers) do
    IO.puts("#{label}_BEGIN workers=#{workers} runtime_ms=#{runtime} spin=#{@fsbase_spin}")
    case :fsbase_probe.available() do
      true -> :ok
      _ -> IO.puts("#{label}_TOTAL bad=0 spans=0 (PROBE_UNAVAILABLE)")
    end
    parent = self()
    deadline = mono_ms() + runtime
    for i <- 1..workers, do: spawn(fn -> fsbase_worker(fun, label, i, parent, deadline, 0, 0) end)
    {bad, spans} = collect_fsbase(workers, {0, 0}, runtime)
    IO.puts("#{label}_TOTAL bad=#{bad} spans=#{spans}")
  end

  defp fsbase_worker(fun, label, i, parent, deadline, bad, spans) do
    if mono_ms() >= deadline do
      send(parent, {:fsdone, bad, spans})
    else
      b = apply(:fsbase_probe, fun, [1, @fsbase_spin])
      if b > 0, do: IO.puts("#{label}_BAD worker=#{i} span=#{spans + 1} bad=#{b}")
      fsbase_worker(fun, label, i, parent, deadline, bad + b, spans + 1)
    end
  end

  defp collect_fsbase(0, {b, s}, _rt), do: {b, s}
  defp collect_fsbase(n, {b, s}, rt) do
    receive do
      {:fsdone, wb, ws} -> collect_fsbase(n - 1, {b + wb, s + ws}, rt)
    after
      rt + 45_000 -> {b, s}
    end
  end

  defp amp_run do
    runtime = env_int("TYN_AMP_RUNTIME_MS", @default_runtime_ms)
    workers = env_int("TYN_AMP_WORKERS", @default_workers)
    churn_kb = env_int0("TYN_AMP_CHURN_KB", @default_churn_kb)
    ctype = System.get_env("TYN_CHURN_TYPE") || "binary"
    IO.puts("AMP_BEGIN mode=churncut workers=#{workers} runtime_ms=#{runtime} churn_kb=#{churn_kb} churn_type=#{ctype}")

    parent = self()
    deadline = mono_ms() + runtime
    for i <- 1..workers, do: spawn(fn -> worker(i, parent, deadline, churn_kb, ctype) end)
    collect(workers, {0, 0}, runtime)
    IO.puts("AMP_END")
  end

  defp worker(i, parent, deadline, churn_kb, ctype) do
    su = <<i, 0xC3, 0x3C, rem(i * 11, 256)>>
    small = :binary.copy(su, div(@small_bytes, 4))
    lu = <<i, 0x5A, 0xA5, rem(i * 7, 256)>>
    lreps = div(@large_kb * 1024, 4)
    large = :binary.copy(lu, lreps)
    creps = div(max(churn_kb, 1) * 1024, 4)
    # heap churn: a list of small immediates ~ churn_kb (≈16 B/cons cell)
    heap_n = div(max(churn_kb, 1) * 1024, 16)

    r_small = :erlang.md5(small)
    r_large = :erlang.md5(large)
    loop(i, parent, deadline, su, small, r_small, lu, lreps, large, r_large, creps, heap_n, ctype, 0, 0)
  end

  defp loop(i, parent, deadline, su, small, r_small, lu, lreps, large, r_large, creps, heap_n, ctype, sT, lT) do
    if mono_ms() >= deadline do
      send(parent, {:done, sT, lT})
    else
      s = if :erlang.md5(small) != r_small,
            do: genuine?(i, :SMALL, div(@small_bytes, 4), su, small, r_small), else: 0
      l = if :erlang.md5(large) != r_large,
            do: genuine?(i, :LARGE, lreps, lu, large, r_large), else: 0

      churn(ctype, lu, creps, heap_n)

      loop(i, parent, deadline, su, small, r_small, lu, lreps, large, r_large, creps, heap_n, ctype, sT + s, lT + l)
    end
  end

  defp churn("binary", lu, creps, _hn), do: (_ = :binary.copy(lu, creps); :ok)
  defp churn("heap", _lu, _creps, hn), do: (_ = heap_junk(hn, []); :ok)
  defp churn(_none, _lu, _creps, _hn), do: :ok

  # build a list of `n` small immediates on the process heap, then drop it (GC).
  defp heap_junk(0, acc), do: acc
  defp heap_junk(n, acc), do: heap_junk(n - 1, [rem(n, 256) | acc])

  defp genuine?(i, tag, reps, unit, bin, ref) do
    expected = :binary.copy(unit, reps)
    cond do
      bin != expected ->
        IO.puts("AMP_INPUT_CORRUPT #{tag} worker=#{i}"); 0
      :erlang.md5(bin) == ref and :erlang.md5(expected) == ref ->
        IO.puts("AMP_TRANSIENT #{tag} worker=#{i}"); 1
      true ->
        IO.puts("AMP_REF_BAD #{tag} worker=#{i}"); 0
    end
  end

  defp collect(0, {s, l}, _rt), do: IO.puts("AMP_TOTAL small_md5=#{s} large_md5=#{l}")

  defp collect(n, {s, l}, rt) do
    receive do
      {:done, ws, wl} -> collect(n - 1, {s + ws, l + wl}, rt)
    after
      rt + 45_000 -> IO.puts("AMP_TOTAL small_md5=#{s} large_md5=#{l} (TIMEOUT missing=#{n})")
    end
  end

  defp env_int(name, d), do: parse_env(name, d, 1)
  defp env_int0(name, d), do: parse_env(name, d, 0)
  defp parse_env(name, default, min) do
    case System.get_env(name) do
      nil -> default
      s -> case Integer.parse(s) do
             {v, _} when v >= min -> v
             _ -> default
           end
    end
  end

  defp mono_ms, do: System.monotonic_time(:millisecond)
end
