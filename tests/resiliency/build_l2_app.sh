#!/bin/bash
# Assemble the Layer-2 adversarial target app: a Bandit HTTP server (:8080) that
# is the victim for the slow-loris + fd-exhaustion attacks, AND runs the tmpfs
# cap-under-concurrency probe at boot (CC_ lines to serial). One app = one Nitro
# instance covers all three Layer-2 checks. Clean-clone reproducible: generates
# the app, drops in the committed probe module (../tmpfs/probe_cap.ex).
# Run on the build host (asdf Elixir/OTP toolchain).
set -e
export PATH="$HOME/.asdf/shims:$PATH"
. ~/.asdf/asdf.sh
export MIX_ENV=prod
HERE="$(cd "$(dirname "$0")" && pwd)"
cd /home/ubuntu
rm -rf l2app
mix new l2app --sup >/dev/null 2>&1
cd l2app
rm -f lib/l2app.ex   # remove generated module; we own our names

cat > mix.exs <<"EOF"
defmodule L2app.MixProject do
  use Mix.Project
  def project, do: [app: :l2app, version: "0.1.0", elixir: "~> 1.18", start_permanent: true, deps: deps()]
  def application, do: [extra_applications: [:logger], mod: {L2app.Application, []}]
  defp deps, do: [{:bandit, "~> 1.5"}, {:plug, "~> 1.16"}]
end
EOF

cat > lib/l2app/application.ex <<"EOF"
defmodule L2app.Application do
  use Application
  @impl true
  def start(_type, _args) do
    children = [
      # The attack victim: a plain HTTP server. /health returns a tiny known body
      # so an attacker-side client can prove legitimate traffic still flows during
      # and after an attack (the recovery assertion).
      {Bandit, plug: L2app.Router, scheme: :http, port: 8080},
      # The in-guest tmpfs cap-under-concurrency probe (CC_ lines to serial).
      %{id: L2app.Cap, start: {Task, :start_link, [&CapProbe.run/0]}, restart: :temporary}
    ]
    Supervisor.start_link(children, strategy: :one_for_one, name: L2app.Supervisor)
  end
end
EOF

cat > lib/router.ex <<"EOF"
defmodule L2app.Router do
  use Plug.Router
  plug :match
  plug :dispatch
  get "/health", do: send_resp(conn, 200, "L2_OK")
  get "/", do: send_resp(conn, 200, "L2_OK root")
  match _, do: send_resp(conn, 404, "nf")
end
EOF

cp "$HERE/../tmpfs/probe_cap.ex" lib/probe_cap.ex

echo "=== deps.get ==="
mix deps.get 2>&1 | tail -2
echo "=== release ==="
mix release 2>&1 | tail -3
ls -d _build/prod/rel/l2app >/dev/null 2>&1 && echo "RELEASE_OK: /home/ubuntu/l2app/_build/prod/rel/l2app"
