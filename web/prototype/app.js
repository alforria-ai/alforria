// alforria web — fleet prototype. Vanilla JS over one in-memory store; the
// production client re-implements this in SolidJS against /global/event.
import * as D from "./data.js";

/* ───────────── helpers ───────────── */

const $ = (sel, root = document) => root.querySelector(sel);
const $$ = (sel, root = document) => [...root.querySelectorAll(sel)];
const esc = (s) =>
  String(s ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);
const icon = (name, cls = "") => `<svg class="i ${cls}" aria-hidden="true"><use href="#i-${name}"/></svg>`;
const reduced = () => matchMedia("(prefers-reduced-motion: reduce)").matches;
const mobile = () => innerWidth <= 760;
const rand = (a, b) => a + Math.random() * (b - a);
const pick = (xs) => xs[Math.floor(Math.random() * xs.length)];

function age(ms) {
  const s = Math.max(0, Math.floor(ms / 1000));
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m`;
  const h = Math.floor(m / 60);
  return h < 24 ? `${h}h` : `${Math.floor(h / 24)}d`;
}
function waited(ms) {
  const s = Math.max(0, Math.floor(ms / 1000));
  const m = Math.floor(s / 60);
  return m ? `${m}m ${String(s % 60).padStart(2, "0")}s` : `${s}s`;
}
function clock(t = Date.now()) {
  return new Date(t).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit", hour12: false });
}
function hm(t) {
  return new Date(t).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", hour12: false });
}
const money = (n) => `$${n.toFixed(2)}`;
const ktok = (n) => (n >= 1_000_000 ? `${(n / 1_000_000).toFixed(2)}M` : `${(n / 1000).toFixed(1)}k`);

const inline = (t) =>
  esc(t)
    .replace(/\*\*(.+?)\*\*/g, "<strong>$1</strong>")
    .replace(/`([^`]+)`/g, "<code>$1</code>");
function md(text) {
  return text
    .split(/\n\n+/)
    .map((block) => {
      const lines = block.split("\n");
      if (lines.every((l) => l.startsWith("- "))) return `<ul>${lines.map((l) => `<li>${inline(l.slice(2))}</li>`).join("")}</ul>`;
      return `<p>${inline(block)}</p>`;
    })
    .join("");
}

/* ───────────── store ───────────── */

const SES = new Map(D.sessions.map((s) => [s.id, structuredClone(s)]));
const PROJ = new Map(D.projects.map((p) => [p.id, p]));
const TRANSCRIPTS = new Map();
const transcriptOf = (sid) => {
  if (!TRANSCRIPTS.has(sid)) {
    const s = SES.get(sid);
    TRANSCRIPTS.set(sid, structuredClone(D.transcripts[sid] ?? (D.briefs[sid] ? synthTranscript(s) : D.fallbackTranscript(s))));
  }
  return TRANSCRIPTS.get(sid);
};

function synthTranscript(s) {
  const b = D.briefs[s.id];
  const parts = [];
  if (b.todos)
    parts.push({
      type: "todos",
      items: b.todos.map((t, i) => [t, i < s.todos[0] ? "completed" : i === s.todos[0] && s.state !== "idle" ? "in_progress" : "pending"]),
    });
  for (const st of b.subtasks ?? []) parts.push({ type: "subtask", ...st, result: null });
  for (const t of b.tools ?? []) parts.push({ type: "tool", status: "completed", ...t });
  if (s.state === "working") parts.push(livePart(s.now));
  if (s.state === "waiting") {
    const q = S.queue.find((x) => x.session === s.id);
    if (q) parts.push(pendingPart(q));
  }
  if (s.state === "retry") parts.push({ type: "retry", attempt: 2, reason: "Provider returned 429 Too Many Requests" });
  if (s.state === "idle" && b.outro) parts.push({ type: "text", text: b.outro });
  const at = s.since - 4 * 60_000;
  return [
    { role: "user", at, text: b.prompt },
    { role: "assistant", agent: s.agent, model: s.model, at, tokens: s.tokens, cost: s.cost, live: s.state === "working", parts },
  ];
}
function livePart(n) {
  if (n.kind === "writing") return { type: "text", text: n.text };
  if (n.kind === "thinking") return { type: "reasoning", seconds: 3, text: n.text };
  return { type: "tool", tool: n.kind, target: n.text, status: "running" };
}
function pendingPart(q) {
  if (q.kind === "question") return { type: "question", permission: q.id };
  return { type: "tool", tool: q.kind === "doom_loop" ? "read" : q.kind, target: q.command ?? q.file, status: "pending", permission: q.id };
}

const stored = (() => {
  try {
    return JSON.parse(localStorage.getItem("alforria.proto") || "{}");
  } catch {
    return {};
  }
})();

const S = {
  theme: stored.theme || "dark",
  view: "overview",
  filter: "all",
  queueOpen: stored.queueOpen ?? true,
  mobile: "queue",
  cursorId: null,
  focus: [],
  activeCol: 0,
  activeSlip: 0,
  queue: structuredClone(D.queue),
  resolved: {},
  stamping: new Set(),
  answers: {},
  ledger: [],
  settingsPage: "providers",
  scroll: {},
  stick: {},
  palette: { q: "", sel: 0, items: [] },
  lastFrame: Date.now(),
  started: Date.now(),
  stream: { s1: 0 },
  activityIdx: {},
  colSeq: 0,
};

function persist() {
  try {
    localStorage.setItem("alforria.proto", JSON.stringify({ theme: S.theme, queueOpen: S.queueOpen }));
  } catch {
    /* storage may be unavailable; preferences just won't persist */
  }
}

const app = $("#app");
document.documentElement.dataset.theme = S.theme;

const queueFor = (sid) => S.queue.find((q) => q.session === sid);
const childrenOf = (sid) => [...SES.values()].filter((s) => s.parent === sid);
const topLevelOf = (pid) => [...SES.values()].filter((s) => s.project === pid && !s.parent);
const counts = () => {
  const c = { waiting: 0, working: 0, idle: 0, all: SES.size };
  for (const s of SES.values()) {
    if (s.state === "waiting") c.waiting++;
    else if (s.state === "working" || s.state === "retry") c.working++;
    else c.idle++;
  }
  return c;
};
const passFilter = (s) =>
  S.filter === "all" ||
  (S.filter === "waiting" && s.state === "waiting") ||
  (S.filter === "working" && (s.state === "working" || s.state === "retry")) ||
  (S.filter === "idle" && (s.state === "idle" || s.state === "fault"));

function visibleRows() {
  const out = [];
  for (const p of D.projects) {
    const rows = [];
    for (const s of topLevelOf(p.id)) {
      if (passFilter(s)) rows.push(s);
      for (const c of childrenOf(s.id)) if (passFilter(c)) rows.push(c);
    }
    if (rows.length) out.push({ project: p, rows });
  }
  return out;
}
const flatVisible = () => visibleRows().flatMap((g) => g.rows.map((r) => r.id));

function announce(msg) {
  const el = $("#announce");
  el.textContent = "";
  requestAnimationFrame(() => (el.textContent = msg));
}

/* ───────────── band ───────────── */

function renderBand() {
  const n = S.queue.length;
  const oldest = n ? Math.min(...S.queue.map((q) => q.since ?? Date.now())) : null;
  const prev = $("#band .wait-count .n")?.textContent;
  $("#band").innerHTML = `
    <a class="wordmark" href="#/" aria-label="alforria, fleet overview" data-act="overview">alforria</a>
    <nav class="band-nav" id="bandNav" aria-label="Views"></nav>
    <button class="designation" data-act="server" title="Server">
      ${icon("server")}<span>SERVER</span><span class="addr">${esc(D.server.address)}</span><span class="dim">v${esc(D.server.version)}</span>
    </button>
    <button class="cmd" data-act="palette" aria-label="Search sessions, files and commands">
      ${icon("search")}<span>Search sessions, files, commands</span><kbd>⌘K</kbd>
    </button>
    <div class="band-actions">
      <button class="band-new" data-act="new" title="New session (N)">${icon("plus")}<span>New session</span><kbd>N</kbd></button>
      <button class="icon-btn theme" data-act="theme" aria-label="Switch to ${S.theme === "dark" ? "light" : "dark"} theme">${icon("contrast")}</button>
      <button class="icon-btn" data-act="settings" aria-label="Settings">${icon("sliders")}</button>
    </div>
    <button class="wait-count ${n ? "lit" : ""}" data-act="queue" aria-label="${n} waiting on you">
      <span class="n">${n}</span>
      <span class="w"><b>Waiting</b><small>${n ? `oldest ${waited(Date.now() - oldest)}` : "all clear"}</small></span>
    </button>`;
  if (prev != null && prev !== String(n) && !reduced()) $("#band .wait-count .n").classList.add("tick");
  renderBandNav();
}
function renderBandNav() {
  const nav = $("#bandNav");
  if (!nav) return;
  const n = S.focus.length;
  nav.innerHTML = `
    <button data-act="overview" aria-current="${S.view === "overview"}">${icon("rows")}Overview<kbd>Esc</kbd></button>
    <button data-act="focus" aria-current="${S.view === "focus"}" ${n ? "" : 'disabled title="Open a session first"'}>${icon("columns")}Focus${n ? `<span class="c">${n}</span>` : ""}<kbd>F</kbd></button>`;
}

/* ───────────── queue ───────────── */

function slipKind(q) {
  return { bash: "Permission · bash", edit: "Permission · edit", doom_loop: "Doom loop", question: "Question" }[q.kind] ?? q.kind;
}
function slipSummary(q) {
  if (q.kind === "bash") return `$ ${q.command}`;
  if (q.kind === "edit") return `${q.file}  +${q.add} −${q.del}`;
  if (q.kind === "doom_loop") return `${q.command}  ×3`;
  if (q.kind === "question") return q.question;
  return "";
}

function slipBody(q, inline = false) {
  const s = SES.get(q.session);
  const p = PROJ.get(s.project);
  const ctx = inline
    ? ""
    : `<div class="slip-ctx">
        <a class="session-link" href="#" data-open="${s.id}">${esc(s.title)}</a>
        <div class="meta">${esc(p.name)} · ${esc(s.agent)} · ${esc(s.model)}</div>
      </div>`;
  let body = "";
  let actions = "";
  if (q.kind === "question") {
    const chosen = S.answers[q.id];
    body = `<p class="q-text">${esc(q.question)}</p>
      <div class="options" role="radiogroup" aria-label="${esc(q.title)}">
        ${q.options
          .map(
            (o, i) => `<button class="option" role="radio" aria-checked="${chosen === i}" data-answer="${q.id}" data-i="${i}">
              <kbd class="k">${i + 1}</kbd><b>${esc(o.label)}</b><span>${esc(o.description)}</span></button>`,
          )
          .join("")}
      </div>
      <input class="other-input" data-other="${q.id}" placeholder="Or type your own answer…" aria-label="Custom answer" />`;
    actions = `<div class="slip-actions two">
        <button class="stamp primary" data-verdict="answer" data-q="${q.id}"><kbd>↵</kbd>Submit answer</button>
        <button class="stamp" data-verdict="dismiss" data-q="${q.id}"><kbd>X</kbd>Dismiss</button>
      </div>`;
  } else {
    if (q.kind === "bash") body = `<div class="plate cmdline">${esc(q.command)}</div>`;
    if (q.kind === "doom_loop")
      body = `<p>${esc(q.detail)}</p><div class="plate">${esc(q.command)}</div>`;
    if (q.kind === "edit") body = `<div class="diff">${diffLines(q.diff)}</div>`;
    const always = q.patterns?.[0] ?? "";
    body += `<div class="pattern-note">Always allow remembers <code>${esc(always)}</code> for this session.</div>`;
    const allowLabel = q.kind === "doom_loop" ? "Continue" : "Allow once";
    actions = `<div class="slip-actions">
        <button class="stamp primary" data-verdict="once" data-q="${q.id}"><kbd>A</kbd>${allowLabel}</button>
        <button class="stamp" data-verdict="always" data-q="${q.id}"><kbd>S</kbd>Always</button>
        <button class="stamp" data-verdict="reject" data-q="${q.id}"><kbd>D</kbd>${q.kind === "doom_loop" ? "Stop" : "Deny"}</button>
      </div>`;
  }
  return `${ctx}<div class="slip-body">${body}</div>${actions}`;
}

function slipHTML(q, i, { inline = false, arriving = false } = {}) {
  const active = inline || i === S.activeSlip;
  const s = SES.get(q.session);
  const since = q.since ?? Date.now();
  if (!active) {
    return `<div class="slip ${arriving ? "arriving" : ""}" data-slip="${q.id}" data-i="${i}" tabindex="-1">
      <div class="slip-head"><span class="lamp waiting"></span><span class="kind">${esc(slipKind(q))}</span><span class="age" data-since="${since}">${waited(Date.now() - since)}</span></div>
      <button class="slip-summary" data-expand="${i}">
        <span class="what ${q.kind === "question" ? "prose" : ""}">${esc(slipSummary(q))}</span>
        <span class="who">${esc(s.title)} · ${esc(PROJ.get(s.project).name)}</span>
      </button>
    </div>`;
  }
  return `<div class="slip is-active ${inline ? "inline" : ""} ${arriving ? "arriving" : ""}" data-slip="${q.id}" data-i="${i}" tabindex="0"
      role="group" aria-label="${esc(slipKind(q))} for ${esc(s.title)}">
    <div class="slip-head"><span class="lamp waiting"></span><span class="kind">${esc(slipKind(q))}</span><span class="age" data-since="${since}">${inline ? "waiting " : ""}${waited(Date.now() - since)}</span></div>
    ${slipBody(q, inline)}
  </div>`;
}

function renderSide(opts = {}) {
  if (S.view === "focus" && S.focus.length && !mobile()) renderRail();
  else renderQueue(opts);
}

function railOrder() {
  const out = [];
  for (const p of D.projects)
    for (const s of topLevelOf(p.id)) {
      out.push(s.id);
      for (const c of childrenOf(s.id)) out.push(c.id);
    }
  return out;
}
function railRowHTML(s) {
  const cols = S.focus.map((c, i) => (c.kind === "session" && c.session === s.id ? i : -1)).filter((i) => i >= 0);
  const active = cols.includes(S.activeCol);
  const n = s.now ?? { kind: "done", text: "" };
  const kind = s.state === "waiting" ? (n.kind === "question" ? "Asks" : "Approve") : s.state === "idle" ? "Done" : s.state === "fault" ? "Fault" : s.state === "retry" ? "Retry" : n.kind;
  const prose = ["writing", "thinking", "done", "question", "retry"].includes(n.kind);
  return `<div class="rail-row ${s.parent ? "sub" : ""} ${s.state} ${active ? "is-active" : ""} ${cols.length ? "is-open" : ""}" data-rail="${s.id}" role="option" aria-selected="${active}" tabindex="-1" title="${esc(s.title)}">
    <span class="lamp ${s.state}"></span>
    <span class="t">${esc(s.title)}</span>
    <span class="badges">${cols.map((i) => `<i class="${i === S.activeCol ? "on" : ""}" title="Column ${i + 1}">${i + 1}</i>`).join("")}</span>
    <span class="age-v" data-age="${s.id}">${age(Date.now() - s.since)}</span>
    <span class="now ${prose ? "prose" : ""}"><b>${esc(kind)}</b> ${esc(n.text)}</span>
    <button class="beside" data-beside="${s.id}" aria-label="Open ${esc(s.title)} beside" title="Open beside (⇧ click)">${icon("columns")}</button>
  </div>`;
}
function renderRail() {
  const n = S.queue.length;
  const c = counts();
  const oldest = n ? S.queue.reduce((a, q) => (q.since < a.since ? q : a)) : null;
  const groups = D.projects
    .map((p) => {
      const rows = railOrder().map((id) => SES.get(id)).filter((s) => s.project === p.id);
      if (!rows.length) return "";
      const w = rows.filter((s) => s.state === "waiting").length;
      return `<div class="rail-proj"><h3>${esc(p.name)}</h3><span class="tally">${rows.length}${w ? ` · <b>${w} waiting</b>` : ""}</span></div>${rows.map(railRowHTML).join("")}`;
    })
    .join("");
  $("#queue").innerHTML = `
    <div class="pane-head">
      <h2 id="railTitle">Sessions</h2><span class="count">${c.working} working · ${c.all} total</span>
      <span class="spacer"></span>
      <button class="icon-btn collapse" data-act="toggle-queue" aria-label="Collapse session list">${icon("panel")}</button>
    </div>
    ${n ? `<button class="rail-next" data-act="next-waiting"><span class="lamp waiting"></span><span><b>${n} waiting</b> · open ${esc(SES.get(oldest.session).title)}</span><kbd>W</kbd></button>` : ""}
    <div class="rail-list" role="listbox" aria-labelledby="railTitle">${groups}</div>
    <div class="rail-hint">Click to switch the active column · ⇧ click to open beside · <kbd>Alt</kbd><kbd>↑</kbd><kbd>↓</kbd></div>
    <div class="q-rail">
      <button class="icon-btn" data-act="toggle-queue" aria-label="Expand session list">${icon("panel")}</button>
      ${railOrder()
        .map((id) => SES.get(id))
        .map((s) => `<button class="mini ${S.focus[S.activeCol]?.session === s.id ? "on" : ""}" data-rail="${s.id}" title="${esc(s.title)} · ${s.state}"><span class="lamp ${s.state}"></span></button>`)
        .join("")}
    </div>`;
}

function renderQueue({ focusActive = false, arriving = null } = {}) {
  const n = S.queue.length;
  const c = counts();
  const list = n
    ? S.queue.map((q, i) => slipHTML(q, i, { arriving: q.id === arriving })).join("")
    : `<div class="q-clear">
        <div class="big">All clear</div>
        <p>Nothing is waiting on you. New approvals and questions land here, oldest first.</p>
        <dl>
          <dt>Working</dt><dd>${c.working}</dd>
          <dt>Idle</dt><dd>${c.idle}</dd>
          <dt>Cleared</dt><dd>${S.ledger.length}</dd>
          <dt>Last</dt><dd>${S.ledger[0]?.t ?? "—"}</dd>
        </dl>
      </div>`;
  const ledger = S.ledger.length
    ? `<details class="q-ledger" ${S.ledgerOpen ? "open" : ""}>
        <summary>${icon("chev-right")}<span class="label">Cleared</span><span class="mono">${S.ledger.length}</span></summary>
        <ol>${S.ledger
          .slice(0, 12)
          .map((l) => `<li><span>${esc(l.t)}</span><b>${esc(l.verdict)}</b><span>${esc(l.what)}</span></li>`)
          .join("")}</ol>
      </details>`
    : "";
  $("#queue").innerHTML = `
    <div class="pane-head">
      <h2 id="queueTitle">Waiting on you</h2><span class="count">${n} · oldest first</span>
      <span class="spacer"></span>
      <button class="icon-btn collapse" data-act="toggle-queue" aria-label="Collapse queue">${icon("panel")}</button>
    </div>
    <div class="q-list" role="list" aria-labelledby="queueTitle">${list}</div>
    ${ledger}
    <div class="q-rail">
      <button class="icon-btn" data-act="toggle-queue" aria-label="Expand queue">${icon("panel")}</button>
      <span class="n ${n ? "lit" : ""}" title="${n} waiting">${n}</span>
    </div>`;
  const det = $("#queue .q-ledger");
  det?.addEventListener("toggle", () => (S.ledgerOpen = det.open));
  if (focusActive) $(`#queue .slip.is-active`)?.focus({ preventScroll: false });
}

// Code may only wrap at token boundaries: after . :: ( , and before operators.
const breakable = (code) =>
  esc(code)
    .replace(/(::|\.|\(|,)/g, "$1<wbr>")
    .replace(/ (=|&lt;|&gt;|-|\+|\|\||&amp;&amp;) /g, " <wbr>$1 ");

function diffLines(rows) {
  return rows
    .map(([kind, a, b]) => {
      if (kind === "hunk") return `<div class="ln hunk">${esc(a)}</div>`;
      const sign = kind === "add" ? "+" : kind === "del" ? "−" : " ";
      const ind = b.match(/^ */)[0].length;
      return `<div class="ln ${kind}"><span>${a}</span><span>${sign}</span><span class="src" style="--ind:${ind}">${breakable(b.slice(ind))}</span></div>`;
    })
    .join("");
}

/* Acting on a slip: stamp → fold → advance the session. */
function act(qid, verdict) {
  const q = S.queue.find((x) => x.id === qid);
  if (!q || S.stamping.has(qid)) return;
  if (verdict === "answer" && S.answers[qid] == null && !S.answersText?.[qid]) {
    const el = $(`[data-slip="${qid}"] .options`);
    el?.animate?.([{ outline: "1px solid var(--ink)" }, { outline: "1px solid transparent" }], { duration: 600 });
    announce("Pick an option or type an answer first");
    return;
  }
  S.stamping.add(qid);
  const s = SES.get(q.session);
  const labels = { once: q.kind === "doom_loop" ? "Continued" : "Allowed once", always: "Always allowed", reject: q.kind === "doom_loop" ? "Stopped" : "Denied", answer: "Answered", dismiss: "Dismissed" };
  const label = labels[verdict];
  const t = clock();
  const wasFocused = document.activeElement?.closest?.(".queue");
  for (const el of $$(`[data-slip="${qid}"]`)) {
    el.classList.add("is-stamped");
    const a = $(".slip-head .age", el);
    if (a) a.textContent = "by you";
    el.insertAdjacentHTML("beforeend", `<div class="verdict-stamp" aria-hidden="true"><b>${esc(label)}</b><span>${esc(t)} · by you</span></div>`);
  }
  announce(`${label}: ${s.title}`);
  const answerText =
    verdict === "answer" ? (S.answersText?.[qid] || q.options[S.answers[qid]]?.label) : null;
  S.ledger.unshift({ t: hm(Date.now()), verdict: label, what: answerText ? `${s.title}: ${answerText}` : `${s.title}: ${slipSummary(q)}` });

  let done = false;
  const finish = () => {
    if (done) return;
    done = true;
    const idx = S.queue.findIndex((x) => x.id === qid);
    S.queue = S.queue.filter((x) => x.id !== qid);
    S.stamping.delete(qid);
    S.resolved[qid] = { verdict: label, t, answer: answerText };
    if (idx <= S.activeSlip) S.activeSlip = Math.max(0, Math.min(S.activeSlip, S.queue.length - 1));
    advanceSession(s, q, verdict, answerText);
    renderSide({ focusActive: !!wasFocused && !mobile() });
    renderBand();
    renderStatus();
    renderMobileTabs();
    patchRow(s.id);
    rerenderColumnsFor(s.id);
  };
  setTimeout(() => {
    const el = $(`#queue [data-slip="${qid}"]`);
    if (!el || reduced()) return finish();
    const inner = document.createElement("div");
    inner.className = "fold-inner";
    inner.append(...el.childNodes);
    el.append(inner);
    el.classList.add("folding");
    requestAnimationFrame(() => requestAnimationFrame(() => el.classList.add("folded")));
    el.addEventListener("transitionend", finish, { once: true });
    setTimeout(finish, 320);
  }, 480);
}

function findPart(sid, pred) {
  for (const m of transcriptOf(sid)) for (const p of m.parts ?? []) if (pred(p)) return { m, p };
  return null;
}
function lastAssistant(sid) {
  const t = transcriptOf(sid);
  for (let i = t.length - 1; i >= 0; i--) if (t[i].role === "assistant") return t[i];
  return null;
}

function advanceSession(s, q, verdict, answerText) {
  const hit = findPart(s.id, (p) => p.permission === q.id);
  const msg = lastAssistant(s.id);
  if (verdict === "reject" || verdict === "dismiss") {
    if (hit) {
      hit.p.status = "error";
      hit.p.summary = verdict === "dismiss" ? "Dismissed by you" : "Denied by you";
    }
    s.state = "idle";
    s.now = { kind: "done", text: verdict === "dismiss" ? "question dismissed · waiting for input" : `stopped · you denied ${q.kind}` };
    s.since = Date.now();
    return;
  }
  s.state = "working";
  s.since = Date.now();
  if (q.kind === "question") {
    s.now = { kind: "thinking", text: `applying answer: ${answerText}` };
    if (msg)
      msg.parts.push({
        type: "text",
        text: `Going with **${answerText}**. Next I'll sketch the store modules: \`events\` (one EventSource, reconnect with backoff), \`parts\` (flat map), and \`fleet\` (per-session aggregates derived from status, todos and the latest tool part).`,
      });
    D.activity[s.id] = [
      ["writing", "Sketching the store modules…"],
      ["read", "crates/alforria-schema/src/session_v1.rs"],
      ["writing", "Deriving per-session aggregates from part updates…"],
    ];
    return;
  }
  if (hit) {
    hit.p.status = "running";
    hit.p.target = hit.p.target || q.command || q.file;
  }
  if (q.kind === "bash" && q.id === "q2") {
    s.now = { kind: "bash", text: q.command };
    later(2200, () => {
      if (hit) {
        hit.p.status = "completed";
        hit.p.duration = "1.9s";
        hit.p.summary = "* [new branch]  fix/sse-heartbeat -> fix/sse-heartbeat";
      }
      msg?.parts.push({ type: "text", text: "Pushed. The branch is ready for a PR; CI will run the reconnect test with the 25 s grace." });
      s.state = "idle";
      s.now = { kind: "done", text: "pushed fix/sse-heartbeat" };
      s.since = Date.now();
      patchRow(s.id);
      rerenderColumnsFor(s.id);
      renderStatus();
    });
    return;
  }
  if (q.kind === "edit") {
    s.now = { kind: "edit", text: q.file };
    later(900, () => {
      if (hit) {
        hit.p.status = "completed";
        hit.p.duration = "0.2s";
        hit.p.add = q.add;
        hit.p.del = q.del;
        hit.p.diff = q.diff;
        hit.p.target = q.file;
      }
      const todos = findPart(s.id, (p) => p.type === "todos");
      if (todos) {
        todos.p.items[1][1] = "completed";
        todos.p.items[2][1] = "in_progress";
      }
      s.todos = [s.todos[0] + 1, s.todos[1]];
      D.activity[s.id] = [
        ["edit", "src/auth/store.rs"],
        ["bash", "cargo test auth::token"],
        ["writing", "Persisting the refreshed pair with a rename for atomicity…"],
      ];
      patchRow(s.id);
      rerenderColumnsFor(s.id);
    });
    return;
  }
  if (q.kind === "doom_loop") {
    s.now = { kind: "read", text: "src/data/pricing.ts" };
    D.activity[s.id] = [
      ["read", "src/data/pricing.ts"],
      ["edit", "src/data/pricing.ts"],
      ["bash", "npm run build"],
    ];
    return;
  }
  s.now = { kind: "bash", text: q.command };
  D.activity[s.id] ??= [["bash", q.command], ["edit", "Cargo.toml"], ["writing", "Wiring the client into the fallback chain…"]];
}

/* ───────────── session table ───────────── */

function stateCell(s) {
  const word = { waiting: "Waiting", working: "Working", retry: "Retry", idle: "Idle", fault: "Fault" }[s.state];
  return `<span class="state ${s.state}"><span class="lamp ${s.state}"></span><span>${word}</span></span>`;
}
function nowCell(s) {
  const n = s.now ?? { kind: "done", text: "" };
  let cls = "";
  let k = n.kind;
  if (s.state === "waiting") {
    cls = "waiting";
    k = n.kind === "question" ? "Asks" : "Approve";
  } else if (s.state === "idle") {
    cls = "idle";
    k = "Done";
  } else if (s.state === "fault") {
    cls = "fault";
    k = "Fault";
  } else if (s.state === "retry") {
    k = "Retry";
  } else if (n.kind === "writing" || n.kind === "thinking") {
    cls = "writing";
    k = n.kind === "writing" ? "Writing" : "Thinking";
  }
  const prose = ["writing", "thinking", "done", "question", "retry"].includes(n.kind) ? "prose" : "";
  return `<div class="now ${cls} ${prose}"><span class="k">${esc(k)}</span><span class="x" title="${esc(n.text)}">${esc(n.text)}</span></div>`;
}
function todosCell(s) {
  if (!s.todos) return `<span class="todos"><span class="n">—</span></span>`;
  const [d, t] = s.todos;
  const cells =
    t <= 10
      ? Array.from({ length: t }, (_, i) => `<i class="${i < d ? "done" : i === d && s.state !== "idle" ? "cur" : ""}"></i>`).join("")
      : "";
  return `<span class="todos" title="${d} of ${t} todos done"><span class="cells">${cells}</span><span class="n">${d}/${t}</span></span>`;
}
function ctxCell(s) {
  const high = s.ctx >= 80;
  return `<span class="ctx" title="Context ${s.ctx}% used"><span class="meter ${high ? "high" : ""}"><i style="width:${Math.min(100, s.ctx)}%"></i></span><span class="n ${high ? "high" : ""}">${s.ctx}%</span></span>`;
}

function rowCells(s) {
  const open = S.focus.some((c) => c.session === s.id && c.kind === "session");
  return `
    <td class="c-state">${stateCell(s)}</td>
    <td class="c-session" title="${esc(s.agent)} · ${esc(s.model)}"><div class="title-cell">${open ? `<span class="open-mark" title="Open in focus">${icon("columns")}</span>` : ""}<span class="t">${esc(s.title)}</span>${s.agent !== "build" ? `<span class="tag">${esc(s.agent)}</span>` : ""}</div></td>
    <td class="c-now">${nowCell(s)}</td>
    <td class="c-todos">${todosCell(s)}</td>
    <td class="c-ctx">${ctxCell(s)}</td>
    <td class="c-cost r"><span class="cost">${money(s.cost)}</span></td>
    <td class="c-age r"><span class="age-v" data-age="${s.id}">${age(Date.now() - s.since)}</span></td>`;
}

function rowClass(s) {
  return ["s-row", s.parent ? "sub" : "", s.state === "waiting" ? "is-waiting" : "", S.cursorId === s.id ? "is-cursor" : ""].join(" ");
}

function renderRegister() {
  const c = counts();
  const groups = visibleRows();
  if (!S.cursorId || !flatVisible().includes(S.cursorId)) S.cursorId = flatVisible()[0] ?? null;
  const filters = [
    ["all", "All", c.all],
    ["waiting", "Waiting", c.waiting],
    ["working", "Working", c.working],
    ["idle", "Idle", c.idle],
  ];
  const body = groups.length
    ? groups
        .map(({ project: p, rows }) => {
          const all = [...SES.values()].filter((s) => s.project === p.id);
          const w = all.filter((s) => s.state === "waiting").length;
          return `<tr class="proj-row"><td colspan="7"><div class="proj">
              <h3>${esc(p.name)}</h3>
              <span class="path">${esc(p.path)}</span>
              <span class="branch">${icon("fork")}${esc(p.branch)}</span>
              <span class="tally">${all.length} sessions${w ? ` · <b>${w} waiting</b>` : ""}</span>
              <button class="add" data-new="${p.id}">${icon("plus")}New session</button>
            </div></td></tr>
            ${rows.map((s) => `<tr class="${rowClass(s)}" data-sid="${s.id}" tabindex="-1" aria-label="${esc(s.title)}, ${s.state}">${rowCells(s)}</tr>`).join("")}`;
        })
        .join("")
    : `<tr><td colspan="7"><div class="q-empty"><h3>No sessions match</h3><p>Switch the filter back to All.</p></div></td></tr>`;

  $("#workspace").innerHTML = `
    <div class="pane-head ws-head">
      <h2>Sessions</h2><span class="count">${c.all} · ${D.projects.length} projects</span>
      <div class="seg" role="group" aria-label="Filter sessions">
        ${filters.map(([id, l, n]) => `<button aria-pressed="${S.filter === id}" data-filter="${id}"><span class="lbl-long">${l}</span><span class="c">${n}</span></button>`).join("")}
      </div>
    </div>
    <div class="ws-body" id="regBody">
      <table class="reg" aria-label="Sessions by project">
        <colgroup><col class="c-state"><col class="c-session"><col class="c-now"><col class="c-todos"><col class="c-ctx"><col class="c-cost"><col class="c-age"></colgroup>
        <thead><tr><th>State</th><th>Session</th><th>Now</th><th>Todos</th><th class="h-ctx">Context</th><th class="r">Cost</th><th class="r">Age</th></tr></thead>
        <tbody>${body}</tbody>
      </table>
    </div>`;
}

function patchRow(sid) {
  const s = SES.get(sid);
  const rr = $(`#queue .rail-row[data-rail="${sid}"]`);
  if (s && rr) rr.outerHTML = railRowHTML(s);
  const mini = $(`#queue .q-rail [data-rail="${sid}"] .lamp`);
  if (s && mini) mini.className = `lamp ${s.state}`;
  const tr = $(`#workspace tr[data-sid="${sid}"]`);
  if (!s || !tr) return;
  const before = $(".now .x", tr)?.textContent;
  tr.className = rowClass(s);
  tr.innerHTML = rowCells(s);
  if (before !== s.now?.text && !reduced()) $(".now", tr)?.classList.add("fresh");
  // Project tally may change with state.
  const proj = tr.parentElement && [...tr.parentElement.children].slice(0, [...tr.parentElement.children].indexOf(tr)).reverse().find((r) => r.classList.contains("proj-row"));
  if (proj) {
    const all = [...SES.values()].filter((x) => x.project === s.project);
    const w = all.filter((x) => x.state === "waiting").length;
    const tally = $(".tally", proj);
    if (tally) tally.innerHTML = `${all.length} sessions${w ? ` · <b>${w} waiting</b>` : ""}`;
  }
  const seg = $$("#workspace [data-filter] .c");
  if (seg.length) {
    const c = counts();
    [c.all, c.waiting, c.working, c.idle].forEach((n, i) => seg[i] && (seg[i].textContent = n));
  }
}

function moveCursor(d) {
  const ids = flatVisible();
  if (!ids.length) return;
  const i = Math.max(0, Math.min(ids.length - 1, ids.indexOf(S.cursorId) + d));
  const prev = S.cursorId;
  S.cursorId = ids[i];
  if (prev) patchRowClass(prev);
  patchRowClass(S.cursorId);
  $(`#workspace tr[data-sid="${S.cursorId}"]`)?.scrollIntoView({ block: "nearest" });
}
function patchRowClass(sid) {
  const tr = $(`#workspace tr[data-sid="${sid}"]`);
  const s = SES.get(sid);
  if (tr && s) tr.className = rowClass(s);
}

/* ───────────── focus columns ───────────── */

function maxCols() {
  const w = $("#workspace").clientWidth || innerWidth;
  return Math.max(1, Math.min(3, Math.floor(w / 400)));
}

// mode "replace": show in the active column (opening focus if needed).
// mode "beside": add a column, or reuse the least recently used other one when full.
function openSession(sid, { tab = "transcript", mode = "replace" } = {}) {
  const at = S.focus.findIndex((c) => c.kind === "session" && c.session === sid);
  if (at >= 0) {
    S.activeCol = at;
    if (tab !== "transcript") S.focus[at].tab = tab;
  } else {
    const col = { key: `c${++S.colSeq}`, kind: "session", session: sid, tab, used: Date.now() };
    const inFocus = S.view === "focus" && S.focus.length;
    if (mode === "beside" && S.focus.length < maxCols()) {
      S.focus.push(col);
      S.activeCol = S.focus.length - 1;
    } else if (mode === "beside") {
      const others = S.focus.map((c, i) => i).filter((i) => i !== S.activeCol);
      const lru = others.reduce((a, i) => (S.focus[i].used < S.focus[a].used ? i : a), others[0] ?? 0);
      S.focus[lru] = col;
      S.activeCol = lru;
    } else if (!S.focus.length) {
      S.focus.push(col);
      S.activeCol = 0;
    } else {
      S.focus[inFocus ? S.activeCol : S.activeCol] = col;
    }
  }
  S.focus[S.activeCol].used = Date.now();
  S.cursorId = sid;
  S.view = "focus";
  S.mobile = "focus";
  renderMobileTabs();
  renderWorkspace();
  renderSide();
}

function openNextWaiting() {
  const q = S.queue.reduce((a, x) => (!a || x.since < a.since ? x : a), null);
  if (!q) return announce("Nothing is waiting on you");
  openSession(q.session);
  requestAnimationFrame(() => {
    const slip = $(`#workspace .col.is-active [data-slip="${q.id}"]`);
    slip?.scrollIntoView({ block: "center" });
    slip?.focus({ preventScroll: true });
  });
}
function stepSession(d) {
  const order = railOrder();
  const cur = S.focus[S.activeCol]?.session;
  const i = order.indexOf(cur);
  const next = order[(i + d + order.length) % order.length];
  if (next) openSession(next);
  requestAnimationFrame(() => $(`#queue [data-rail="${next}"]`)?.scrollIntoView({ block: "nearest" }));
}

function openTerminal(pid) {
  const col = { key: `c${++S.colSeq}`, kind: "terminal", project: pid, used: Date.now(), lines: D.terminalLines.filter((l) => l !== "$ ") };
  if (S.focus.length >= maxCols()) S.focus[S.activeCol] = col;
  else S.focus.push(col);
  S.activeCol = S.focus.indexOf(col);
  S.view = "focus";
  S.mobile = "focus";
  renderMobileTabs();
  renderWorkspace();
  renderSide();
}

function closeCol(i) {
  S.focus.splice(i, 1);
  S.activeCol = Math.max(0, Math.min(S.activeCol, S.focus.length - 1));
  if (!S.focus.length) {
    S.view = "overview";
    S.mobile = "sessions";
  }
  renderWorkspace();
  renderSide();
  renderMobileTabs();
}

function saveScroll() {
  for (const el of $$("#workspace .transcript")) {
    S.scroll[el.dataset.key] = el.scrollTop;
    S.stick[el.dataset.key] = el.scrollHeight - el.scrollTop - el.clientHeight < 24;
  }
}
function restoreScroll(root = document) {
  for (const el of $$(".transcript", root)) {
    const k = el.dataset.key;
    if (S.stick[k] !== false) el.scrollTop = el.scrollHeight;
    else el.scrollTop = S.scroll[k] ?? 0;
    // Late font loads grow the content; stay pinned if the reader was at the bottom.
    document.fonts?.ready.then(() => {
      if (S.stick[k] !== false) el.scrollTop = el.scrollHeight;
    });
    el.addEventListener("scroll", () => {
      const atBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 24;
      S.stick[k] = atBottom;
      S.scroll[k] = el.scrollTop;
      $(".latest", el)?.classList.toggle("show", !atBottom);
    });
  }
}

function renderFocus() {
  saveScroll();
  $("#workspace").innerHTML = `
    <div class="pane-head ws-head">
      <button class="back-link" data-act="overview">${icon("back")}Overview<kbd>Esc</kbd></button>
      <span class="spacer"></span>
      <span class="count">${S.focus.length} of ${maxCols()} columns</span>
      ${S.focus.length > 1 ? `<button class="link-btn" data-act="close-others">Close others</button>` : ""}
    </div>
    <div class="ws-body focus" id="focusRoot">${S.focus.map((c, i) => colHTML(c, i)).join("")}</div>`;
  restoreScroll($("#workspace"));
}

function rerenderColumnsFor(sid) {
  if (S.view !== "focus") return;
  S.focus.forEach((c, i) => {
    if (c.session !== sid) return;
    const el = $(`#workspace .col[data-col="${i}"]`);
    if (!el) return;
    saveScroll();
    const active = document.activeElement;
    const keepComposer = active && el.contains(active) && active.tagName === "TEXTAREA" ? active.value : null;
    el.outerHTML = colHTML(c, i);
    const fresh = $(`#workspace .col[data-col="${i}"]`);
    restoreScroll(fresh);
    if (keepComposer != null) {
      const ta = $("textarea", fresh);
      ta.value = keepComposer;
      ta.focus();
    }
  });
}

function colHTML(c, i) {
  const active = i === S.activeCol;
  if (c.kind === "terminal") {
    const p = PROJ.get(c.project);
    return `<section class="col ${active ? "is-active" : ""}" data-col="${i}" aria-label="Terminal ${esc(p.name)}">
      <div class="col-head">
        <div class="l1">
          <button class="icon-btn back-btn" data-act="mobile-back" aria-label="Back">${icon("back")}</button>
          ${icon("terminal")}<h2>Terminal</h2>
          <div class="acts"><button data-close="${i}" aria-label="Close column">${icon("close")}</button></div>
        </div>
        <div class="l2"><span class="grow">${esc(p.path)} · zsh · pty 120×38</span></div>
        <div class="tabs" role="tablist"><button role="tab" aria-selected="true">Shell</button></div>
      </div>
      <div class="term" tabindex="0" aria-label="Terminal output">${c.lines
        .map((l) => (l.startsWith("$ ") ? `<span class="prompt">$ </span>${esc(l.slice(2))}` : esc(l)))
        .join("\n")}</div>
      <label class="term-line"><span class="prompt">$</span><input data-term-input="${i}" autocomplete="off" autocapitalize="off" autocorrect="off" spellcheck="false" enterkeyhint="send" aria-label="Run a command in ${esc(p.name)}" /></label>
    </section>`;
  }
  const s = SES.get(c.session);
  const p = PROJ.get(s.project);
  const ch = D.changes[s.id] ?? [];
  const busy = s.state === "working" || s.state === "retry";
  const tabs = [
    ["transcript", "Transcript", ""],
    ["changes", "Changes", ch.length || ""],
    ["files", "Files", ""],
  ];
  let body = "";
  if (c.tab === "changes") body = changesHTML(s, c);
  else if (c.tab === "files") body = filesHTML(c);
  else body = transcriptHTML(s, c);
  const wq = queueFor(s.id);
  const band = wq
    ? `<button class="col-band" data-jump="${wq.id}" data-col-i="${i}"><span class="lamp waiting"></span>Waiting on you · ${esc(slipKind(wq))}<span class="spacer"></span><span class="go">Jump${icon("arrow-down")}</span></button>`
    : "";
  return `<section class="col ${active ? "is-active" : ""}" data-col="${i}" aria-label="${esc(s.title)}">
    ${band}
    <div class="col-head">
      <div class="l1">
        <button class="icon-btn back-btn" data-act="mobile-back" aria-label="Back to sessions">${icon("back")}</button>
        <span class="lamp ${s.state}" title="${s.state}"></span>
        <h2 title="${esc(s.title)}">${esc(s.title)}</h2>
        <div class="acts">
          <button class="term-btn" data-term-open="${s.project}" aria-label="Open terminal in ${esc(p.name)}">${icon("terminal")}</button>
          <button data-fork="${s.id}" aria-label="Fork session">${icon("fork")}</button>
          <button data-compact="${s.id}" aria-label="Compact session">${icon("compact")}</button>
          <button data-close="${i}" aria-label="Close column">${icon("close")}</button>
        </div>
      </div>
      <div class="l2">
        <span class="grow">${esc(p.name)} · ${esc(s.agent)} · ${esc(s.model)}${s.branch ? ` · ${esc(s.branch)}` : ""}</span>
        ${ctxCell(s)}<span>${money(s.cost)}</span><span>${ktok(s.tokens)} tok</span>
      </div>
      <div class="tabs" role="tablist" aria-label="Session views">
        ${tabs.map(([id, l, n]) => `<button role="tab" aria-selected="${c.tab === id}" data-tab="${id}" data-col-i="${i}">${l}${n ? `<span class="c">${n}</span>` : ""}</button>`).join("")}
      </div>
    </div>
    ${body}
    ${c.tab === "transcript" ? composerHTML(s, i, busy) : ""}
  </section>`;
}

function transcriptHTML(s, c) {
  const msgs = transcriptOf(s.id)
    .map((m) => {
      if (m.role === "user")
        return `<div class="msg msg-user"><div class="msg-label">You<span class="mono">${hm(m.at)}</span></div><div class="body">${inline(m.text)}</div></div>`;
      const parts = m.parts.map((p, j) => partHTML(s, p, `${m.at}-${j}`)).join("");
      const live = m.live && s.state === "working";
      return `<div class="msg msg-asst">
        <div class="msg-label">${esc(m.agent)}<span class="mono">${esc(m.model)} · ${hm(m.at)}</span></div>
        <div class="parts">${parts}${live && m.parts.at(-1)?.type === "text" ? `<div class="prose"><span class="caret"></span></div>` : ""}</div>
        ${m.tokens && !live ? `<div class="step-meta">${ktok(m.tokens)} tokens · ${money(m.cost)}${m.duration ? ` · ${m.duration}` : ""}</div>` : ""}
      </div>`;
    })
    .join("");
  return `<div class="transcript" data-key="${c.key}" tabindex="0" aria-label="Transcript">${msgs}
    <button class="latest" data-act="latest">${icon("arrow-down")}Latest</button></div>`;
}

function partHTML(s, p, key) {
  switch (p.type) {
    case "text":
      return `<div class="prose">${md(p.text)}</div>`;
    case "reasoning":
      return `<div class="reasoning"><button data-toggle-reason>${icon("chev-right")}Reasoning · ${p.seconds}s</button><div class="r-body">${esc(p.text)}</div></div>`;
    case "todos": {
      const done = p.items.filter(([, st]) => st === "completed").length;
      return `<div class="todo-plate"><header>Todos<span class="n">${done}/${p.items.length}</span></header><ol>${p.items
        .map(([t, st], i) => `<li class="${st}"><span class="no">${String(i + 1).padStart(2, "0")}</span><span class="box"></span><span>${inline(t)}</span></li>`)
        .join("")}</ol></div>`;
    }
    case "subtask": {
      const child = SES.get(p.session);
      return `<div class="subtask"><span class="name">Task</span>
        <span class="t"><span class="mono">${esc(p.agent)}</span>${esc(p.title)}</span>
        <button class="link-btn" data-open="${p.session}">${child ? `<span class="lamp ${child.state}"></span>` : ""}Open</button>
        <span class="res">${p.result ? inline(p.result) : child ? childNow(child) : "Running…"}</span></div>`;
    }
    case "error":
      return `<div class="fault-plate"><header>Fault<span class="mono">${esc(p.name)}</span></header><p>${esc(p.message)}</p>
        <div class="slip-actions"><button class="stamp primary" data-compact="${s.id}">${icon("compact")}Compact session</button><button class="stamp" data-fork="${s.id}">${icon("fork")}Fork and continue</button></div></div>`;
    case "question": {
      const q = S.queue.find((x) => x.id === p.permission);
      if (q) return slipHTML(q, 0, { inline: true });
      const r = S.resolved[p.permission];
      return `<div class="verdict-row"><b>${esc(r?.verdict ?? "Answered")}</b><span>${esc(r?.answer ?? "")}</span><span class="spacer"></span><span>${esc(r?.t ?? "")}</span></div>`;
    }
    case "tool":
      return toolHTML(s, p, key);
    case "retry":
      return `<div class="tool"><div class="tool-row"><span class="mark running"></span><span class="name">Retry</span><span class="target">${esc(p.reason)} · attempt ${p.attempt}</span><span class="meta" data-retry="${s.id}">${s.retryIn ? `next in ${s.retryIn}s` : "backing off"}</span></div></div>`;
    default:
      return "";
  }
}

function childNow(c) {
  const n = c.now;
  if (["writing", "thinking", "done", "question", "retry"].includes(n.kind)) return esc(n.text);
  return `<span class="sub-k">${esc(n.kind)}</span><code>${esc(n.text)}</code>`;
}

function toolHTML(s, p, key) {
  const q = p.permission ? S.queue.find((x) => x.id === p.permission) : null;
  if (q && p.status === "pending") {
    return slipHTML(q, 0, { inline: true });
  }
  const r = p.permission ? S.resolved[p.permission] : null;
  const verdict = r ? `<div class="verdict-row"><b>${esc(r.verdict)}</b><span>by you · ${esc(r.t)}</span></div>` : "";
  const meta =
    p.add != null
      ? `<span class="meta"><span class="add">+${p.add}</span> <span class="del">−${p.del}</span>${p.duration ? ` · ${p.duration}` : ""}</span>`
      : `<span class="meta">${p.status === "running" ? "running" : p.duration ?? ""}</span>`;
  const hasBody = p.diff || p.output;
  const open = p.open || p.status === "running";
  let bodyHTML = "";
  if (p.diff) bodyHTML = `<div class="diff">${diffLines(p.diff)}</div>`;
  if (p.output)
    bodyHTML = `<div class="out" ${p.stream ? `data-stream="${s.id}"` : ""}>${p.output.map(outLine).join("\n")}</div>`;
  const row = `<span class="mark ${p.status}"></span><span class="name">${esc(p.tool)}</span><span class="target">${esc(p.target)}</span>${meta}${hasBody ? icon("chev-right", "chev") : ""}`;
  return `${verdict}<div class="tool ${open ? "open" : ""}" data-tool="${key}">
    ${hasBody ? `<button class="tool-row" data-toggle-tool aria-expanded="${open}">${row}</button>` : `<div class="tool-row">${row}</div>`}
    ${p.summary ? `<div class="tool-summary">${esc(p.summary)}</div>` : ""}
    ${hasBody ? `<div class="tool-body">${bodyHTML}</div>` : ""}
  </div>`;
}
function outLine(l) {
  if (/\bPASS\b/.test(l)) return `<span class="pass">${esc(l)}</span>`;
  if (/\bFAIL\b/.test(l)) return `<span class="fail">${esc(l)}</span>`;
  return esc(l);
}

function composerHTML(s, i, busy) {
  const agentSeg = ["build", "plan"]
    .map((a) => `<button aria-pressed="${(s.agent === "plan" ? "plan" : "build") === a}" data-agent="${a}" data-sid="${s.id}">${a}</button>`)
    .join("");
  return `<div class="composer" style="position:relative">
    <textarea data-compose="${s.id}" data-col-i="${i}" rows="2" placeholder="Message ${esc(s.agent)} — @ for files, / for commands" aria-label="Message ${esc(s.title)}"></textarea>
    <div class="row">
      <div class="seg" role="group" aria-label="Agent">${agentSeg}</div>
      <button class="model-btn" data-model="${s.id}" title="Switch model (${esc(s.model)})"><span class="m">${esc(s.model)}</span>${icon("chev-down")}</button>
      <button class="icon-btn attach" aria-label="Attach file">${icon("attach")}</button>
      <span class="spacer"></span>
      <span class="hint">↵ send · ⇧↵ newline</span>
      ${busy ? `<button class="send stop" data-abort="${s.id}">${icon("stop")}Stop</button>` : `<button class="send" data-send="${s.id}">${icon("send")}Send</button>`}
    </div>
  </div>`;
}

function changesHTML(s, c) {
  const files = D.changes[s.id] ?? [];
  if (!files.length) return `<div class="transcript" data-key="${c.key}"><div class="q-empty"><h3>No changes yet</h3><p>File edits from this session will be listed here, with revert.</p></div></div>`;
  c.file ??= 0;
  const add = files.reduce((a, f) => a + f.add, 0);
  const del = files.reduce((a, f) => a + f.del, 0);
  const f = files[c.file];
  const hit = findPart(s.id, (p) => p.diff && p.target === f.path);
  const rows = hit?.p.diff ?? synthDiff(f);
  return `<div class="changes">
    <div>
      <div class="change-bar"><span>${files.length} files · <span class="add" style="color:var(--add-mark)">+${add}</span> <span style="color:var(--del-mark)">−${del}</span> · since first prompt</span><span class="spacer"></span>
        ${c.confirmRevert ? `<span>Revert ${files.length} files to the snapshot before this session?</span><button class="link-btn" data-revert-yes="${s.id}">Revert</button><button class="link-btn" data-revert-no>Cancel</button>` : `<button class="link-btn" data-revert="${s.id}">${icon("undo")}Revert all</button>`}
      </div>
      <div class="change-list" role="listbox" aria-label="Changed files">${files
        .map((x, j) => `<button role="option" aria-selected="${j === c.file}" data-file-i="${j}"><span>${esc(x.path)}</span><span class="add">+${x.add}</span><span class="del">−${x.del}</span></button>`)
        .join("")}</div>
    </div>
    <div class="change-diff"><div class="diff">${diffLines(rows)}</div></div>
  </div>`;
}
function synthDiff(f) {
  const rows = [["hunk", `@@ -1,6 +1,${6 + f.add - f.del} @@`]];
  for (let i = 0; i < Math.min(f.del, 4); i++) rows.push(["del", i + 3, `    // previous implementation line ${i + 1}`]);
  for (let i = 0; i < Math.min(f.add, 8); i++) rows.push(["add", i + 3, `    // new implementation line ${i + 1}`]);
  rows.push(["ctx", 12, "}"]);
  return rows;
}

function filesHTML(c) {
  c.filePath ??= "crates/alforria-tui/src/transport/events.rs";
  const tree = D.fileTree
    .map((f) => {
      const name = f.path.replace(/\/$/, "").split("/").pop();
      return `<button class="${f.path === c.filePath ? "on" : ""}" style="padding-left:${12 + (f.depth ?? 0) * 12}px" ${f.dir ? "" : `data-file="${esc(f.path)}"`}>${icon(f.dir ? "folder" : "file")}${esc(name)}</button>`;
    })
    .join("");
  const code = c.filePath.endsWith("events.rs") ? D.fileContent : `// ${c.filePath}\n// (synthetic preview)\n`;
  return `<div class="files"><nav class="tree" aria-label="Project files">${tree}</nav>
    <div class="code" tabindex="0" aria-label="${esc(c.filePath)}">${code.split("\n").map((l, i) => `<div class="ln"><span>${i + 1}</span><span>${hl(l)}</span></div>`).join("")}</div></div>`;
}
function hl(line) {
  const ci = line.indexOf("//");
  const code = ci >= 0 ? line.slice(0, ci) : line;
  const cm = ci >= 0 ? line.slice(ci) : "";
  const out = esc(code)
    .replace(/(&quot;.*?&quot;)/g, '<span class="st">$1</span>')
    .replace(/\b(use|const|pub|async|fn|let|mut|loop|match|break|Ok|Err|None|Some|crate|await)\b/g, '<span class="kw">$1</span>');
  return out + (cm ? `<span class="cm">${esc(cm)}</span>` : "");
}

/* ───────────── settings ───────────── */

function renderSettings() {
  const pages = [
    ["providers", "Providers"],
    ["models", "Models"],
    ["agents", "Agents"],
    ["mcp", "MCP servers"],
    ["permissions", "Permissions"],
    ["appearance", "Appearance"],
    ["server", "Server"],
  ];
  let content = "";
  const pg = S.settingsPage;
  if (pg === "providers")
    content = `<h3>Providers</h3><p class="lead">Credentials live on the server in auth.json; the browser never stores keys.</p>
      <div class="spec providers">
        <div class="row head"><span>Provider</span><span>Endpoint</span><span>Credentials</span><span class="r">Models</span><span></span></div>
        ${D.settings.providers
          .map((p) => `<div class="row"><span class="n">${esc(p.name)}</span><span class="d mono ep ${p.status === "connected" ? "ok" : ""}">${esc(p.endpoint)}</span>
            <span class="d cr">${esc(p.auth)}</span><span class="d mono r md">${p.models != null ? `${p.models}<span class="unit"> models</span>` : "—"}</span>
            ${p.status === "connected" ? `<span class="pill-state on st">Connected</span>` : `<button class="link-btn st">Connect</button>`}</div>`)
          .join("")}</div>`;
  else if (pg === "permissions")
    content = `<h3>Permissions</h3><p class="lead">Rules are evaluated last-match-wins. Anything marked Ask lands in the queue.</p>
      <div class="spec numbered">${D.settings.permissions
        .map((r, i) => `<div class="row"><span class="no">${String(i + 1).padStart(2, "0")}</span><span class="n">${esc(r.tool)}</span><span class="d mono">${esc(r.pattern)}</span>
          <div class="seg perm-seg" role="group" aria-label="${esc(r.tool)} ${esc(r.pattern)}">${["allow", "ask", "deny"].map((a) => `<button aria-pressed="${r.action === a}" data-perm="${i}" data-v="${a}">${a}</button>`).join("")}</div></div>`)
        .join("")}</div>`;
  else if (pg === "appearance")
    content = `<h3>Appearance</h3><p class="lead">Stored in this browser only.</p>
      <div class="spec"><div class="row"><span class="n">Theme</span><span class="d">Dark suits long supervision; light matches the printed datasheet.</span>
      <div class="seg" role="group" aria-label="Theme">${["dark", "light"].map((t) => `<button aria-pressed="${S.theme === t}" data-theme-set="${t}">${t}</button>`).join("")}</div></div>
      <div class="row"><span class="n">Queue</span><span class="d">Collapse the queue to a rail when nothing is waiting.</span><span class="pill-state">Manual</span></div></div>`;
  else if (pg === "server")
    content = `<h3>Server</h3><p class="lead">This client talks to one alforria server. A LibertAI account can list more machines here later.</p>
      <div class="spec">
        <div class="row"><span class="n">Address</span><span class="d mono ok">${esc(D.server.address)}</span><span class="pill-state on">Linked</span></div>
        <div class="row"><span class="n">Version</span><span class="d mono ok">alforria ${esc(D.server.version)} · API v1</span><span></span></div>
        <div class="row"><span class="n">Auth</span><span class="d mono ok">HTTP Basic · user alforria</span><span class="pill-state on">On</span></div>
        <div class="row"><span class="n">Event stream</span><span class="d mono ok">/global/event · heartbeat 10 s</span><span class="pill-state on">Live</span></div>
      </div>`;
  else {
    const lists = {
      models: [["Default", "libertai/qwen3-coder-480b", 1], ["Small", "libertai/qwen3-coder-30b", 1], ["Plan agent", "libertai/kimi-k2", 1]],
      agents: [["build", "Primary · all tools"], ["plan", "Primary · edits ask"], ["explore", "Subagent · read-only"], ["general", "Subagent · all tools"]],
      mcp: [["context7", "stdio · 2 tools", 1], ["playwright", "stdio · 21 tools", 1]],
    }[pg];
    content = `<h3>${pages.find(([id]) => id === pg)[1]}</h3><p class="lead">Read from opencode.json and the server catalog.</p>
      <div class="spec">${lists.map(([n, d, mono]) => `<div class="row"><span class="n">${esc(n)}</span><span class="d ok ${mono ? "mono" : ""}">${esc(d)}</span><button class="link-btn">Edit</button></div>`).join("")}</div>`;
  }
  $("#workspace").innerHTML = `
    <div class="pane-head ws-head"><button class="back-link" data-act="settings-back">${icon("back")}Back<kbd>Esc</kbd></button><h2>Settings</h2></div>
    <div class="ws-body"><div class="settings">
      <nav aria-label="Settings sections">${pages.map(([id, l]) => `<button aria-current="${pg === id}" data-settings="${id}">${l}</button>`).join("")}</nav>
      <section>${content}</section>
    </div></div>`;
}

/* ───────────── workspace / status / mobile ───────────── */

function renderWorkspace() {
  if (S.view === "focus" && !S.focus.length) S.view = "overview";
  app.dataset.view = S.view;
  if (S.view === "settings") renderSettings();
  else if (S.view === "focus") renderFocus();
  else renderRegister();
  renderBandNav();
  syncURL();
}

function routeHash() {
  if (S.view === "focus") return `#/focus/${S.focus.map((c) => (c.kind === "session" ? c.session : `term:${c.project}`)).join(",")}`;
  if (S.view === "settings") return `#/settings/${S.settingsPage}`;
  return "#/";
}
function syncURL() {
  if (S.routing) return;
  const h = routeHash();
  if (location.hash !== h) history.pushState(null, "", h);
}
function applyHash() {
  const [, view = "", arg = ""] = (location.hash || "#/").split("/");
  if (!$("#palette").hidden) closePalette();
  if (!$("#keysSheet").hidden) closeKeys();
  S.routing = true;
  if (view === "focus" && arg) {
    const prev = S.focus;
    S.focus = arg
      .split(",")
      .map((id) => {
        if (id.startsWith("term:")) return prev.find((c) => c.kind === "terminal" && c.project === id.slice(5)) ?? { key: `c${++S.colSeq}`, kind: "terminal", project: id.slice(5), used: Date.now(), lines: D.terminalLines.filter((l) => l !== "$ ") };
        if (!SES.has(id)) return null;
        return prev.find((c) => c.session === id) ?? { key: `c${++S.colSeq}`, kind: "session", session: id, tab: "transcript", used: Date.now() };
      })
      .filter(Boolean);
    S.activeCol = Math.min(S.activeCol, Math.max(0, S.focus.length - 1));
    S.view = "focus";
    S.mobile = "focus";
  } else if (view === "settings") {
    S.settingsPage = arg || "providers";
    S.view = "settings";
  } else {
    S.view = "overview";
    if (S.mobile === "focus") S.mobile = "sessions";
  }
  renderWorkspace();
  renderSide();
  renderMobileTabs();
  S.routing = false;
}
addEventListener("popstate", applyHash);

function renderStatus() {
  const c = counts();
  const cost = [...SES.values()].reduce((a, s) => a + s.cost, 0);
  const tok = [...SES.values()].reduce((a, s) => a + s.tokens, 0);
  $("#status").innerHTML = `
    <span title="Event stream"><i class="link-dot"></i>LINKED /global/event · last frame <span id="lastFrame">${((Date.now() - S.lastFrame) / 1000).toFixed(1)}s</span></span>
    <span>${c.all} sessions · ${c.working} working · ${c.waiting} waiting</span>
    <span>${ktok(tok)} tokens today</span>
    <span>${money(cost)} today</span>
    <span class="spacer"></span>
    <span class="synthetic">SYNTHETIC DATA · PROTOTYPE</span>
    <button data-act="keys">? Keys</button>`;
}

function renderMobileTabs() {
  app.dataset.mobile = S.mobile;
  const n = S.queue.length;
  const cur = (m) => S.view !== "settings" && S.mobile === m;
  $("#mobileTabs").innerHTML = `
    <button aria-current="${cur("queue")}" data-mobile="queue">${icon("queue")}<span>Queue${n ? `<span class="badge">${n}</span>` : ""}</span></button>
    <button aria-current="${cur("sessions")}" data-mobile="sessions">${icon("rows")}<span>Sessions</span></button>
    <button aria-current="${cur("focus")}" data-mobile="focus" ${S.focus.length ? "" : "disabled"}>${icon("columns")}<span>Focus</span></button>`;
}

function renderAll() {
  renderBand();
  renderSide();
  renderWorkspace();
  renderStatus();
  renderMobileTabs();
}

/* ───────────── palette ───────────── */

function paletteItems(q) {
  const items = [];
  const ql = q.toLowerCase().trim();
  const m = (t) => !ql || ql.split(/\s+/).every((w) => t.toLowerCase().includes(w));
  for (const s of SES.values()) {
    const t = `${s.title} ${PROJ.get(s.project).name}`;
    if (m(t)) items.push({ group: "Sessions", label: s.title, meta: `${PROJ.get(s.project).name} · ${s.state}`, lamp: s.state, run: () => openSession(s.id) });
  }
  const cmds = [
    ...D.projects.map((p) => ({ label: `New session in ${p.name}`, meta: "N", run: () => newSession(p.id) })),
    ...D.projects.map((p) => ({ label: `Open terminal in ${p.name}`, meta: "", run: () => openTerminal(p.id) })),
    { label: "Show waiting only", meta: "filter", run: () => setFilter("waiting") },
    { label: `Switch to ${S.theme === "dark" ? "light" : "dark"} theme`, meta: "", run: toggleTheme },
    { label: "Open settings", meta: "", run: () => setView("settings") },
    { label: "Keyboard shortcuts", meta: "?", run: openKeys },
  ];
  for (const c of cmds) if (m(c.label)) items.push({ group: "Commands", ...c });
  for (const f of D.fileTree.filter((f) => !f.dir))
    if (m(f.path)) items.push({ group: "Files · opencode-rs", label: f.path, meta: "", file: true, run: () => openFile(f.path) });
  return items.slice(0, 40);
}
function openPalette(prefill = "") {
  const el = $("#palette");
  el.hidden = false;
  el.innerHTML = `<div class="palette" role="dialog" aria-modal="true" aria-label="Command palette">
    <input id="palInput" placeholder="Jump to a session, file or command" value="${esc(prefill)}" autocomplete="off" spellcheck="false" aria-controls="palResults" />
    <div class="results" id="palResults" role="listbox"></div></div>`;
  S.palette.q = prefill;
  S.palette.sel = 0;
  drawPalette();
  const input = $("#palInput");
  input.focus();
  input.setSelectionRange(input.value.length, input.value.length);
}
function drawPalette() {
  S.palette.items = paletteItems(S.palette.q);
  let g = "";
  $("#palResults").innerHTML = S.palette.items.length
    ? S.palette.items
        .map((it, i) => {
          const head = it.group !== g ? `<h4>${esc((g = it.group))}</h4>` : "";
          return `${head}<button class="res ${i === S.palette.sel ? "on" : ""}" role="option" aria-selected="${i === S.palette.sel}" data-pal="${i}">
            ${it.lamp ? `<span class="lamp ${it.lamp}"></span>` : it.file ? icon("file") : icon("chev-right")}
            <span class="t">${esc(it.label)}</span><span class="m">${esc(it.meta)}</span></button>`;
        })
        .join("")
    : `<div class="empty">Nothing matches “${esc(S.palette.q)}”.</div>`;
  $("#palResults .res.on")?.scrollIntoView({ block: "nearest" });
}
function closePalette() {
  $("#palette").hidden = true;
  $("#palette").innerHTML = "";
}

function openKeys() {
  const el = $("#keysSheet");
  el.hidden = false;
  const row = (keys, what) => `<dt>${keys.map((k) => `<kbd>${k}</kbd>`).join("")}</dt><dd>${what}</dd>`;
  el.innerHTML = `<div class="keys" role="dialog" aria-modal="true" aria-label="Keyboard shortcuts"><h3>Keys</h3><dl>
    <div class="grp">Anywhere</div>
    ${row(["⌘K"], "Search sessions, files, commands")}${row(["Q"], "Go to the queue")}${row(["N"], "New session")}${row(["Esc"], "Back to overview")}${row(["F"], "Back to focus")}${row(["?"], "This sheet")}
    <div class="grp">Queue</div>
    ${row(["A"], "Allow once / continue")}${row(["S"], "Always allow this pattern")}${row(["D"], "Deny / stop")}${row(["1", "–", "9"], "Pick an answer")}${row(["↵"], "Submit answer")}${row(["J", "K"], "Next / previous item")}
    <div class="grp">Sessions</div>
    ${row(["J", "K"], "Move through rows")}${row(["↵"], "Open in focus")}${row(["⇧", "↵"], "Open beside the current column")}
    <div class="grp">Focus</div>
    ${row(["Alt", "↑", "↓"], "Previous / next session in the active column")}${row(["W"], "Open the oldest waiting session")}${row(["[", "]"], "Previous / next column")}${row(["I"], "Write in the active column")}
  </dl></div>`;
  el.onclick = (e) => e.target === el && closeKeys();
}
function closeKeys() {
  $("#keysSheet").hidden = true;
}

/* ───────────── actions ───────────── */

function setView(v) {
  if (v === "settings" && S.view !== "settings") S.prevView = S.view;
  S.view = v;
  if (v === "overview") S.mobile = "sessions";
  renderWorkspace();
  renderSide();
  renderMobileTabs();
}
function setFilter(f) {
  S.filter = f;
  S.view = "overview";
  renderWorkspace();
}
function toggleTheme() {
  S.theme = S.theme === "dark" ? "light" : "dark";
  document.documentElement.dataset.theme = S.theme;
  $('meta[name="theme-color"]').content = S.theme === "dark" ? "#0B0E10" : "#F5F7F8";
  persist();
  renderBand();
  if (S.view === "settings") renderSettings();
}
function openFile(path) {
  const sid = [...SES.values()].find((s) => s.project === "p1")?.id ?? "s2";
  openSession("s2", { tab: "files" });
  const col = S.focus[S.activeCol];
  col.filePath = path;
  renderWorkspace();
  void sid;
}
function newSession(pid) {
  const id = `n${Date.now().toString(36)}`;
  const s = { id, project: pid, title: "New session", agent: "build", model: "qwen3-coder-480b", state: "idle", now: { kind: "done", text: "draft · nothing sent yet" }, todos: null, ctx: 0, cost: 0, since: Date.now(), tokens: 0 };
  SES.set(id, s);
  TRANSCRIPTS.set(id, []);
  openSession(id);
  renderStatus();
  setTimeout(() => $(`textarea[data-compose="${id}"]`)?.focus(), 30);
}
function focusQueue() {
  if (!S.queueOpen) {
    S.queueOpen = true;
    app.dataset.queue = "open";
    persist();
  }
  S.mobile = "queue";
  renderMobileTabs();
  S.activeSlip = 0;
  renderQueue({ focusActive: true });
}

function sendMessage(sid, text) {
  const s = SES.get(sid);
  if (!text.trim()) return;
  const t = transcriptOf(sid);
  if (s.title === "New session") s.title = text.trim().split(/\s+/).slice(0, 6).join(" ").replace(/[.,:;]$/, "");
  t.push({ role: "user", at: Date.now(), text });
  const msg = { role: "assistant", agent: s.agent, model: s.model, at: Date.now(), tokens: 0, cost: 0, live: true, parts: [{ type: "text", text: "" }] };
  t.push(msg);
  s.state = "working";
  s.since = Date.now();
  s.now = { kind: "thinking", text: "reading the request" };
  patchRow(sid);
  rerenderColumnsFor(sid);
  const reply = "I'll start by reading the relevant files, then propose a change before editing anything.";
  let i = 0;
  const tick = setInterval(() => {
    i += 3;
    msg.parts[0].text = reply.slice(0, i);
    s.now = { kind: "writing", text: reply.slice(0, i) };
    const prose = $$(`#workspace .col`).filter((c) => S.focus[c.dataset.col]?.session === sid).map((c) => $$(".msg-asst .parts", c).pop());
    for (const p of prose) {
      const el = p && $(".prose", p);
      if (el) el.innerHTML = `${md(msg.parts[0].text)}<span class="caret"></span>`;
    }
    if (i >= reply.length) {
      clearInterval(tick);
      msg.parts.push({ type: "tool", tool: "glob", target: "**/*.rs", status: "completed", duration: "0.1s", summary: "212 files" });
      msg.live = false;
      msg.tokens = 3_200;
      msg.cost = 0.01;
      s.state = "idle";
      s.now = { kind: "done", text: "answered · waiting for your next message" };
      s.since = Date.now();
      s.tokens += 3_200;
      s.cost += 0.01;
      patchRow(sid);
      rerenderColumnsFor(sid);
    }
  }, 40);
}

/* ───────────── composer popovers ───────────── */

const SLASH = [
  ["/compact", "Summarize to free context"],
  ["/fork", "Fork from the latest message"],
  ["/review", "Review uncommitted changes"],
  ["/init", "Create or update AGENTS.md"],
  ["/share", "Share a read-only link"],
];
function composerPopover(ta) {
  const wrap = ta.closest(".composer");
  $(".popover", wrap)?.remove();
  const v = ta.value.slice(0, ta.selectionStart);
  const at = v.match(/(^|\s)@([\w./-]*)$/);
  const sl = v.match(/^\/(\w*)$/);
  let items = [];
  let title = "";
  if (at) {
    title = "Files";
    items = D.fileTree.filter((f) => !f.dir && f.path.toLowerCase().includes(at[2].toLowerCase())).map((f) => [f.path, ""]);
  } else if (sl) {
    title = "Commands";
    items = SLASH.filter(([c]) => c.slice(1).startsWith(sl[1]));
  }
  if (!items.length) return;
  ta._pop = { items, sel: 0, at: !!at };
  wrap.insertAdjacentHTML(
    "beforeend",
    `<div class="popover" role="listbox"><header>${title}</header>${items.map(([a, b], i) => `<button class="${i === 0 ? "on" : ""}" data-pop="${i}">${esc(a)}<span>${esc(b)}</span></button>`).join("")}</div>`,
  );
}
function acceptPopover(ta, i) {
  const pop = ta._pop;
  if (!pop) return;
  const [val] = pop.items[i];
  const before = ta.value.slice(0, ta.selectionStart);
  const after = ta.value.slice(ta.selectionStart);
  const replaced = pop.at ? before.replace(/@[\w./-]*$/, `@${val} `) : `${val} `;
  ta.value = replaced + after;
  ta.selectionStart = ta.selectionEnd = replaced.length;
  ta._pop = null;
  ta.closest(".composer").querySelector(".popover")?.remove();
  ta.focus();
}

/* ───────────── events ───────────── */

document.addEventListener("click", (e) => {
  const t = e.target.closest("button, a, tr, [data-rail], .slip:not(.is-active)");
  if (!t) return;
  const d = t.dataset;

  if (d.act) {
    e.preventDefault();
    switch (d.act) {
      case "overview":
        return setView("overview");
      case "focus":
        return setView("focus");
      case "next-waiting":
        return openNextWaiting();
      case "close-others": {
        const keep = S.focus[S.activeCol];
        S.focus = [keep];
        S.activeCol = 0;
        renderWorkspace();
        return renderSide();
      }
      case "settings-back":
        return setView(S.prevView && S.prevView !== "settings" ? S.prevView : "overview");
      case "palette":
        return openPalette();
      case "theme":
        return toggleTheme();
      case "settings":
        return setView("settings");
      case "queue":
        return focusQueue();
      case "new":
        return openPalette("New session in ");
      case "server":
        S.settingsPage = "server";
        return setView("settings");
      case "keys":
        return openKeys();
      case "toggle-queue":
        S.queueOpen = !S.queueOpen;
        app.dataset.queue = S.queueOpen ? "open" : "closed";
        persist();
        return;
      case "latest": {
        const tr = t.closest(".transcript");
        tr.scrollTop = tr.scrollHeight;
        return;
      }
      case "mobile-back":
        S.mobile = "sessions";
        return renderMobileTabs();
    }
  }
  if (d.verdict) return act(d.q, d.verdict);
  if (d.jump) {
    const col = t.closest(".col");
    const slip = $(`.transcript [data-slip="${d.jump}"]`, col);
    slip?.scrollIntoView({ block: "center", behavior: reduced() ? "auto" : "smooth" });
    slip?.focus({ preventScroll: true });
    return;
  }
  if (d.answer) {
    S.answers[d.answer] = Number(d.i);
    for (const el of $$(`[data-answer="${d.answer}"]`)) el.setAttribute("aria-checked", String(el.dataset.i === d.i));
    return;
  }
  if (d.expand != null || (t.matches(".slip:not(.is-active)") && t.dataset.i != null)) {
    S.activeSlip = Number(d.expand ?? t.dataset.i);
    return renderQueue({ focusActive: true });
  }
  if (d.beside) {
    e.stopPropagation();
    return openSession(d.beside, { mode: "beside" });
  }
  if (d.rail) return openSession(d.rail, { mode: e.shiftKey ? "beside" : "replace" });
  if (d.open) {
    e.preventDefault();
    // A subagent opens next to its parent; anything else takes the active column.
    const child = SES.get(d.open)?.parent && t.closest(".col");
    return openSession(d.open, { mode: child || e.shiftKey ? "beside" : "replace" });
  }
  if (d.filter) return setFilter(d.filter);
  if (d.new) return newSession(d.new);
  if (t.matches("tr.s-row")) {
    S.cursorId = d.sid;
    return openSession(d.sid, { mode: e.shiftKey ? "beside" : "replace" });
  }
  if (d.close != null) return closeCol(Number(d.close));
  if (d.tab) {
    const c = S.focus[Number(d.colI)];
    c.tab = d.tab;
    S.activeCol = Number(d.colI);
    return renderFocus();
  }
  if (d.fileI != null) {
    const c = S.focus[S.activeCol] && S.focus[Number(t.closest(".col").dataset.col)];
    c.file = Number(d.fileI);
    return renderFocus();
  }
  if (d.file) {
    const c = S.focus[Number(t.closest(".col").dataset.col)];
    c.filePath = d.file;
    return renderFocus();
  }
  if (d.revert != null) {
    S.focus[Number(t.closest(".col").dataset.col)].confirmRevert = true;
    return renderFocus();
  }
  if (d.revertNo != null || d.revertYes != null) {
    const c = S.focus[Number(t.closest(".col").dataset.col)];
    c.confirmRevert = false;
    if (d.revertYes) {
      announce("Reverted to snapshot");
      delete D.changes[d.revertYes];
      c.tab = "transcript";
    }
    return renderFocus();
  }
  if (t.hasAttribute("data-toggle-tool")) {
    const tool = t.closest(".tool");
    tool.classList.toggle("open");
    t.setAttribute("aria-expanded", tool.classList.contains("open"));
    return;
  }
  if (t.hasAttribute("data-toggle-reason")) return t.closest(".reasoning").classList.toggle("open");
  if (d.termOpen) return openTerminal(d.termOpen);
  if (d.fork) {
    const s = SES.get(d.fork);
    const id = `f${Date.now().toString(36)}`;
    SES.set(id, { ...structuredClone(s), id, title: `${s.title} (fork)`, state: "idle", now: { kind: "done", text: "forked · ready" }, since: Date.now(), parent: undefined });
    TRANSCRIPTS.set(id, structuredClone(transcriptOf(s.id)));
    announce(`Forked ${s.title}`);
    return openSession(id);
  }
  if (d.compact) {
    const s = SES.get(d.compact);
    s.ctx = Math.round(s.ctx * 0.3);
    s.tokens = Math.round(s.tokens * 0.3);
    if (s.state === "fault") {
      s.state = "idle";
      s.now = { kind: "done", text: "compacted · ready to continue" };
      const t = transcriptOf(s.id);
      const m = lastAssistant(s.id);
      if (m) m.parts = m.parts.filter((p) => p.type !== "error");
      t.push({ role: "assistant", agent: s.agent, model: s.model, at: Date.now(), tokens: 0, cost: 0.01, parts: [{ type: "text", text: "Compacted the conversation to a summary. Context is back under budget; say **continue** to resume the port." }] });
    }
    announce(`Compacted ${s.title}`);
    patchRow(s.id);
    return rerenderColumnsFor(s.id);
  }
  if (d.agent) {
    const s = SES.get(d.sid);
    s.agent = d.agent;
    patchRow(s.id);
    return rerenderColumnsFor(s.id);
  }
  if (d.model) {
    const s = SES.get(d.model);
    const models = ["qwen3-coder-480b", "glm-4.6", "kimi-k2", "qwen3-coder-30b"];
    s.model = models[(models.indexOf(s.model) + 1) % models.length];
    patchRow(s.id);
    return rerenderColumnsFor(s.id);
  }
  if (d.send) {
    const ta = $(`textarea[data-compose="${d.send}"]`);
    const v = ta.value;
    ta.value = "";
    return sendMessage(d.send, v);
  }
  if (d.abort) {
    const s = SES.get(d.abort);
    s.state = "idle";
    s.now = { kind: "done", text: "stopped by you" };
    s.since = Date.now();
    const m = lastAssistant(s.id);
    if (m) {
      m.live = false;
      for (const p of m.parts) if (p.status === "running") ((p.status = "error"), (p.summary = "Aborted"));
    }
    patchRow(s.id);
    return rerenderColumnsFor(s.id);
  }
  if (d.settings) {
    S.settingsPage = d.settings;
    return renderSettings();
  }
  if (d.themeSet) {
    if (d.themeSet !== S.theme) toggleTheme();
    return renderSettings();
  }
  if (d.perm != null) {
    D.settings.permissions[Number(d.perm)].action = d.v;
    return renderSettings();
  }
  if (d.mobile) {
    S.mobile = d.mobile;
    if (d.mobile === "focus") S.view = "focus";
    else if (S.view !== "overview") S.view = "overview";
    renderMobileTabs();
    return renderWorkspace();
  }
  if (d.pal != null) {
    const it = S.palette.items[Number(d.pal)];
    closePalette();
    return it?.run();
  }
  if (d.pop != null) {
    const ta = $("textarea", t.closest(".composer"));
    return acceptPopover(ta, Number(d.pop));
  }
});

$("#palette").addEventListener("mousedown", (e) => {
  if (e.target.id === "palette") closePalette();
});
document.addEventListener("input", (e) => {
  if (e.target.id === "palInput") {
    S.palette.q = e.target.value;
    S.palette.sel = 0;
    drawPalette();
  }
  if (e.target.dataset.compose) {
    const ta = e.target;
    ta.style.height = "auto";
    ta.style.height = `${Math.min(220, ta.scrollHeight + 2)}px`;
    composerPopover(ta);
  }
  if (e.target.dataset.other) {
    S.answersText ??= {};
    S.answersText[e.target.dataset.other] = e.target.value.trim();
    if (e.target.value.trim()) {
      delete S.answers[e.target.dataset.other];
      for (const el of $$(`[data-answer="${e.target.dataset.other}"]`)) el.setAttribute("aria-checked", "false");
    }
  }
});
document.addEventListener("focusin", (e) => {
  const col = e.target.closest?.(".col");
  if (col && S.view === "focus") {
    const i = Number(col.dataset.col);
    if (i !== S.activeCol) {
      S.activeCol = i;
      S.focus[i].used = Date.now();
      for (const c of $$("#workspace .col")) c.classList.toggle("is-active", Number(c.dataset.col) === i);
      renderSide();
    }
  }
});

const typing = (el) => el && (el.tagName === "INPUT" || el.tagName === "TEXTAREA" || el.isContentEditable);

document.addEventListener("keydown", (e) => {
  const k = e.key;
  // Palette
  if (!$("#palette").hidden) {
    if (k === "Escape") return closePalette();
    if (k === "ArrowDown" || (e.ctrlKey && k === "n")) {
      e.preventDefault();
      S.palette.sel = Math.min(S.palette.items.length - 1, S.palette.sel + 1);
      return drawPalette();
    }
    if (k === "ArrowUp" || (e.ctrlKey && k === "p")) {
      e.preventDefault();
      S.palette.sel = Math.max(0, S.palette.sel - 1);
      return drawPalette();
    }
    if (k === "Enter") {
      e.preventDefault();
      const it = S.palette.items[S.palette.sel];
      closePalette();
      return it?.run();
    }
    return;
  }
  if (!$("#keysSheet").hidden) {
    if (k === "Escape" || k === "?") closeKeys();
    return;
  }
  if ((e.metaKey || e.ctrlKey) && k.toLowerCase() === "k") {
    e.preventDefault();
    return openPalette();
  }

  // Alt+↑/↓ steps sessions even while typing in a composer.
  if (e.altKey && (k === "ArrowDown" || k === "ArrowUp") && S.view === "focus") {
    e.preventDefault();
    return stepSession(k === "ArrowDown" ? 1 : -1);
  }
  const ae = document.activeElement;
  // Composer
  if (ae?.dataset?.compose) {
    const pop = ae._pop && ae.closest(".composer").querySelector(".popover");
    if (pop) {
      if (k === "ArrowDown" || k === "ArrowUp") {
        e.preventDefault();
        ae._pop.sel = (ae._pop.sel + (k === "ArrowDown" ? 1 : -1) + ae._pop.items.length) % ae._pop.items.length;
        $$("button", pop).forEach((b, i) => b.classList.toggle("on", i === ae._pop.sel));
        return;
      }
      if (k === "Enter" || k === "Tab") {
        e.preventDefault();
        return acceptPopover(ae, ae._pop.sel);
      }
      if (k === "Escape") {
        ae._pop = null;
        return pop.remove();
      }
    }
    if (k === "Enter" && !e.shiftKey) {
      e.preventDefault();
      const v = ae.value;
      ae.value = "";
      ae.style.height = "auto";
      return sendMessage(ae.dataset.compose, v);
    }
    if (k === "Escape") return ae.blur();
    return;
  }
  // Question free text: Enter submits.
  if (ae?.dataset?.other && k === "Enter") {
    e.preventDefault();
    return act(ae.dataset.other, "answer");
  }
  // Terminal input line
  if (ae?.dataset?.termInput != null && k === "Enter") {
    e.preventDefault();
    const i = Number(ae.dataset.termInput);
    const c = S.focus[i];
    const cmd = ae.value;
    c.lines.push(`$ ${cmd}`);
    if (cmd.trim()) c.lines.push(`(prototype) ${cmd.trim().split(" ")[0]}: output streams from the PTY here`);
    renderFocus();
    const input = $(`[data-term-input="${i}"]`);
    input?.focus();
    const out = input?.closest(".col")?.querySelector(".term");
    if (out) out.scrollTop = out.scrollHeight;
    return;
  }
  if (typing(ae)) return;
  if (e.metaKey || e.ctrlKey || e.altKey) return;

  if (k === "?") return openKeys();
  if (k === "/") {
    e.preventDefault();
    return openPalette();
  }
  if (k === "Escape") {
    if (S.view === "settings") return setView(S.prevView && S.prevView !== "settings" ? S.prevView : "overview");
    if (S.view !== "overview") return setView("overview");
    return;
  }

  // Queue keys apply when focus is inside the queue or an inline slip.
  const slipEl = ae?.closest?.(".slip");
  if (slipEl || ae?.closest?.(".queue")) {
    const q = slipEl ? S.queue.find((x) => x.id === slipEl.dataset.slip) : S.queue[S.activeSlip];
    const lower = k.toLowerCase();
    if (q && q.kind !== "question") {
      if (lower === "a") return act(q.id, "once");
      if (lower === "s") return act(q.id, "always");
      if (lower === "d") return act(q.id, "reject");
    }
    if (q && q.kind === "question") {
      if (/^[1-9]$/.test(k) && q.options[Number(k) - 1]) {
        S.answers[q.id] = Number(k) - 1;
        for (const el of $$(`[data-answer="${q.id}"]`)) el.setAttribute("aria-checked", String(Number(el.dataset.i) === S.answers[q.id]));
        return;
      }
      if (k === "Enter") return act(q.id, "answer");
      if (lower === "x") return act(q.id, "dismiss");
    }
    if ((lower === "j" || k === "ArrowDown") && !slipEl?.classList.contains("inline")) {
      e.preventDefault();
      S.activeSlip = Math.min(S.queue.length - 1, S.activeSlip + 1);
      return renderQueue({ focusActive: true });
    }
    if ((lower === "k" || k === "ArrowUp") && !slipEl?.classList.contains("inline")) {
      e.preventDefault();
      S.activeSlip = Math.max(0, S.activeSlip - 1);
      return renderQueue({ focusActive: true });
    }
    if (lower === "o" && q) return openSession(q.session);
  }

  const lower = k.toLowerCase();
  if (lower === "q") {
    e.preventDefault();
    return focusQueue();
  }
  if (lower === "n") {
    e.preventDefault();
    return openPalette("New session in ");
  }
  if (S.view === "overview") {
    if (lower === "j" || k === "ArrowDown") {
      e.preventDefault();
      return moveCursor(1);
    }
    if (lower === "k" || k === "ArrowUp") {
      e.preventDefault();
      return moveCursor(-1);
    }
    if (k === "Enter" && S.cursorId) return openSession(S.cursorId, { mode: e.shiftKey ? "beside" : "replace" });
    if (lower === "f" && S.focus.length) return setView("focus");
  }
  if (S.view === "focus") {
    if (lower === "w") return openNextWaiting();
    if (k === "[" || k === "]") {
      S.activeCol = (S.activeCol + (k === "]" ? 1 : -1) + S.focus.length) % S.focus.length;
      for (const c of $$("#workspace .col")) c.classList.toggle("is-active", Number(c.dataset.col) === S.activeCol);
      return renderSide();
    }
    if (lower === "i") {
      e.preventDefault();
      return $(`#workspace .col[data-col="${S.activeCol}"] textarea`)?.focus();
    }
  }
});

/* ───────────── simulation ───────────── */

function later(ms, fn) {
  setTimeout(fn, ms);
}

function stepActivity() {
  const working = [...SES.values()].filter((s) => s.state === "working" && D.activity[s.id]);
  if (!working.length) return;
  const s = pick(working);
  const script = D.activity[s.id];
  const i = (S.activityIdx[s.id] = ((S.activityIdx[s.id] ?? 0) + 1) % script.length);
  const [kind, text] = script[i];
  s.now = { kind, text };
  s.cost += rand(0.004, 0.03);
  s.tokens += Math.round(rand(400, 2400));
  s.ctx = Math.min(96, s.ctx + (Math.random() < 0.4 ? 1 : 0));
  if (s.todos && Math.random() < 0.08 && s.todos[0] < s.todos[1] - 1) s.todos = [s.todos[0] + 1, s.todos[1]];
  S.lastFrame = Date.now();
  patchRow(s.id);
  if (TRANSCRIPTS.has(s.id) && s.id !== "s1") {
    const m = lastAssistant(s.id);
    if (m) {
      for (const p of m.parts) if (p.status === "running") ((p.status = "completed"), (p.duration = `${rand(0.1, 4).toFixed(1)}s`));
      const last = m.parts.at(-1);
      if (kind === "writing" && last?.type === "text" && m.live) last.text = text;
      else m.parts.push(livePart(s.now));
      m.live = true;
      if (m.parts.length > 40) m.parts.splice(0, m.parts.length - 40);
      if (S.focus.some((c) => c.session === s.id)) rerenderColumnsFor(s.id);
    }
  }
  // Keep open column meta live without rebuilding the transcript.
  for (const col of $$("#workspace .col")) {
    const c = S.focus[Number(col.dataset.col)];
    if (c?.session !== s.id) continue;
    const l2 = $(".col-head .l2", col);
    if (l2) l2.innerHTML = `<span class="grow">${esc(PROJ.get(s.project).name)} · ${esc(s.agent)} · ${esc(s.model)}${s.branch ? ` · ${esc(s.branch)}` : ""}</span>${ctxCell(s)}<span>${money(s.cost)}</span><span>${ktok(s.tokens)} tok</span>`;
  }
  renderStatus();
}

function streamBash() {
  const sid = "s1";
  const s = SES.get(sid);
  if (s.state !== "working") return;
  const hit = findPart(sid, (p) => p.stream && p.status === "running");
  if (!hit) return;
  const i = S.stream[sid]++;
  if (i < D.bashStream.length) {
    const line = D.bashStream[i];
    hit.p.output.push(line);
    for (const out of $$(`[data-stream="${sid}"]`)) {
      out.insertAdjacentHTML("beforeend", `\n${outLine(line)}`);
      out.scrollTop = out.scrollHeight;
      const tr = out.closest(".transcript");
      if (tr && S.stick[tr.dataset.key] !== false) tr.scrollTop = tr.scrollHeight;
    }
  } else if (i === D.bashStream.length) {
    hit.p.output.push("────────────", "     Summary [  41.207s] 412 tests run: 411 passed, 1 failed, 0 skipped");
    hit.p.status = "completed";
    hit.p.duration = "41.2s";
    hit.m.parts.push({
      type: "text",
      text: "One failure: `prompt_streams_parts` expects `message.part.delta` before `message.part.updated`, but the adapter mirrors the projection first. Reordering the mirror after dispatch.",
    });
    hit.m.parts.push({ type: "tool", tool: "edit", target: "crates/alforria-server/src/projection.rs", status: "completed", duration: "0.2s", add: 6, del: 4 });
    s.now = { kind: "edit", text: "crates/alforria-server/src/projection.rs" };
    patchRow(sid);
    rerenderColumnsFor(sid);
  }
}

function tickClocks() {
  const now = Date.now();
  for (const el of $$("[data-since]")) {
    const since = Number(el.dataset.since);
    el.textContent = `${el.closest(".inline") ? "waiting " : ""}${waited(now - since)}`;
  }
  for (const el of $$("[data-age]")) {
    const s = SES.get(el.dataset.age);
    if (s) el.textContent = age(now - s.since);
  }
  const lf = $("#lastFrame");
  if (lf) lf.textContent = `${((now - S.lastFrame) / 1000).toFixed(1)}s`;
  const small = $("#band .wait-count small");
  if (small && S.queue.length) small.textContent = `oldest ${waited(now - Math.min(...S.queue.map((q) => q.since)))}`;
  // retry countdown
  const r = SES.get("s8");
  if (r.state === "retry") {
    r.retryIn = (r.retryIn ?? 8) - 1;
    if (r.retryIn <= 0) {
      r.state = "working";
      r.now = { kind: "bash", text: "cargo bench --bench ingestion" };
      D.activity.s8 = [["bash", "cargo bench --bench ingestion"], ["writing", "Batching inserts cut p99 from 41 ms to 12 ms…"], ["read", "benches/ingestion.rs"]];
    } else r.now = { kind: "retry", text: `rate limited · attempt 2 in ${r.retryIn}s` };
    for (const el of $$('[data-retry="s8"]')) el.textContent = r.state === "retry" ? `next in ${r.retryIn}s` : "retrying now";
    patchRow("s8");
  }
}

function scheduleArrivals() {
  for (const a of D.arrivals) {
    later(a.at, () => {
      const item = { ...a.item, since: Date.now() };
      S.queue.push(item);
      const s = SES.get(item.session);
      s.state = "waiting";
      s.now = a.now;
      s.since = Date.now();
      if (TRANSCRIPTS.has(s.id)) {
        const m = lastAssistant(s.id);
        for (const p of m?.parts ?? []) if (p.status === "running") ((p.status = "completed"), (p.duration = "0.4s"));
        m?.parts.push(pendingPart(item));
        if (m) m.live = false;
        rerenderColumnsFor(s.id);
      }
      const typingInQueue = document.activeElement?.closest?.(".queue");
      renderSide({ arriving: item.id, focusActive: !!typingInQueue });
      renderBand();
      if (!reduced()) $("#band .wait-count")?.classList.add("flash");
      renderStatus();
      renderMobileTabs();
      patchRow(s.id);
      announce(`${s.title} is waiting on you`);
    });
  }
}

/* ───────────── boot ───────────── */

app.dataset.queue = S.queueOpen ? "open" : "closed";
S.mobile = S.queue.length ? "queue" : "sessions";
// The first render must not overwrite a deep-linked hash before it is read.
S.routing = true;
renderAll();
S.routing = false;
setInterval(stepActivity, 1300);
setInterval(streamBash, 900);
setInterval(tickClocks, 1000);
scheduleArrivals();

let lastW = innerWidth;
addEventListener("resize", () => {
  if (Math.abs(innerWidth - lastW) < 40) return;
  lastW = innerWidth;
  const m = maxCols();
  if (S.focus.length > m && !mobile()) {
    S.focus = S.focus.slice(-m);
    S.activeCol = Math.min(S.activeCol, S.focus.length - 1);
  }
  renderWorkspace();
});

// Deep link for review captures: ?view=focus&open=s2,s1
const params = new URLSearchParams(location.search);
if (params.get("theme")) {
  S.theme = params.get("theme");
  document.documentElement.dataset.theme = S.theme;
  renderBand();
}
if (params.get("open")) {
  for (const id of params.get("open").split(",")) if (SES.has(id)) openSession(id, { mode: "beside" });
} else if (location.hash && location.hash !== "#/") applyHash();
else history.replaceState(null, "", "#/");
if (params.get("mobile")) {
  S.mobile = params.get("mobile");
  renderMobileTabs();
}
if (params.get("view") === "settings") setView("settings");
