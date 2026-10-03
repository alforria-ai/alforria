// What the client knows about the server it is attached to. In LibertAI cloud
// mode this is the slot a machine switcher replaces.
import { createStore } from "solid-js/store"
import { api } from "../api/client"

export const [server, setServer] = createStore({
  address: typeof location !== "undefined" ? location.host : "",
  version: "",
  home: "",
})

export async function loadServerInfo() {
  const [health, path] = await Promise.all([api.health().catch(() => undefined), api.path().catch(() => undefined)])
  if (health) setServer("version", health.version)
  if (path) setServer("home", path.home)
}
