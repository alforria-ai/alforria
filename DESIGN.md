---
name: alforria web
description: 'The operating datasheet: a dark-first fleet console where hazard orange means exactly one thing, an agent waiting on you.'
colors:
  ground: '#0b0e10'
  plate: '#121619'
  plate-2: '#181d21'
  plate-3: '#1f262b'
  rule: '#232b30'
  rule-soft: '#1a2125'
  rule-2: '#35414a'
  ink: '#e3e8ea'
  ink-2: '#a3aeb4'
  ink-3: '#7d8a91'
  accent: '#e25303'
  accent-ink: '#0b0e10'
  accent-text: '#ff7a33'
  accent-veil: 'rgba(226, 83, 3, 0.09)'
  accent-veil-2: 'rgba(226, 83, 3, 0.18)'
  add-bg: 'rgba(84, 170, 110, 0.13)'
  add-mark: '#8fd19e'
  del-bg: 'rgba(226, 92, 80, 0.13)'
  del-mark: '#f0a39b'
  scrim: 'rgba(4, 6, 7, 0.62)'
  ground-light: '#f5f7f8'
  plate-light: '#e8edef'
  plate-2-light: '#dde4e7'
  plate-3-light: '#d2dade'
  rule-light: '#cfd7db'
  rule-soft-light: '#dfe5e8'
  rule-2-light: '#b7c0c5'
  ink-light: '#14191c'
  ink-2-light: '#4b555c'
  ink-3-light: '#5e6a72'
  accent-ink-light: '#14191c'
  accent-text-light: '#b84300'
  accent-veil-light: 'rgba(226, 83, 3, 0.08)'
  accent-veil-2-light: 'rgba(226, 83, 3, 0.16)'
  add-bg-light: 'rgba(40, 140, 70, 0.12)'
  add-mark-light: '#1d6b35'
  del-bg-light: 'rgba(200, 50, 40, 0.11)'
  del-mark-light: '#a3271c'
  scrim-light: 'rgba(20, 25, 28, 0.28)'
typography:
  wordmark:
    fontFamily: "'Barlow Condensed', 'Arial Narrow', sans-serif"
    fontSize: 25px
    fontWeight: 800
    lineHeight: 1
    letterSpacing: -0.005em
  display:
    fontFamily: "'Barlow Condensed', 'Arial Narrow', sans-serif"
    fontSize: 64px
    fontWeight: 800
    lineHeight: 0.85
    letterSpacing: -0.01em
  verdict:
    fontFamily: "'Barlow Condensed', 'Arial Narrow', sans-serif"
    fontSize: 30px
    fontWeight: 800
    lineHeight: 0.9
    letterSpacing: 0.01em
  counter:
    fontFamily: "'Barlow Condensed', 'Arial Narrow', sans-serif"
    fontSize: 26px
    fontWeight: 800
    lineHeight: 1
  headline:
    fontFamily: "'Barlow Condensed', 'Arial Narrow', sans-serif"
    fontSize: 22px
    fontWeight: 700
    lineHeight: 1.05
  title:
    fontFamily: "'Barlow Condensed', 'Arial Narrow', sans-serif"
    fontSize: 19px
    fontWeight: 700
    lineHeight: 1.05
    letterSpacing: 0.015em
  pane-title:
    fontFamily: "'Barlow Condensed', 'Arial Narrow', sans-serif"
    fontSize: 16px
    fontWeight: 700
    lineHeight: 1
    letterSpacing: 0.025em
  control:
    fontFamily: "'Barlow Condensed', 'Arial Narrow', sans-serif"
    fontSize: 14px
    fontWeight: 700
    lineHeight: 1
    letterSpacing: 0.03em
  label:
    fontFamily: "'Barlow Condensed', 'Arial Narrow', sans-serif"
    fontSize: 13px
    fontWeight: 700
    lineHeight: 1
    letterSpacing: 0.03em
  column-head:
    fontFamily: "'Barlow Condensed', 'Arial Narrow', sans-serif"
    fontSize: 11.5px
    fontWeight: 700
    lineHeight: 1
    letterSpacing: 0.05em
  body:
    fontFamily: "system-ui, -apple-system, 'Segoe UI', sans-serif"
    fontSize: 13.5px
    fontWeight: 400
    lineHeight: 1.5
    fontFeature: tnum
  prose:
    fontFamily: "system-ui, -apple-system, 'Segoe UI', sans-serif"
    fontSize: 14px
    fontWeight: 400
    lineHeight: 1.62
  data:
    fontFamily: "ui-monospace, 'SF Mono', 'Cascadia Code', 'JetBrains Mono', Menlo, Consolas, monospace"
    fontSize: 12px
    fontWeight: 400
    lineHeight: 1.55
  data-small:
    fontFamily: "ui-monospace, 'SF Mono', 'Cascadia Code', 'JetBrains Mono', Menlo, Consolas, monospace"
    fontSize: 11px
    fontWeight: 400
    lineHeight: 1
  state:
    fontFamily: "ui-monospace, 'SF Mono', 'Cascadia Code', 'JetBrains Mono', Menlo, Consolas, monospace"
    fontSize: 10.5px
    fontWeight: 500
    lineHeight: 1
    letterSpacing: 0.06em
rounded:
  square: '0'
  lamp: '50%'
spacing:
  band-h: 44px
  status-h: 24px
  queue-w: 400px
  row-h: 32px
  gutter: 16px
  cell: 8px
components:
  wait-count:
    textColor: '{colors.ink-3}'
    typography: '{typography.counter}'
    rounded: '{rounded.square}'
    padding: 0 16px
    height: '{spacing.band-h}'
  wait-count-lit:
    backgroundColor: '{colors.accent}'
    textColor: '{colors.accent-ink}'
  slip-head:
    textColor: '{colors.ink-2}'
    typography: '{typography.label}'
    height: 30px
    padding: 0 12px 0 16px
  slip-head-active:
    backgroundColor: '{colors.accent}'
    textColor: '{colors.accent-ink}'
  slip-head-stamped:
    backgroundColor: '{colors.ink}'
    textColor: '{colors.ground}'
  stamp:
    textColor: '{colors.ink}'
    typography: '{typography.control}'
    rounded: '{rounded.square}'
    padding: 0 10px
    height: 40px
  stamp-hover:
    backgroundColor: '{colors.plate-2}'
  stamp-primary:
    backgroundColor: '{colors.ink}'
    textColor: '{colors.ground}'
  stamp-primary-hover:
    backgroundColor: '{colors.ink-2}'
  verdict-stamp:
    backgroundColor: '{colors.ground}'
    textColor: '{colors.ink}'
    typography: '{typography.verdict}'
    padding: 9px 18px 10px
  send:
    backgroundColor: '{colors.ink}'
    textColor: '{colors.ground}'
    typography: '{typography.control}'
    rounded: '{rounded.square}'
    padding: 0 12px
    height: 30px
  send-hover:
    backgroundColor: '{colors.ink-2}'
  send-stop:
    textColor: '{colors.ink}'
  link-btn:
    textColor: '{colors.ink-2}'
    typography: '{typography.label}'
    padding: 4px 6px
  link-btn-hover:
    backgroundColor: '{colors.ink}'
    textColor: '{colors.ground}'
  seg-button:
    textColor: '{colors.ink-2}'
    typography: '{typography.label}'
    padding: 0 9px
    height: 24px
  seg-button-pressed:
    backgroundColor: '{colors.ink}'
    textColor: '{colors.ground}'
  tab:
    textColor: '{colors.ink-3}'
    typography: '{typography.label}'
    height: 30px
  tab-selected:
    textColor: '{colors.ink}'
  session-row:
    textColor: '{colors.ink}'
    typography: '{typography.body}'
    height: '{spacing.row-h}'
    padding: 0 8px
  session-row-hover:
    backgroundColor: '{colors.plate}'
  session-row-waiting:
    backgroundColor: '{colors.accent-veil}'
  session-row-waiting-hover:
    backgroundColor: '{colors.accent-veil-2}'
  session-row-cursor:
    backgroundColor: '{colors.plate-2}'
  col-band:
    backgroundColor: '{colors.accent}'
    textColor: '{colors.accent-ink}'
    typography: '{typography.label}'
    height: 28px
    padding: 0 12px 0 16px
  plate:
    backgroundColor: '{colors.plate}'
    textColor: '{colors.ink}'
    typography: '{typography.data}'
    rounded: '{rounded.square}'
    padding: 8px 10px
  composer-input:
    backgroundColor: '{colors.plate}'
    textColor: '{colors.ink}'
    typography: '{typography.prose}'
    rounded: '{rounded.square}'
    padding: 9px 11px
  composer-input-focus:
    backgroundColor: '{colors.plate-2}'
  kbd:
    textColor: '{colors.ink-3}'
    typography: '{typography.data-small}'
    padding: 2px 4px
  pill-state:
    textColor: '{colors.ink-3}'
    typography: '{typography.state}'
    padding: 4px 6px
  pill-state-on:
    backgroundColor: '{colors.ink}'
    textColor: '{colors.ground}'
  settings-nav-item:
    textColor: '{colors.ink-3}'
    typography: '{typography.control}'
    height: 34px
    padding: 0 16px
  settings-nav-item-current:
    backgroundColor: '{colors.ink}'
    textColor: '{colors.ground}'
  fault-plate-head:
    backgroundColor: '{colors.ink}'
    textColor: '{colors.ground}'
    typography: '{typography.label}'
    height: 30px
  mobile-tab-badge:
    backgroundColor: '{colors.accent}'
    textColor: '{colors.accent-ink}'
    padding: 2px 5px
---

# Design System: alforria web

## Overview

**Creative North Star: "The Operating Datasheet"**

The alforria website is a component datasheet: an orange identifier band, condensed caps, numbered specifications and square stamped controls on cool paper. The web client is that same datasheet put into service. It is dense and dark-first, read for hours, and its job is to clear a queue of waiting agents and scan a table of live ones. It keeps the website's materials and inverts their emphasis. On the website orange announces the product. Here orange is silent until an agent needs the human, and then it is the only orange on screen.

The surface is flat and ruled. A cool near-black ground carries slightly lifted plates, and 1px cool-gray rules divide every region, row and cell. Pale ink carries the text. Barlow Condensed caps name things (panes, columns, tools, verdicts, controls). System sans carries sentences. Mono carries only what is data or code: paths, commands, costs, ages, token counts and state codes. Density is set by a 32px row and a 16px gutter. Motion is a grammar of immediate fills plus one signature: the 150ms registration-offset stamp that marks a cleared slip.

This record was taken from the shipped prototype: `web/prototype/proto.css` (tokens on `:root`, light theme on `:root[data-theme="light"]`), `app.js`, the icon sprite in `index.html`, and the self-hosted fonts in `web/prototype/fonts/`. Brand lineage: `~/repos/alforria-website/DESIGN.md`. Every color key in the frontmatter maps to a CSS custom property of the same name prefixed with `--`. Keys ending in `-light` are the same property's value under `[data-theme="light"]`, and `accent` is identical in both themes. The SolidJS build should keep these property names unchanged.

**Key Characteristics:**

- Dark-first, with a light theme that returns to the website's paper and ink.
- Orange means "waiting on you" and nothing else, always with ink on top.
- Square corners, 1px rules, and flat tonal plates. No drop shadows and no gradients.
- Condensed caps for names, system sans for prose, and mono only for data and code.
- Ink-filled controls: the primary action is an ink stamp, never an orange button.
- One signature motion: the registration-offset stamp.

## Colors

The palette is cool engineering neutrals stepped in small tonal increments, plus one hazard orange held in reserve and two functional diff tints.

### Primary

- **Hazard Orange** (`accent`): the brand orange inherited from the website. It fills the lit WAITING counter in the band, the head of the active queue slip, the column band on a waiting session's focus column, the inline slip's head and border, the waiting lamp, the pending-permission tool mark, and the mobile Queue badge.
- **Ink on Orange** (`accent-ink`): the foreground on every orange fill. It equals the ground in dark (`#0b0e10`) and the ink in light (`#14191c`), so orange always carries near-black text.
- **Orange Text** (`accent-text`): a lightened (dark theme) or deepened (light theme) orange used where waiting state is set as text on the ground: the WAITING state code, the session table's "now" cell for waiting rows, and project tallies ("2 waiting"). It exists to meet AA as text. The fill orange is never used for text on the ground.
- **Orange Veils** (`accent-veil`, `accent-veil-2`): 9% and 18% washes behind waiting session rows (rest and hover).

### Neutral

- **Fleet Ground** (`ground`): the page background, sticky table heads, and the background of the palette, keys sheet, popover and verdict stamp. It is also the reversed text on ink fills.
- **Plates** (`plate`, `plate-2`, `plate-3`): three lifted tonal steps. `plate` is the hover fill, code/command plates, the composer, and the terminal and output wells. `plate-2` is the cursor row, selected list items, inline code, the focused composer, and stamp hover. `plate-3` is the top step of the ramp.
- **Hairlines** (`rule`, `rule-soft`, `rule-2`): `rule` divides regions and plates, `rule-soft` divides table rows, and `rule-2` is the stronger rule for control outlines, kbd keys, segmented buttons, the cursor-row frame, the subagent tree connector, and scrollbars.
- **Ink** (`ink`, `ink-2`, `ink-3`): primary text, then secondary text (meta, tool targets, verdict times), then tertiary text (column heads, ages, placeholders, idle state). `ink` is also the fill for every primary control, the selected state, the stamped slip head and the fault badge, and the 2px focus outline.

### Functional (diff only)

- **Add** (`add-bg`, `add-mark`) and **Delete** (`del-bg`, `del-mark`): a muted green and a muted red. They are used only for diff line tints, the +/- gutter marks, and the "+14 −5" counts beside changed files and edit tools.
- **Scrim** (`scrim`): the veil behind the command palette and the keys sheet.

### Named Rules

**The One Signal Rule.** Orange means "an agent is waiting on the human" and nothing else. If the waiting state goes away, the orange goes with it: after the last slip clears, the band counter returns to ink-3 on the ground. Primary actions, selection, focus, progress and errors never use orange.

**The Ink-on-Orange Rule.** Every orange fill carries `accent-ink`. Orange text on the ground uses `accent-text`, never `accent`.

**The Diff-Tint Rule.** Green and red exist only to describe added and deleted lines. They never mark status, success, failure or cost.

**The Never-Colour-Alone Rule.** Every state pairs its colour with a shape and a word. Lamps differ by form: filled for waiting, filled with a pulsing ring for working, half-filled for retry, hollow for idle, and slashed for fault. They always sit next to a mono state code (WAITING, WORKING, RETRY, IDLE, FAULT).

## Typography

**Display / Label Font:** Barlow Condensed 700 and 800, self-hosted WOFF2 (OFL), `font-display: swap`, falling back to Arial Narrow and then sans-serif.
**Body Font:** system-ui, falling back to -apple-system, Segoe UI and sans-serif. Body text uses tabular numerals throughout.
**Mono Font:** ui-monospace, falling back to SF Mono, Cascadia Code, JetBrains Mono, Menlo and Consolas. No download.

**Character:** Condensed industrial caps label the machine, quiet system text explains it, and mono reports its readings. The three faces never swap roles.

### Hierarchy

- **Wordmark** (800, 25px): the lowercase `alforria` in the band. It is the only lowercase use of Barlow.
- **Display** (800, 64px, 0.85 line-height, uppercase): the "ALL CLEAR" reward when the queue empties.
- **Verdict** (800, 30px, uppercase): the word on the stamp (ALLOWED ONCE, ALWAYS ALLOWED, DENIED, CONTINUED, STOPPED, ANSWERED, DISMISSED).
- **Counter** (800, 26px): the band's waiting count and, at 22px, the collapsed rail count.
- **Headline** (700, 22px, uppercase): settings section titles, the keys sheet title, and empty-queue headings.
- **Title** (700, 19px, uppercase): focus column session titles. They are ellipsized on one line, and clamped to two lines on mobile.
- **Pane title** (700, 16px, uppercase): pane heads ("WAITING ON YOU", "SESSIONS") and project group names in the table.
- **Control** (700, 14px, uppercase): stamps, Send, New session, and settings navigation items.
- **Label** (700, 12 to 13px, uppercase, 0.03 to 0.05em tracking): slip heads, tool names, tabs, segmented buttons, message labels, and the column band.
- **Column head** (700, 11.5px, uppercase, 0.05em): table headers, palette group heads, and the provider table head.
- **Body** (400, 13.5px/1.5): the default UI text. Session titles use weight 500 and subagent titles use 400 in ink-2.
- **Prose** (400, 14px/1.62, max 74ch): assistant text in transcripts. User messages run to 78ch on a plate and reasoning to 72ch.
- **Data** (mono 400, 12 to 12.5px, 1.55 to 1.6): code, diffs, terminal output, command plates, file trees and tool targets.
- **Data small** (mono 400, 11 to 11.5px): paths, branches, ages, costs, token counts, kbd keys and the status line.
- **State** (mono 500, 10.5px, uppercase, 0.06em): lamp state codes and state pills.

### Named Rules

**The Three Voices Rule.** Barlow names, sans explains, mono measures. A sentence is never set in mono. Question text and permission explanations use sans even inside slips. A path, command, cost or count is never set in sans.

**The Caps-Only-Condensed Rule.** Barlow Condensed is always uppercase, with the single exception of the wordmark. Inline mono inside a caps label keeps its own case and drops the tracking.

**The Mid-Token Wrap Rule.** Code never breaks inside an identifier. When a diff line wraps inside a narrow slip, break opportunities are inserted only at token boundaries (after `::`, `.`, `(` and `,`, and around spaced operators). Continuation lines hang 2ch past the line's own indent. Wide views scroll horizontally and do not wrap.

## Layout

The app is a three-row grid: band (`band-h`, 44px), main, and status line (`status-h`, 24px). Main is a two-column grid: the queue (`queue-w`, 400px) and the workspace. Neither the band nor the status line scrolls, while each pane scrolls on its own. Collapsing the queue reduces it to a 48px rail that shows the waiting count, lit orange when it is above zero.

- **Band:** a run of ruled cells, left to right: wordmark, server designation (mono), a centered ⌘K search field up to 460px, New session, theme, settings, and the waiting counter (min 176px) at the far right.
- **Queue:** a 38px pane head, then slips stacked in oldest-first order with 1px rules between them, then the collapsible cleared ledger at the foot (max 34% of the height).
- **Workspace:** a 38px pane head with segmented filters (ALL / WAITING / WORKING / IDLE) and view toggles (OVERVIEW / FOCUS). The body is either the session table or focus columns.
- **Session table:** fixed layout, 32px rows, 40px project group rows with the name, path, branch and tally, a sticky 28px header, and subagents nested under an L-shaped connector in `rule-2`.
- **Focus:** one to three columns, each with a minimum width of 400px and divided by 1px rules. Each column has a head (title, mono meta line, tabs), a body (transcript, changes, files or terminal) and a composer. The active column is marked by a 2px inset ink line at its top.
- **Settings:** a 220px navigation rail and a spec sheet up to 860px wide. Spec rows have a 1px ink top rule and `rule` dividers, with a minimum height of 44px.

**Rhythm.** The left inset is 16px (`gutter`) everywhere: pane heads, slips, table edges, column heads and the composer. Cells use 8px (`cell`) padding, and controls use internal gaps of 8 to 12px. There is no separate spacing scale: component dimensions are the system.

### Responsive

- **≤1280px:** `queue-w` drops to 360px, the server address hides (the designation keeps its version), and the table re-weights the session and todos columns.
- **≤1080px:** the context column hides, New session collapses to its icon and key, and focus columns may shrink to 380px.
- **≤760px:** single pane. `band-h` grows to 48px, the status line is replaced by a 56px bottom tab bar (Queue / Sessions / Focus, with an orange count badge on Queue), and ⌘K collapses to an icon cell. Stamps grow to 50px tall and drop their kbd hints. Table rows become two-line grid cards (lamp, title and age; then now; then todos, context and cost). Waiting rows keep their veil. Focus shows only the active column with a back button, settings navigation becomes a horizontal strip, and the file tree stacks above the code.
- **Container ≤560px (focus column):** the composer hides its key hint.

## Elevation & Depth

The system is flat. There are no drop shadows, blurs or gradients. Depth comes from tonal steps (ground, then plate, plate-2, plate-3) and from 1px rules. Overlays (palette, keys sheet, popover) sit on `ground` with a `rule-2` border, and the full-screen `scrim` sits behind them.

`box-shadow` appears only as inset hairlines, never as lift:

- **Cursor frame** (`inset 0 ±1px 0 var(--rule-2)`, plus the side edges on the first and last cells): the keyboard cursor row in the table.
- **Active column** (`inset 0 2px 0 var(--ink)`): the focused focus column, and the current mobile tab.
- **Palette selection** (`inset 0 0 0 1px var(--rule-2)`): the selected palette result.

### Named Rules

**The Flat Plate Rule.** If something needs to stand forward, give it a tonal step or an ink rule, never a shadow. The only offset in the system is the stamp's registration line, and it is a 1px rule, not a shadow.

## Shapes

Every corner is square (`rounded.square`). This applies to buttons, plates, inputs, slips, overlays, pills, todo cells, meters and scrollbar thumbs. The one round form is the status lamp (`rounded.lamp`): a 9px circle. It is the datasheet's indicator light, and its states are told apart by fill, ring, half-fill and slash. Recurring geometry includes:

- 1px borders throughout. Ink 1px top rules mark spec-sheet heads (settings, keys sheet, all-clear tally).
- 2px ink outlines for focus and the verdict stamp.
- Small square glyph forms: 7px tool marks, 6×10px todo cells, 10px todo boxes, and the 40×4px context meter.

Icons are an authored 16px SVG sprite: 1.5px stroke, square caps, miter joins, no fill except small solid blocks. The icon library for the build is still undecided, and any replacement must match this drawing language.

## Components

### Waiting Counter (signature)

The band's rightmost cell. At rest it shows ink-3 on the ground. While anything waits it is **lit**: an `accent` fill with `accent-ink` text, the count in the counter style, "WAITING" in label caps, and "oldest 9m 00s" in mono below. On hover or focus a 1px `accent-ink` outline appears, inset 4px. When the count changes, the numeral ticks in from above (220ms). When a new item arrives, the cell flashes to ink once (520ms, two hard steps). Activating it opens the oldest slip.

### Queue Slip (signature)

A slip is a ruled block in the queue. Collapsed, it shows a 30px head (lamp, kind such as "PERMISSION · BASH", and age in mono) and a one-line summary: the command in mono, or the question in sans, with the session and project below in ink-3. The **active** slip's head fills orange and expands into:

- context: the session link, and mono meta for project, agent and model;
- body: a sans explanation, a command `plate`, a diff, or a question with numbered options;
- a row of **stamps**.

Option rows are bordered and carry a kbd key 1 to 9. A selected option inverts to ink. The free-text "other" input is a `plate` field.

**Inline slip.** The same slip appears inside a focus transcript, with an orange 1px border and an orange head. It is the same store item and is resolved in either place.

### Stamps

The slip's action row is a grid of 40px cells separated by 1px rules, in Barlow control caps, each with its kbd key (A / S / D for permissions; 1 to 9 for options, then Enter to submit or X to dismiss). The first, primary stamp (Allow once, Continue, or Submit answer) is **ink-filled with ground text**, and its hover steps to ink-2. Secondary stamps are text on the ground, and their hover steps to `plate-2`. On press, a stamp shifts by (1px, 1px) and scales to 0.985 over 150ms, and an 8px ink corner bracket appears at its top-left as a registration mark.

### The Stamp Moment (signature motion)

Acting on a slip runs this sequence:

1. The head turns ink with ground text.
2. A 1px ink frame registers from a 4px offset to 1px (`register`, 150ms, `ease-out`).
3. A verdict stamp lands in the middle of the slip: a 2px ink border on the ground, the verdict in 800 caps, and "time · by you" in mono. It arrives from a 3px offset at scale 1.04 (`stamp-in`, 150ms).
4. Context, body and actions dim to 32% opacity.
5. After 480ms the slip folds away (`grid-template-rows` 1fr to 0fr, 200ms).
6. The next slip opens, and the band counter ticks down.

Under reduced motion the slip resolves immediately. Each cleared item writes a row to the **Cleared ledger**: time, verdict, session, and what was cleared. When the queue empties, the **All clear** state shows "ALL CLEAR" in display type and a tally (working, idle, cleared, last) under a 1px ink top rule.

### Buttons

- **Shape:** square, with 1px outlines where outlined.
- **Send** (30px, control caps): ink fill with ground text, and ink-2 on hover. It presses like a stamp. While a run is active it becomes **Stop**: transparent, with a 1px ink border.
- **Link button** (label caps, 1px `rule-2` border, 4px 6px padding): inverts to ink on hover. Used for "Open session" and similar actions.
- **Icon button**: a 44px band cell or a 30px pane/column cell, ink-2 or ink-3 by default, with `plate` fill and ink on hover.
- **Ghost text action** (project "+ NEW SESSION"): ink-3 label caps, with `plate` and ink on hover.
- **Inverse hover:** the band's New session control and link buttons hover to a full ink fill with ground text, and their kbd keys reverse with them.

### Segmented Controls and Tabs

- **Segmented:** 24px buttons with 1px `rule-2` borders sharing a common edge (-1px overlap), in label caps with a mono count. The pressed button is ink-filled with ground text.
- **Tabs** (Session / Changes / Files / Terminal): 30px, label caps, ink-3. The selected tab is ink with a 2px ink underline.

### Session Table

Rows are 32px, separated by `rule-soft`. The columns are:

- **State:** lamp and mono state code.
- **Session:** title, an optional agent tag (a mono 11px label in a 1px `rule-2` box), and the open-in-focus mark.
- **Now:** a tool name in label caps followed by its target in mono, or a prose status in sans for done or retry rows.
- **Todos:** a strip of 6×10px cells. Done cells are filled ink-3 and the current cell has an ink border, followed by an n/m count.
- **Context:** a 40×4px meter, ink-3 normally and ink when high, with its percentage.
- **Cost and age**, both in mono.

Row states:

- **Hover:** `plate`.
- **Waiting:** an `accent-veil` wash (`accent-veil-2` on hover), with `accent-text` in the state and now cells.
- **Cursor:** `plate-2` with a `rule-2` inset frame.
- **Fresh:** when a row's now cell changes, its text fades from ink to ink-2 over 700ms.

Fault rows reverse their state code to an ink badge.

### Focus Column

- **Column band:** a 28px orange strip at the very top, shown only while that session waits. It reads "WAITING ON YOU · <kind> … JUMP ↓" and jumps to the inline slip.
- **Head:** the title (19px caps) and mono meta.
- **Transcript:**
  - message labels in label caps;
  - user messages on a `plate` with a 1px rule;
  - assistant prose in sans;
  - reasoning that folds behind a chevron, with a `rule-2` left rule;
  - **tool rows**, each a 1px-ruled box: a 7px mark (ink-3 done, blinking ink running, orange pending permission, hollow ink error), the name in caps, the target in mono, and meta (with add/del counts in diff colours). Each expands to output or a diff.
- **Plates:** todo plates (numbered, with the in-progress box drawn as an inset ink square), subtask plates, and **fault plates** with an ink head bar and a 1px ink border.
- **Jump to latest:** a sticky ink "LATEST" button.
- **Composer:** a `plate` textarea from 62px to 220px tall that steps to `plate-2` on focus (without an outline), a mono model button, a mono key hint, and Send.

### Diffs and Code

Diffs and code use the data face (12px/1.6) on a `plate`, with a 44px right-aligned line-number gutter in ink-3 and a 16px mark column. Add and delete lines are tinted with `add-bg` and `del-bg`, and their marks are coloured. Hunk headers sit on `plate-2` in ink-3. Code viewer syntax highlighting is monochrome: keywords bold, comments ink-3, strings ink-2. The terminal is a `plate` well with a blinking ink block cursor.

### Inputs

All inputs are square with a 1px `rule` border on `plate`. Focus moves the border to `rule-2` or ink-3, and the composer also steps its fill. The palette input is borderless at 48px, with a rule underneath. Placeholders use ink-3.

### Navigation and Overlays

- **Command palette** (⌘K): 660px wide, positioned 11vh from the top over the scrim, with a ground background and a `rule-2` border. It has a 48px input, group heads in column-head caps, and 34px results. The selected result is `plate-2` with a `rule-2` inset frame.
- **Keys sheet** (`?`): 560px wide, in the same frame, with a two-column key/description list under a 1px ink top rule.
- **Popover** (composer suggestions): ground background with a `rule-2` border, and mono entries with sans descriptions.
- **Settings navigation:** 34px items in control caps. The current item is ink-filled.
- **Status line:** 24px, mono 11px in ink-3. Its cells are divided by rules and show link state, session tallies, tokens today, cost today, the synthetic-data marker and the keys hint.

### Lamps and State Codes

Lamps are 9px. Waiting is solid `accent`. Working is solid ink with a ring that pulses outward (1.8s, infinite). Retry is half-filled ink-2, idle is a hollow ink-3 ring, and fault is a hollow ink ring with a 45° slash. Inside orange or stamped heads the lamp takes the head's text colour. The mono state code always sits beside it.

### Keys (kbd)

Mono 10.5px in ink-3, with a 1px `rule-2` border, 2px 4px padding and a minimum width of 16px. On an ink fill, kbd text and border reverse to the ground colour.

### Motion Grammar

- **Hover and selection fills:** immediate, with no transition.
- **Feedback:** `--t-feedback` (150ms) with `--ease-out` (`cubic-bezier(0.16, 1, 0.3, 1)`). Covers stamp and send presses, chevron rotation, and the stamp moment.
- **Structural:** slip fold 200ms, slip arrival 260ms (it slides down 12px and fades in), counter tick 220ms.
- **Ambient (state, not decoration):** the working lamp ring at 1.8s, and the running tool mark, streaming caret and terminal cursor blinking in 1s hard steps.
- **Reduced motion:** all animation and transition durations collapse to near zero, smooth scrolling is turned off, and the stamp sequence resolves immediately.

## Do's and Don'ts

### Do:

- **Do** reserve `accent` for things waiting on the human: the lit counter, the active and inline slip heads, the column band, the waiting lamp, the pending tool mark, waiting-row veils, and the mobile queue badge.
- **Do** put `accent-ink` on every orange fill, and use `accent-text` for orange words on the ground.
- **Do** make primary actions ink-filled with ground text. Selection and the current item are also ink fills.
- **Do** draw focus as a 2px ink outline inset by 2px (`outline-offset: -2px`).
- **Do** set every name and control in uppercase Barlow Condensed (700, or 800 for the wordmark, counter, verdict and display). Set sentences in system sans, and data and code in mono.
- **Do** separate everything with 1px rules (`rule`, `rule-soft` between rows, `rule-2` for control outlines). Head spec lists with a 1px ink rule.
- **Do** pair every state colour with a lamp shape and a mono state code.
- **Do** keep hover fills immediate, and spend motion only on the stamp grammar (150ms feedback, 200ms fold, 260ms arrival).
- **Do** break code only at token boundaries, with a 2ch hanging indent in narrow views.
- **Do** keep the 16px left gutter and 32px rows when adding panes or tables.

### Don't:

- **Don't** use orange for primary buttons, links, selection, focus, progress, errors, brand decoration or input carets.
- **Don't** use green or red outside diff lines and diff counts. Success and failure are told with ink, shape and words.
- **Don't** round corners. The only round form is the 9px status lamp.
- **Don't** add drop shadows, blurs or gradients. Inset 1 to 2px box-shadow lines are rules, and are the only permitted `box-shadow`.
- **Don't** set prose in mono or data in sans.
- **Don't** set Barlow Condensed in lowercase or sentence case, except the `alforria` wordmark.
- **Don't** wrap code mid-identifier, and don't let wide code views wrap. They scroll.
- **Don't** put kickers or eyebrow labels above titles. Pane heads and slip heads are functional labels that carry state, counts or keys.
