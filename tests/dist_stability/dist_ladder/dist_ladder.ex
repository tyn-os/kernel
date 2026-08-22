defmodule DistLadder do
  @moduledoc """
  Remote target + helpers for the dist-stability ladder (DIST_STABILITY_HUNT).

  The diagnosis is banked: the 2-node cluster FORMS ({packet,2} handshake ok) but
  no term frame traverses the connected-phase data path (dist_ctrl_*) — rt=0 and
  the peer drops at one net_ticktime. This harness drives that data path directly
  to characterize it (tiny vs 1MB rpc; idle at low ticktime).

  ## The traversal detector has TEETH by construction
  `echo/1` returns its argument verbatim. The caller (`/rpc`) sends a fresh random
  payload and matches the reply against the EXACT bytes with a pinned `^payload`.
  So "traversed + byte-exact" is a structural match on the actual data — it CANNOT
  match its own log line or a probe string (the grep-false-positive class). A
  timeout reads as no-traversal; a returned-but-different binary reads as
  traversed-but-corrupt. Three distinct, real outcomes.
  """

  @doc "Remote echo — the round-trip target. Identity."
  def echo(term), do: term

  @doc """
  Lower net_ticktime so the idle-stability test is fast: if the data path is dead,
  even the tick can't traverse and the peer drops in ~ticktime seconds. Both nodes
  must agree within a 1/4 window; setting the same value on both at boot is fine.
  """
  def set_ticktime(secs) when is_integer(secs) do
    :net_kernel.set_net_ticktime(secs, 0)
  end
end
