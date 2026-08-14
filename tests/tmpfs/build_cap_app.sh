#!/bin/bash
# Assemble the minimal cap-probe app (no Bandit/Plug/Ecto — pure in-guest FS
# concurrency) and build a prod release. Clean-clone reproducible: it generates
# the whole app, then drops in the committed probe module (probe_cap.ex next to
# this script). Run on the build host (needs the asdf Elixir/OTP toolchain).
set -e
export PATH="$HOME/.asdf/shims:$PATH"
. ~/.asdf/asdf.sh
export MIX_ENV=prod
HERE="$(cd "$(dirname "$0")" && pwd)"
cd /home/ubuntu
rm -rf cap_probe
mix new cap_probe --sup >/dev/null 2>&1
cd cap_probe

# mix new --sup generates lib/cap_probe.ex defining `CapProbe` — remove it; our
# probe_cap.ex owns that module name (a duplicate defmodule would not compile).
rm -f lib/cap_probe.ex

cat > mix.exs <<"EOF"
defmodule CapProbe.MixProject do
  use Mix.Project
  def project, do: [app: :cap_probe, version: "0.1.0", elixir: "~> 1.18", start_permanent: true, deps: []]
  def application, do: [extra_applications: [:logger], mod: {CapProbe.Application, []}]
end
EOF

cat > lib/cap_probe/application.ex <<"EOF"
defmodule CapProbe.Application do
  use Application
  @impl true
  def start(_type, _args) do
    # One temporary Task runs the adversarial cap probe at boot and prints CC_
    # lines. No HTTP server — this is in-guest FS concurrency only.
    children = [
      %{id: CapProbe.Task, start: {Task, :start_link, [&CapProbe.run/0]}, restart: :temporary}
    ]
    Supervisor.start_link(children, strategy: :one_for_one, name: CapProbe.Supervisor)
  end
end
EOF

cp "$HERE/probe_cap.ex" lib/probe_cap.ex

echo "=== release ==="
mix release 2>&1 | tail -4
ls -d _build/prod/rel/cap_probe >/dev/null 2>&1 && echo "RELEASE_OK: /home/ubuntu/cap_probe/_build/prod/rel/cap_probe"
