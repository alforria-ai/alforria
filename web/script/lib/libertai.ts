// A stand-in for LibertAI's console sign-in page and account API, so the dev
// server and the e2e suite exercise "Sign in with LibertAI" end to end
// without a real account. `alforria serve` reaches it through the
// LIBERTAI_CONSOLE_URL / LIBERTAI_ACCOUNT_BASE overrides.
//
// The console page checks what the real one checks (a loopback redirect, a
// state and a challenge) and hands the code back the same way; the exchange
// verifies the PKCE verifier against the challenge it was issued for.

import { createHash, randomBytes } from "node:crypto"

const LOOPBACK = new Set(["127.0.0.1", "localhost", "::1", "[::1]"])
const b64url = (buf: Buffer) => buf.toString("base64url")

export function startLibertai(port: number) {
  const challenges = new Map<string, string>()
  const issued: string[] = []

  const server = Bun.serve({
    port,
    hostname: "127.0.0.1",
    async fetch(req) {
      const url = new URL(req.url)
      if (req.method === "GET" && url.pathname === "/cli") {
        const redirect = url.searchParams.get("redirect_uri") ?? ""
        const state = url.searchParams.get("state") ?? ""
        const challenge = url.searchParams.get("challenge") ?? ""
        let target: URL | undefined
        try {
          target = new URL(redirect)
        } catch {}
        if (!target || !LOOPBACK.has(target.hostname) || !state || !challenge)
          return new Response("Invalid sign-in request", { status: 400 })
        const code = b64url(randomBytes(12))
        challenges.set(code, challenge)
        target.searchParams.set("code", code)
        target.searchParams.set("state", state)
        const href = target.toString().replace(/"/g, "&quot;")
        return new Response(
          `<!doctype html><meta charset="utf-8"><title>LibertAI (dev stand-in)</title>
<style>body{font:15px system-ui;background:#0b0e10;color:#e7ecef;display:grid;place-items:center;height:100vh;margin:0}
a{display:inline-block;margin-top:16px;padding:12px 18px;background:#e7ecef;color:#0b0e10;text-decoration:none;font-weight:600}</style>
<main><h1>LibertAI · dev stand-in</h1><p>${url.searchParams.get("client") ?? "A client"} wants to sign in as <b>dev@libertai.test</b>.</p>
<a id="approve" href="${href}">Approve</a></main>`,
          { headers: { "content-type": "text/html; charset=utf-8" } },
        )
      }
      if (req.method === "POST" && url.pathname === "/auth/exchange") {
        const body = (await req.json().catch(() => ({}))) as { code?: string; verifier?: string }
        const challenge = body.code ? challenges.get(body.code) : undefined
        const ok =
          challenge && body.verifier && b64url(createHash("sha256").update(body.verifier).digest()) === challenge
        if (!ok) return Response.json({ detail: "invalid code or verifier" }, { status: 400 })
        challenges.delete(body.code!)
        return Response.json({
          access_token: `at_${b64url(randomBytes(9))}`,
          refresh_token: `rt_${b64url(randomBytes(9))}`,
        })
      }
      if (req.method === "POST" && url.pathname === "/api-keys/cli") {
        if (!req.headers.get("authorization")?.startsWith("Bearer at_"))
          return Response.json({ detail: "unauthorized" }, { status: 401 })
        const { host } = (await req.json().catch(() => ({}))) as { host?: string }
        const key = `LTAI_dev_${b64url(randomBytes(12))}`
        issued.push(key)
        const expires = new Date(Date.now() + 30 * 86_400_000).toISOString()
        return Response.json({
          id: `key_${issued.length}`,
          name: `cli-${host ?? "device"}`,
          full_key: key,
          expires_at: expires,
        })
      }
      if (req.method === "POST" && url.pathname === "/auth/logout") return Response.json({ ok: true })
      return new Response("not found", { status: 404 })
    },
  })
  return {
    url: `http://127.0.0.1:${server.port}`,
    issued,
    env: (): Record<string, string> => ({
      LIBERTAI_CONSOLE_URL: `http://127.0.0.1:${server.port}`,
      LIBERTAI_ACCOUNT_BASE: `http://127.0.0.1:${server.port}`,
    }),
    stop: () => server.stop(true),
  }
}
