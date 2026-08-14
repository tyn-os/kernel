defmodule CapProbe do
  # Phase-2 Layer-2 adversarial fixture: the tmpfs 4 MiB byte-cap under
  # CONCURRENT writers racing the boundary. This is the Layer-1 deferral closed
  # in situ — Layer 1 unit-tested grant_write's arithmetic on a slice; this tests
  # the *live node store* (src/tmpfs.rs, one coarse Mutex over nodes+total+open)
  # under real concurrent load, where an accounting race would let total exceed
  # the cap or interleave two writers' bytes into one file.
  #
  # probe_a.ex already covers the two NON-adversarial neighbors: (7) N concurrent
  # writes to DISTINCT small files (no cap pressure) and (8) a SINGLE-writer
  # over-cap ENOSPC. The gap between them — many writers contending the cap at
  # once — is exactly what this stresses.
  #
  # Demand is 2x the cap (N * chunk = 16 * 512 KiB = 8 MiB vs 4 MiB) so the cap
  # is *guaranteed* contended: some writers must win, some must hit ENOSPC.
  #
  # Prints machine-greppable CC_ lines. The three-part assertion (drive script):
  #   (1) clean handling — every result is :ok or {:error,:enospc}, node alive
  #   (2) no corruption   — total never exceeds the cap AND no file holds a
  #                         foreign writer's bytes (interleave detector)
  #   (3) recovery        — after freeing space, a fresh write succeeds byte-exact

  @cap 4 * 1024 * 1024
  @n 16
  @chunk 512 * 1024

  # true iff every byte of `bin` equals `byte` — a foreign byte means two
  # concurrent writers interleaved into one file (the corruption this catches).
  defp only_byte?(bin, byte), do: bin == :binary.copy(<<byte>>, byte_size(bin))

  def run do
    # Let boot settle. If probe_a ran first it leaves /tmp empty (it rm's all its
    # files), so the cap starts fully free and this owns the whole budget.
    Process.sleep(2500)
    IO.puts("CC_BEGIN cap=#{@cap} n=#{@n} chunk=#{@chunk} demand=#{@n * @chunk}")

    # Each writer i fills /tmp/cc_i.bin with byte value (i rem 256). File.write
    # (not write!) so ENOSPC comes back as {:error,:enospc}, not a raise.
    writers =
      1..@n
      |> Enum.map(fn i ->
        Task.async(fn ->
          byte = rem(i, 256)
          path = "/tmp/cc_#{i}.bin"
          res = File.write(path, :binary.copy(<<byte>>, @chunk))
          {i, byte, path, res}
        end)
      end)
      |> Enum.map(&Task.await(&1, 30_000))

    oks = Enum.count(writers, fn {_, _, _, r} -> r == :ok end)
    enospc = Enum.count(writers, fn {_, _, _, r} -> match?({:error, :enospc}, r) end)
    others = Enum.reject(writers, fn {_, _, _, r} -> r == :ok or match?({:error, :enospc}, r) end)

    # (1) clean handling: only :ok / {:error,:enospc} — anything else is a fail.
    IO.puts("CC_RESULTS ok=#{oks} enospc=#{enospc} other=#{length(others)} other_detail=#{inspect(others, limit: 5)}")

    # (2a) invariant: accounted bytes on disk never exceed the cap (the race the
    # coarse Mutex must serialize). Sum the real stat sizes of every cc_ file.
    files = File.ls!("/tmp") |> Enum.filter(&String.starts_with?(&1, "cc_"))
    total = files |> Enum.map(fn f -> File.stat!("/tmp/#{f}").size end) |> Enum.sum()
    IO.puts("CC_TOTAL bytes=#{total} cap=#{@cap} within_cap=#{total <= @cap}")

    # (2b) no corruption: every file that exists (full OR partial from a straddle
    # write) must contain ONLY its own writer's byte. A foreign byte == interleave.
    corrupt =
      writers
      |> Enum.filter(fn {_, _, path, _} -> File.exists?(path) end)
      |> Enum.reduce([], fn {i, byte, path, _}, acc ->
        if only_byte?(File.read!(path), byte), do: acc, else: [{i, File.stat!(path).size} | acc]
      end)
    IO.puts("CC_CORRUPT count=#{length(corrupt)} detail=#{inspect(corrupt, limit: 5)}")

    # teeth: the cap is only genuinely contended if some writers won AND some hit
    # ENOSPC. All-win would mean demand didn't exceed the cap (test too weak);
    # all-lose would mean nothing fit (something else wrong).
    IO.puts("CC_TEETH contended=#{oks > 0 and enospc > 0}")

    # (3) recovery: free everything, then a fresh 1 MiB write must succeed and
    # read back byte-exact — the store recovered and freed bytes are reusable.
    Enum.each(files, fn f -> File.rm("/tmp/#{f}") end)

    recov =
      try do
        d = :binary.copy(<<7>>, 1024 * 1024)
        :ok = File.write!("/tmp/cc_recov.bin", d)
        back = File.read!("/tmp/cc_recov.bin")
        _ = File.rm("/tmp/cc_recov.bin")
        back == d
      rescue
        e -> {:rescue, Exception.message(e)}
      catch
        k, v -> {:catch, k, inspect(v)}
      end

    IO.puts("CC_RECOVERY ok=#{inspect(recov)}")
    IO.puts("CC_END node_alive=#{Process.alive?(self())}")
  end
end
