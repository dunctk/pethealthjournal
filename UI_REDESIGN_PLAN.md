# Console UI Redesign Plan

Status: **proposed, not started.** Written 2026-07-25 from analysis of the current
console. No code has been changed.

Note on provenance: this file was drafted on a Syncthing mirror that has no `.git`
(Syncthing excludes it). Pick it up on the machine that holds the real repository,
and branch before starting.

---

## 1. Why

The console shell is `15rem | 1fr | 21rem` (`static/app.css:66`). The right column
is labelled **"HELP FOR THIS HISTORY"** (`templates/console.html:66`) but actually
contains eight unrelated things: the knowledge card, the vet-share form, the weight
tracker, the medication dose log, the prescription plan, the adherence capture, the
blood-test uploader, and the multi-marker trend chart. That is roughly lines 65–172
of `console.html` — four complete data-entry systems inside a fixed 21rem rail.

Four structural failures follow.

**1. Width is allocated inversely to content need.** The 21rem rail holds the widest
content in the app: an 11-field prescription form rendered two-up
(`console.html:127`), a `repeat(auto-fit, minmax(19rem, 1fr))` chart grid
(`app.css:206`), and a table with `min-width: 34rem` (`app.css:218`). The chart
already needed an `is-expanded` fixed-position escape hatch (`app.css:223`) to be
legible at all — the layout admitting it is wrong. Meanwhile the timeline, which is
mostly short text rows, gets 33rem+.

**2. Frequency is inverted.** The daily action (write "Milo vomited") is one input.
The twice-a-year action (enter a prescription) is a permanently expanded 11-field
form that every session scrolls past.

**3. "One timeline per pet" is not true.** The landing page promises it
(`templates/home.html:53`), but `create_weight` (`db.rs:879`),
`create_medication_administration` (`db.rs:1204`), and `create_lab_report`
(`db.rs:952`) each write only to their own table and never touch `health_events`.
`list_events` (`db.rs:1491`) returns observations and symptoms only. The console
therefore renders five parallel logs, each with its own date ordering — the exact
fragmentation the product pitch says it fixes.

**4. Standing state and history look identical.** Prescriptions (what should happen)
and administrations (what did happen) are sibling `.tracker-box` elements with equal
visual weight. That distinction is the whole point of the medication model, and the
UI erases it.

Supporting evidence: at ≤1120px the rail simply becomes a six-form block below the
timeline (`app.css:145`). There is no responsive strategy because there is no
hierarchy to preserve. And `index` (`web.rs:266`) issues nine queries to populate a
15-field `ConsoleTemplate` (`web.rs:1099`) on every load, whether or not the user
looks at any of it.

---

## 2. Principles

1. **Design at 390px first.** There is no rail at 390px, so the rail's contents need
   a real home regardless. Whatever structure survives that width *is* the
   structure; desktop gets more room, not more panels.
2. **Permanent space in order of session frequency:** capture → look back → import a
   lab → edit the plan → share with a vet.
3. **One pet, one spine.** Every dated fact — observation, dose, weight, lab result
   — is an entry on one chronological record, rendered differently by type.
4. **Forms are a mode, not furniture.** Only the fast capture path stays always-on.
5. **The right column returns only as a transient drawer**, holding exactly one
   thing: detail about the entry you selected.

---

## 3. Target structure

```
┌ topbar ────────────────────────────────────────────┐
├ pets ┬ Milo · cat · 4.8 kg   [Share] [+ Record]     │
│ Milo │ Timeline · Plan · Labs · Sharing             │
│ Nala ├─────────────────────────────────────────────┤
│      │ [ write what happened ................. ↑ ] │
│  +   │ All · Symptoms · Meds · Weight · Labs       │
│      │ ── 25 Jul ──────────────────────────────    │
│      │ 08:42 observation  Vomited once…      → ┃   │
│      │ 07:10 dose         Benazepril 1 mg      ┃   │
│      │ ── 24 Jul ──────────────────────────    ┃   │
└──────┴──────────────────────────────────────────┘  │
                        drawer: knowledge + related ─┘
```

- **Pet rail** — unchanged, it works.
- **Timeline tab** — full width, unified across the four tables, filterable by type.
- **Plan / Labs / Sharing tabs** — each gets full width and costs zero scroll when
  unused. Tabs work identically at 375px; a 21rem rail cannot.
- **Drawer** — transient, opened from a timeline entry.
- **`+ Record`** — opens weight / dose / lab / structured-symptom forms in a
  `<dialog>` via `hx-get`. The plain capture input stays zero-click; do **not** put
  the fast path behind a sheet.

### Migration table

| Current location (right rail) | Destination |
| --- | --- |
| Knowledge card (`console.html:69`) | Drawer, opened from an event |
| `related_count` (`console.html:76`) | Drawer header |
| Share form + `_shares.html` (`:81`) | Sharing tab (or sheet from header) |
| Weight form (`:95`) | `+ Record → Weight` dialog |
| Weight list (`:103`) | Timeline entries + sparkline in Plan |
| Medication dose form (`:110`) | `+ Record → Dose` dialog |
| Medication list (`:121`) | Timeline entries |
| Prescription form + list (`:127`) | Plan tab |
| Adherence capture (`:145`) | Plan tab, inline per prescription row |
| Blood-test upload/import (`:153`) | Labs tab |
| Lab trends chart (`:159`) | Labs tab, full width |
| Lab report list (`:169`) | Labs tab; also timeline entries |
| Metrics strip (`:54`) | Stays, in pet header |
| Structured symptom form (`_agent_timeline.html:12`) | `+ Record → Symptom` dialog |

---

## 4. Phases

Land these as separate commits. Phase 2 is the only one that touches the schema
question, and Phase 3 is independently valuable — if the project stalls, stopping
after Phase 3 still leaves the app better than it is now.

### Phase 1 — Tab shell

- Add `view` to `IndexQuery` (`web.rs`), defaulting to `timeline`.
- Split `ConsoleTemplate` (`web.rs:1099`) into a shell template plus one template
  per tab: `_tab_timeline.html`, `_tab_plan.html`, `_tab_labs.html`,
  `_tab_sharing.html`.
- Move the tab-specific queries out of `index` (`web.rs:266`) so each request runs
  only the queries its tab needs. This removes the nine-query-always problem.
- Tab links use `hx-get` with `hx-push-url` so deep links and back/forward work;
  full page loads must still render the right tab server-side.
- Delete `.context-panel` (`app.css:127`) and its `@media` overrides.

### Phase 2 — Unified timeline

- Add `db::list_timeline(household_id, pet_id, before: Option<DateTime>, limit)`
  returning a `TimelineEntry` enum over `health_events`, `weight_entries`,
  `medication_administrations`, and `lab_reports`, ordered by time descending.
- **Decision required:** read-time `UNION ALL` vs. writing real `health_events` rows
  from the four `create_*` functions. Recommendation: **read-time union first.** It
  is reversible, needs no migration, and does not risk double-counting in
  `count_related` (`db.rs:1530`) or the MCP tools. Revisit only if the union query
  becomes a performance problem.
- Keyset pagination on `(occurred_at, id)`. The current `limit: 50`
  (`web.rs:281`) becomes a real page boundary once four sources are merged.
- **Undo is the sharp edge.** `db::undo_event` (`db.rs:1461`) only knows
  `health_events`. Either make undo type-aware, or — simpler and safer — render the
  Undo button *only* on entries backed by `health_events` and give the other types
  their own delete affordances later. Do not silently show a button that no-ops.
- Group entries under date headers; render each type with its own icon and summary
  line.

### Phase 3 — Charts (see §5)

### Phase 4 — Dialogs and drawer

- `+ Record` menu → `hx-get` fragment routes returning form partials into a
  `<dialog>`.
- Existing POST handlers (`/weights`, `/medications`, `/symptoms`, etc.) keep their
  paths; only the response target changes.
- Drawer replaces the knowledge card's permanent slot.

---

## 5. Charts: near-full-screen

The SVG is already resolution-independent — `viewBox="0 0 720 250"` with
`width:100%; height:auto` (`app.css:213`). Making it bigger is free. **The trap is
that scaling is not enlarging:** at 2× container width the 9px axis text becomes
18px, `stroke-width: 2.5` becomes 5, and `r="5"` dots become 10. That is a zoomed
screenshot, not a bigger chart.

### 5a. Space

Once the chart lives in the Labs tab it defaults to roughly `calc(100vw - 15rem)`
— about 85% of screen, with no overlay needed for the normal case.

Then **replace the hand-rolled overlay with `<dialog>` + `showModal()`**.
`.lab-trends.is-expanded` is `position: fixed; inset: 4.85rem 1.25rem 1.25rem
16.25rem` (`app.css:223`), which hard-codes the topbar height and rail width and is
already duplicated across two media queries (`app.css:227`, `:232`) to patch the
rail changing size. A `<dialog>` needs no knowledge of the shell and brings Escape
handling and focus trapping for free — delete the manual keydown listener at
`app.js:144` and the `.trend-focus-open` body class (`app.css:222`).

### 5b. Overview + detail

Currently it is small multiples: one card per marker,
`repeat(auto-fit, minmax(19rem, 1fr))`. Going fullscreen with small multiples just
yields *more cards per row* — five markers at 32rem across a 27" display. That is a
bigger grid, not a bigger chart.

Instead:

- **One selected marker**, large: full width, ~55–60vh.
- **All other markers** demoted to a sparkline strip (side rail inside the tab, or a
  bottom filmstrip). Click to promote.

This is what makes the pixels worth spending — more room buys time resolution and
annotation for the marker you care about.

### 5c. What to spend the pixels on

In priority order:

1. **A real date axis.** `x` is index-based (`app.js:118`) — points are evenly
   spaced regardless of actual dates, so three tests in one week and one a year
   later plot identically. At full size that is actively misleading.
2. **Reference range as a shaded band.** `reference_range` is already parsed and
   stored (`domain.rs:191`) and shown in the table, but never drawn. This turns
   "a number went up" into "a number left normal" — the highest-value addition
   here, and it needs vertical room to read. Note the ranges are free text from OCR
   (e.g. `"3.5 - 5.5"`); parse defensively and skip the band when parsing fails.
3. **Gridlines that scale with height.** Currently fixed at three
   (`[0, .5, 1]`, `app.js:122`).
4. **Every point dated.** Only first and last are labelled (`app.js:127`).
5. **Flagged points annotated inline**, not just recoloured.

### 5d. Mechanism

Render in pixel space instead of `viewBox` space. `renderSeries` (`app.js:103`) is
nearly there already — `width`, `height`, and `pad` are locals at `app.js:110-112`,
so hoisting them to parameters is most of the work:

```js
const draw = () => {
  const box = chart.getBoundingClientRect();
  const w = Math.round(box.width);
  const h = Math.round(Math.min(box.height, window.innerHeight * 0.6));
  chart.innerHTML = renderSeries(selected, 0, w, h);   // no viewBox
};
new ResizeObserver(draw).observe(chart);
```

Drop the `viewBox` and set `width`/`height` attributes directly. Then
`font: 9px var(--mono)` and `stroke-width: 2.5` stay literally 9px and 2.5px at any
size, and `pad.left` can grow only as much as the axis labels actually need. One
renderer serves both the inline size and the fullscreen dialog.

Debounce the observer with `requestAnimationFrame` — `innerHTML` on resize will
thrash otherwise.

Aspect ratio: do not carry `2.15 / 1` (`app.css:213`) into the large chart. For
reading slopes, aim for a height where the median segment sits near 45°; for sparse
lab series that lands nearer 2.5–3:1, but let it be driven by the data rather than
fixed.

Re-init already happens on `htmx:afterSwap` (`app.js:154`) — keep that working when
the Labs tab is swapped in, and disconnect stale `ResizeObserver`s on re-init to
avoid leaking one per swap.

---

## 6. Risks

- **Undo silently breaking** on non-`health_events` timeline entries. See Phase 2.
- **Tabs hide things.** Discovery of prescriptions drops. Mitigate by surfacing
  "2 active prescriptions" as a pet-header metric that links to Plan.
- **MCP surface must not regress.** `clinical_timeline` (`db.rs:1429`) and
  `medication_plan` (`db.rs:1417`) feed the MCP tools; the UI refactor must leave
  their behaviour unchanged.
- **Household scoping.** Every new query and fragment route must stay
  household-scoped per `AGENTS.md`. The union query is the easiest place to leak
  another household's rows — scope every arm of the `UNION`, not just the outer
  select.
- **Reference-range parsing** on OCR text will fail on some rows; never let that
  break the chart.

---

## 7. Verification

Per `AGENTS.md`:

```bash
cargo fmt --check
cargo check --all-targets
cargo test
```

Plus, because this is a UI change: exercise the HTMX capture and undo flow at
desktop and mobile widths. Specifically check the 390px layout first (it is the
design driver), then 760px, 1120px, and wide desktop — the three existing
breakpoints all carry rail-specific rules that this refactor should be deleting
rather than patching.
