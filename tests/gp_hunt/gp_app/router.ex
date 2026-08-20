defmodule GpApp.Router do
  @moduledoc "Boot-detection /health + the stored GP_REPRO result at /gp."
  use Plug.Router

  plug(:match)
  plug(:dispatch)

  get "/health" do
    send_resp(conn, 200, "L2_OK")
  end

  get "/gp" do
    body =
      case :persistent_term.get(:gp_result, :running) do
        :running -> "GP: running (repro not finished yet)"
        r -> "GP: #{inspect(r)}"
      end

    send_resp(conn, 200, body)
  end

  match _ do
    send_resp(conn, 404, "not found")
  end
end
