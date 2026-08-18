# Loader for Tyn's in-guest inbound TLS NIF (tyn_tls), statically linked into
# beam.smp. load_nif resolves the static tyn_tls_nif_init from the NIF table.
defmodule :tyn_tls do
  @on_load :init
  def init, do: :erlang.load_nif(~c"tyn_tls", 0)

  def config_new(_cert_der, _key_der), do: :erlang.nif_error(:nif_not_loaded)
  def conn_new(_cfg), do: :erlang.nif_error(:nif_not_loaded)
  def feed(_conn, _ciphertext), do: :erlang.nif_error(:nif_not_loaded)
  def pull_send(_conn), do: :erlang.nif_error(:nif_not_loaded)
  def read_plain(_conn, _n), do: :erlang.nif_error(:nif_not_loaded)
  def write_plain(_conn, _plaintext), do: :erlang.nif_error(:nif_not_loaded)
  def is_handshaking(_conn), do: :erlang.nif_error(:nif_not_loaded)
  def close_conn(_conn), do: :erlang.nif_error(:nif_not_loaded)
end
