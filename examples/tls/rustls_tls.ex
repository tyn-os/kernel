# In-guest inbound TLS transport for Bandit/ThousandIsland, backed by the tyn_tls
# rustls NIF. Config-only wire-in: scheme: :https + transport_module override.
# BEAM owns the kernel TCP socket; the NIF only transforms bytes (sans-IO).
defmodule Tyn.Transports.RustlsTLS do
  @behaviour ThousandIsland.Transport
  import Kernel, except: [send: 2]
  @hs_timeout 5000

  @impl true
  def listen(port, opts) do
    {cert_der, key_der} = load_cert_key(opts)
    {:ok, cfg} = :tyn_tls.config_new(cert_der, key_der)
    ip = Keyword.get(opts, :ip, {0, 0, 0, 0})
    {:ok, lsock} =
      :gen_tcp.listen(port, [:binary, {:active, false}, {:reuseaddr, true}, {:backlog, 1024}, {:nodelay, true}, {:ip, ip}])
    :persistent_term.put({__MODULE__, :cfg, lsock}, cfg)
    {:ok, lsock}
  end

  @impl true
  def accept(lsock) do
    case :gen_tcp.accept(lsock) do
      {:ok, raw} -> {:ok, {:tyn_s, raw, :persistent_term.get({__MODULE__, :cfg, lsock})}}
      err -> err
    end
  end

  @impl true
  def handshake({:tyn_s, raw, cfg}) do
    :inet.setopts(raw, active: false)
    case :tyn_tls.conn_new(cfg) do
      {:ok, conn} -> do_hs(raw, conn)
      other -> {:error, other}
    end
  end

  defp do_hs(raw, conn) do
    flush(raw, conn)
    if :tyn_tls.is_handshaking(conn) do
      case :gen_tcp.recv(raw, 0, @hs_timeout) do
        {:ok, data} ->
          case :tyn_tls.feed(conn, data) do
            :ok -> do_hs(raw, conn)
            {:error, e} -> flush(raw, conn); {:error, e}
          end

        {:error, e} -> {:error, e}
      end
    else
      flush(raw, conn)
      {:ok, {:tyn_s, raw, conn}}
    end
  end

  @impl true
  def upgrade(_socket, _opts), do: {:error, :notsup}

  @impl true
  def controlling_process({:tyn_s, raw, _}, pid), do: :gen_tcp.controlling_process(raw, pid)

  @impl true
  def recv({:tyn_s, raw, conn}, n, timeout), do: do_recv(raw, conn, n, timeout, <<>>)

  defp do_recv(raw, conn, 0, timeout, _acc) do
    case :tyn_tls.read_plain(conn, 0) do
      <<>> ->
        case :gen_tcp.recv(raw, 0, timeout) do
          {:ok, ct} ->
            case :tyn_tls.feed(conn, ct) do
              :ok -> flush(raw, conn); do_recv(raw, conn, 0, timeout, <<>>)
              {:error, e} -> {:error, e}
            end

          {:error, e} -> {:error, e}
        end

      p -> {:ok, p}
    end
  end

  defp do_recv(raw, conn, n, timeout, acc) when n > 0 do
    acc = acc <> :tyn_tls.read_plain(conn, n - byte_size(acc))
    if byte_size(acc) >= n do
      {:ok, acc}
    else
      case :gen_tcp.recv(raw, 0, timeout) do
        {:ok, ct} ->
          case :tyn_tls.feed(conn, ct) do
            :ok -> flush(raw, conn); do_recv(raw, conn, n, timeout, acc)
            {:error, e} -> {:error, e}
          end

        {:error, e} -> {:error, e}
      end
    end
  end

  @impl true
  def send({:tyn_s, raw, conn}, data) do
    case :tyn_tls.write_plain(conn, :erlang.iolist_to_binary(data)) do
      :ok -> flush(raw, conn)
      {:error, e} -> {:error, e}
    end
  end

  @impl true
  def sendfile({:tyn_s, _raw, _conn} = sock, filename, offset, length) do
    case File.open(filename, [:read, :binary]) do
      {:ok, io} ->
        {:ok, _} = :file.position(io, offset)
        {:ok, data} = :file.read(io, length)
        File.close(io)
        case send(sock, data) do
          :ok -> {:ok, byte_size(data)}
          err -> err
        end

      err -> err
    end
  end

  defp flush(raw, conn) do
    case :tyn_tls.pull_send(conn) do
      <<>> -> :ok
      out -> :gen_tcp.send(raw, out)
    end
  end

  @impl true
  def getopts({:tyn_s, raw, _}, opts), do: :inet.getopts(raw, opts)
  @impl true
  def setopts({:tyn_s, raw, _}, opts), do: :inet.setopts(raw, opts)
  @impl true
  def shutdown({:tyn_s, raw, _}, way), do: :gen_tcp.shutdown(raw, way)

  @impl true
  def close({:tyn_s, raw, conn}) do
    :tyn_tls.close_conn(conn)
    :gen_tcp.close(raw)
  end

  def close(lsock), do: :gen_tcp.close(lsock)

  @impl true
  def sockname({:tyn_s, raw, _}), do: :inet.sockname(raw)
  def sockname(lsock), do: :inet.sockname(lsock)
  @impl true
  def peername({:tyn_s, raw, _}), do: :inet.peername(raw)
  @impl true
  def peercert(_), do: {:error, :no_peercert}
  @impl true
  def secure?, do: true
  @impl true
  def getstat({:tyn_s, raw, _}), do: :inet.getstat(raw)
  @impl true
  def negotiated_protocol(_), do: {:error, :protocol_not_negotiated}
  @impl true
  def connection_information(_), do: {:error, :notsup}

  defp load_cert_key(opts) do
    cond do
      opts[:cert] && opts[:key] -> {opts[:cert], opts[:key]}
      opts[:certfile] && opts[:keyfile] ->
        {pem_to_der(File.read!(opts[:certfile])), pem_to_der(File.read!(opts[:keyfile]))}
      true ->
        raise "RustlsTLS: need certfile/keyfile (or cert/key DER) in transport_options"
    end
  end

  defp pem_to_der(pem) do
    pem
    |> String.split("\n")
    |> Enum.reject(&(String.starts_with?(&1, "-----") or &1 == ""))
    |> Enum.join("")
    |> Base.decode64!()
  end
end
