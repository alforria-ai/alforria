// Connecting a provider: a browser sign-in where the server offers one
// (LibertAI), or an API key. Either way the credential lands in the server's
// auth.json; the browser never keeps it.
import { createSignal, For, Match, onCleanup, Show, Switch } from "solid-js"
import { api, ApiError, type AuthMethod, type OAuthAuthorization } from "../api/client"
import { Icon } from "../ui/icons"
import { DialogHead, dialogId, Modal } from "../ui/modal"
import { toast } from "./toast"

export interface ConnectTarget {
  id: string
  name: string
  env: string[]
}

/** Where a provider hands out keys, for the API-key note. */
const KEY_PAGES: Record<string, string> = {
  libertai: "https://console.libertai.io",
}

type Flow =
  | { step: "choose" }
  | { step: "starting"; method: number }
  | { step: "waiting"; method: number; auth: OAuthAuthorization; opened: boolean }
  | { step: "finishing"; method: number; auth: OAuthAuthorization; opened: boolean }
  | { step: "failed"; method: number; message: string }

function describe(err: unknown) {
  const name =
    err instanceof ApiError && typeof err.body === "object" && err.body ? (err.body as { name?: string }).name : ""
  if (name === "ProviderAuthOauthCallbackFailed")
    return "Sign-in didn't complete. It was refused or timed out, or the pasted address belongs to another attempt."
  if (name === "ProviderAuthOauthMissing") return "This sign-in attempt has expired."
  return err instanceof Error ? err.message : String(err)
}

export function ConnectDialog(props: {
  provider: ConnectTarget
  methods: AuthMethod[]
  directory?: string
  onClose: () => void
  onConnected: () => void
}) {
  const title = dialogId()
  const pasteId = dialogId()
  const keyId = dialogId()
  const oauth = () => props.methods.flatMap((m, i) => (m.type === "oauth" ? [{ ...m, i }] : []))
  const takesKey = () => !props.methods.length || props.methods.some((m) => m.type === "api")
  const [flow, setFlow] = createSignal<Flow>({ step: "choose" })
  const [key, setKey] = createSignal("")
  const [saving, setSaving] = createSignal(false)
  const [pasted, setPasted] = createSignal("")
  let waiting: AbortController | undefined
  let done = false
  onCleanup(() => waiting?.abort())

  const finish = () => {
    if (done) return
    done = true
    waiting?.abort()
    toast(`${props.provider.name} connected`)
    props.onConnected()
    props.onClose()
  }
  const fail = (method: number, err: unknown) => {
    if (done) return
    waiting?.abort()
    setFlow({ step: "failed", method, message: describe(err) })
  }

  const signIn = async (method: number) => {
    // Open the tab inside the click so popup blockers let it through, then
    // point it at the sign-in page once the server has one.
    const tab = window.open("", "_blank")
    setFlow({ step: "starting", method })
    setPasted("")
    let auth: OAuthAuthorization | null | undefined
    try {
      auth = await api.oauthAuthorize(props.provider.id, method, props.directory)
      if (!auth) throw new Error("The server offered no sign-in page for this method.")
    } catch (err) {
      tab?.close()
      return fail(method, err)
    }
    let opened = false
    if (tab && !tab.closed) {
      tab.opener = null
      tab.location.href = auth.url
      opened = true
    }
    setFlow({ step: "waiting", method, auth, opened })
    if (auth.method !== "auto") return
    const ctl = (waiting = new AbortController())
    api.oauthCallback(props.provider.id, method, { directory: props.directory, signal: ctl.signal }).then(
      () => finish(),
      (err) => !ctl.signal.aborted && fail(method, err),
    )
  }

  const submitPasted = async () => {
    const f = flow()
    const code = pasted().trim()
    if (f.step !== "waiting" || !code) return
    setFlow({ ...f, step: "finishing" })
    try {
      await api.oauthCallback(props.provider.id, f.method, { code, directory: props.directory })
      finish()
    } catch (err) {
      fail(f.method, err)
    }
  }

  const cancel = () => {
    waiting?.abort()
    setFlow({ step: "choose" })
  }

  const saveKey = async () => {
    const value = key().trim()
    if (!value || saving()) return
    setSaving(true)
    try {
      await api.setAuth(props.provider.id, value)
      finish()
    } catch (err) {
      toast(`Could not save: ${describe(err)}`, "error")
    } finally {
      setSaving(false)
    }
  }

  const SignInPage = (p: { auth: OAuthAuthorization; opened: boolean }) => (
    <a class="link-btn" href={p.auth.url} target="_blank" rel="noopener noreferrer">
      {p.opened ? "Open the sign-in page again" : "Open the sign-in page"}
      <Icon name="external" />
    </a>
  )

  return (
    <Modal
      class="connect"
      labelledBy={title}
      onClose={props.onClose}
      initialFocus={(panel) =>
        panel.querySelector<HTMLElement>(".dialog-body :is(button, input):not([disabled])") ?? undefined
      }
    >
      <DialogHead id={title} title={`Connect ${props.provider.name}`} onClose={props.onClose} />
      <div class="dialog-body">
        <Switch>
          <Match when={flow().step === "choose" || flow().step === "starting"}>
            <Show when={oauth().length}>
              <div class="connect-methods">
                <For each={oauth()}>
                  {(m) => (
                    <button
                      class="stamp primary"
                      disabled={flow().step === "starting"}
                      onClick={() => void signIn(m.i)}
                    >
                      {flow().step === "starting" ? "Opening sign-in…" : m.label}
                      <Icon name="external" />
                    </button>
                  )}
                </For>
              </div>
              <p class="connect-note">
                Opens a {props.provider.name} page in a new tab. The key it issues is stored on the server, never in
                this browser.
              </p>
            </Show>
            <Show when={takesKey()}>
              <Show when={oauth().length}>
                <div class="connect-or">
                  <span>or use an API key</span>
                </div>
              </Show>
              <form
                class="connect-field"
                onSubmit={(e) => {
                  e.preventDefault()
                  void saveKey()
                }}
              >
                <label for={keyId}>API key</label>
                <div class="field-row">
                  <input
                    id={keyId}
                    class="other-input"
                    type="password"
                    autocomplete="off"
                    spellcheck={false}
                    value={key()}
                    onInput={(e) => setKey(e.currentTarget.value)}
                  />
                  <button class="stamp" type="submit" disabled={!key().trim() || saving()}>
                    {saving() ? "Saving…" : "Save key"}
                  </button>
                </div>
                <p class="connect-note">
                  <Show when={KEY_PAGES[props.provider.id]}>
                    {(href) => (
                      <>
                        Create one at{" "}
                        <a href={href()} target="_blank" rel="noopener noreferrer">
                          {href().replace(/^https?:\/\//, "")}
                        </a>
                        .{" "}
                      </>
                    )}
                  </Show>
                  <Show when={props.provider.env[0]}>
                    {(env) => (
                      <>
                        Or set <code>{env()}</code> where the server runs.
                      </>
                    )}
                  </Show>
                </p>
              </form>
            </Show>
          </Match>

          <Match
            when={(() => {
              const f = flow()
              return f.step === "waiting" || f.step === "finishing" ? f : undefined
            })()}
          >
            {(f) => (
              <div class="connect-wait" tabindex="-1" ref={(el) => queueMicrotask(() => el.focus())}>
                <Show
                  when={f().auth.method === "auto"}
                  fallback={<p class="connect-status">Sign in, then paste the code you're given</p>}
                >
                  <p class="connect-status" role="status">
                    <span class="lamp working" />
                    Waiting for you to sign in
                  </p>
                </Show>
                <p class="connect-instr">{f().auth.instructions}</p>
                <SignInPage auth={f().auth} opened={f().opened} />
                <form
                  class="connect-field"
                  onSubmit={(e) => {
                    e.preventDefault()
                    void submitPasted()
                  }}
                >
                  <label for={pasteId}>
                    {f().auth.method === "auto"
                      ? "Browser on another machine? Once you've signed in, its tab tries to load a 127.0.0.1 address and fails. Copy that address here."
                      : "Code"}
                  </label>
                  <div class="field-row">
                    <input
                      id={pasteId}
                      class="other-input mono"
                      autocomplete="off"
                      spellcheck={false}
                      placeholder={f().auth.method === "auto" ? "http://127.0.0.1:…/callback?code=…" : ""}
                      value={pasted()}
                      onInput={(e) => setPasted(e.currentTarget.value)}
                    />
                    <button class="stamp" type="submit" disabled={!pasted().trim() || f().step === "finishing"}>
                      {f().step === "finishing" ? "Finishing…" : "Finish"}
                    </button>
                  </div>
                </form>
                <div class="connect-foot">
                  <button class="link-btn" onClick={cancel}>
                    Cancel sign-in
                  </button>
                </div>
              </div>
            )}
          </Match>

          <Match
            when={(() => {
              const f = flow()
              return f.step === "failed" ? f : undefined
            })()}
          >
            {(f) => (
              <div class="connect-wait" tabindex="-1" ref={(el) => queueMicrotask(() => el.focus())}>
                <p class="connect-status fault" role="alert">
                  <span class="lamp fault" />
                  Not connected
                </p>
                <p class="connect-instr">{f().message}</p>
                <div class="connect-foot">
                  <button class="stamp primary" onClick={() => void signIn(f().method)}>
                    Try again
                  </button>
                  <button class="link-btn" onClick={cancel}>
                    Back
                  </button>
                </div>
              </div>
            )}
          </Match>
        </Switch>
      </div>
    </Modal>
  )
}
