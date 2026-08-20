# GP_HUNT reproducer — concurrent async tmpfs I/O under SMP.
#
# The "tmpfs large-write #GP" is NOT a tmpfs bug (GP_PROBE_A0: the fault RIPs sit
# in BeamAsm JIT code, not tmpfs/kernel). It's the SMP memory-corruption class —
# a spilled binary pointer clobbered under preemption, then dereferenced wild by
# JIT code → non-canonical → #GP. tmpfs was just the workload that put enough
# large parallel sys_write copies in flight (via the ERTS +A async pool) to widen
# the race window. That class == BUG-1 (IPI vector 34 missing its IST → red-zone
# clobber), now fixed in-tree (7270266). This drives the *file-I/O surface* of
# that corruption to check whether BUG-1's fix closed it.
#
# Detection is TWO-sided, because the fault has two faces:
#   * hard #GP  → the node crashes; the kernel's loud #GP handler prints
#     "#GP ip=..." + GPRs on serial (interrupts.rs gpf_handler). Watch the console.
#   * silent corruption → the readback is NOT byte-exact. Verified with `===`
#     (structural), NEVER erlang:md5 (itself flaky on large binaries here — the
#     same corruption class; a broken instrument, see docs/DIST_ACCEPT_HUNT.md).
#
# REQUIRES REAL SMP + the async pool. The race is SMP-only (like BUG-1); TCG
# serializes and almost certainly will NOT reproduce it — TCG runs validate the
# harness mechanics only. Authoritative run: Nitro (c5.xlarge = 4 vCPU). Boot the
# node with `+A 4` (>=2 async threads) so File.write dispatches to parallel OS
# threads; the reproducer prints the pool size it actually saw.
#
# Total live bytes are kept UNDER the 4 MiB tmpfs cap (procs*size < 4 MiB) so the
# large copies actually run instead of starving on ENOSPC — cap-thrash is noise;
# the parallel in-flight memcpy is the signal.
#
# Usage (eval shell):  c("/path/gp_repro.exs"); GpHunt.run(3, 1_048_576, 500)
#   args: procs, bytes_each, iterations.  Default 3 x 1 MiB x 500.

defmodule GpHunt do
  def run(procs \\ 3, size \\ 1_048_576, iters \\ 500) do
    pool = :erlang.system_info(:thread_pool_size)
    schedulers = :erlang.system_info(:schedulers_online)
    IO.puts("GP_REPRO: start procs=#{procs} size=#{size} iters=#{iters} " <>
            "+A(async_pool)=#{pool} schedulers_online=#{schedulers} " <>
            "live_bytes=#{procs * size} (cap 4MiB)")

    if procs * size >= 4 * 1024 * 1024 do
      IO.puts("GP_REPRO: WARN procs*size >= 4MiB cap — writes will ENOSPC-thrash; lower it")
    end

    # One fixed payload; every worker writes it and must read it back identical.
    payload = :crypto.strong_rand_bytes(size)
    parent = self()

    for i <- 1..procs do
      spawn(fn -> worker(i, size, iters, payload, parent) end)
    end

    collect(procs, %{mismatches: 0, enospc: 0, other_err: 0, ok: 0})
  end

  defp worker(i, _size, iters, payload, parent) do
    path = "/tmp/gp_#{i}.bin"

    acc =
      Enum.reduce(1..iters, %{mismatches: 0, enospc: 0, other_err: 0, ok: 0}, fn _n, acc ->
        result =
          try do
            :ok = File.write(path, payload)
            back = File.read!(path)
            _ = File.rm(path)
            # `===` structural equality — the honest byte-exact check. A mismatch
            # here IS the silent corruption (a clobbered binary), not an md5 fluke.
            if back === payload, do: :ok, else: :mismatch
          rescue
            e in File.Error ->
              case e do
                %File.Error{reason: :enospc} -> :enospc
                _ -> :other_err
              end
            _ -> :other_err
          catch
            _, _ -> :other_err
          end

        Map.update!(acc, key_of(result), &(&1 + 1))
      end)

    send(parent, {:done, i, acc})
  end

  defp key_of(:ok), do: :ok
  defp key_of(:mismatch), do: :mismatches
  defp key_of(:enospc), do: :enospc
  defp key_of(_), do: :other_err

  defp collect(0, tot) do
    IO.puts("GP_REPRO: DONE ok=#{tot.ok} mismatches=#{tot.mismatches} " <>
            "enospc=#{tot.enospc} other_err=#{tot.other_err}")
    verdict =
      cond do
        tot.mismatches > 0 -> "GP_REPRO: FAIL — #{tot.mismatches} SILENT CORRUPTIONS (readback != written)"
        # NOTE: deliberately does NOT contain the literal fault string the kernel
        # prints (the gpf handler emits "#GP ip=<hex> rsp=..."). An earlier version
        # said "check serial for any '#GP ip='" and a count-grep matched THIS line —
        # a false positive that read as "fault on both trees". Keep this benign.
        true -> "GP_REPRO: PASS — no corruption, node survived (inspect serial for a real GPF dump)"
      end
    IO.puts(verdict)
    tot
  end

  defp collect(n, tot) do
    receive do
      {:done, _i, acc} ->
        merged = Map.merge(tot, acc, fn _k, a, b -> a + b end)
        collect(n - 1, merged)
    after
      180_000 ->
        IO.puts("GP_REPRO: TIMEOUT — #{n} workers never reported (possible crash/wedge); tot=#{inspect(tot)}")
        tot
    end
  end
end
