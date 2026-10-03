---
version: 1
slug: "web"
primary_target: "web"
related_targets: ["web/prototype"]
---

# Surface brief: alforria web client

## Scope and mode

The whole web client served by `alforria serve`:
- the fleet overview (interrupt queue + session table);
- focus mode (1–3 columns of session, review, terminal or file views);
- settings.

Mode: **Operate**. The current round is an interactive HTML prototype with synthetic data (`web/prototype/`), which the SolidJS build implements afterwards.

## Audience and job

A developer supervising a fleet of 7+ concurrent agent sessions (plus subagents) across several projects:
- from a laptop over the LAN, from localhost, on a big monitor for hours, or from a phone for check-ins;
- their job is to unblock waiting agents fast, keep sight of every running one, and dive into one or two when needed.

Hierarchy: project → session. A machine level (LibertAI cloud) may attach above it later via an injectable server-switcher slot in the band.

## Constraints

- Wire format: v1 API + `/global/event`.
- One store: a permission or question exists once, however many places show it.
- Full keyboard operation; WCAG AA in both themes.
- Status is never carried by colour alone.
- Phone layout must support approving and answering.

## Direction contract

**THESIS.** Supervising agents means clearing a queue and scanning a table. Waiting agents are drained from one keyboard queue, and every session is one live row. The surface refuses the category's grid of chat-preview cards and its one-chat-plus-sidebar layout.

**OWN-WORLD.**
- Colour: cool near-black ground, slightly lifted plates, hairline cool-gray rules, pale ink. Hazard orange `#E25303` appears only where something waits on the human, with ink on orange.
- Type: Barlow Condensed caps for labels and titles, system sans for prose, mono only for data and code.
- Shape: square corners, 1px rules, no shadows or gradients.
- Light theme: the website's paper/ink.

**STORY.** On arrival the band already says how many agents are waiting. The oldest waiting item is open at the top of the queue. One key clears it and the next opens. The session table shows every other session's live tool, todos, context and cost. Enter opens any row in focus.

**FIRST VIEWPORT** (1440×900):
- 44px band: wordmark, server designation, ⌘K, and the orange WAITING counter.
- Left 400px: the queue. The active slip is expanded, with its command, diff or question and stamped A/S/D or 1–9 keys.
- Right: the session table, grouped by project with subagents nested, 32px rows.
- 24px status line.

**FORM.** Interrupt queue + session table, position 3 of 7, seed key `fa4230a5`.

**SIGNATURE.** Acting on a slip stamps it (registration offset, then the verdict and time), collapses it, opens the next, and decrements the band counter. The motion is immediate fills plus a single 150ms stamp.

**FINISH.** unreviewed and undocumented is unfinished; this build ends with the finish review, the verdict, DESIGN.md, and every shipping raster carrying its provenance.

## Additions accepted at finish review

These elements are not in the contract; the finish review accepted them as supporting the signature.

- **Cleared ledger.** A collapsible log at the foot of the queue: time, verdict, session and what was cleared.
- **All clear.** The empty queue's reward state, with a working / idle / cleared / last tally.
- **Column band.** The orange band on a waiting session's focus column head. It jumps to the inline slip.
- **Keys sheet.** Opened with `?`, serving the full-keyboard requirement.

## Unresolved

- Cloud machine switcher: design deferred to the LibertAI product.
- Icon set: authored 1.5-stroke SVG in the prototype; a library choice for the build.
