# Design: Magic mode — unified natural-language command search

Status: **design, not yet implemented.** Written 2026-07-28 out of a
brainstorm session on making the explorer "feel like AI" without paying
for it. This is a **partial, explicit reversal** of the 2026-07-21
decision recorded in `docs/ui-enhance-roadmap.md` ("'Ask AI' / semantic
search — Phase 3. The search bar gets no AI affordances now.") — see
"Relationship to prior decisions" below for exactly what is and isn't
being reopened.

## Goal & scope

One search bar, no manual mode toggle. Typing a filename behaves exactly
as it does today (frecency + fuzzy, unchanged). Typing a command-shaped
phrase — "delete screenshots older than 30 days", "move PDFs modified
this week to Documents" — gets classified as intent, parsed into a
structured multi-step plan, shown as a **Magic card** for review, and
only executed on explicit confirm.

Two new pieces, both cheap and deterministic:

1. An **intent classifier** (filename-like vs command-like) that decides
   whether to show a Magic card at all.
2. A **plan grammar** that turns a recognized command into a batch of
   file operations against the existing `ops::FileOp`/`Journal`.

Neither is a model in the embeddings/inference sense — see below.

## Non-goals (v1)

- No embeddings, no semantic/content search (OCR, PDF text). Unchanged
  Phase 3 gate — see "Relationship to prior decisions."
- No bundled or OS-provided LLM in v1. The grammar covers a fixed verb
  set; unmatched phrasing shows no Magic card at all rather than a
  best-guess plan. (See "Deferred" below.)
- No auto-execution, ever. Every Magic plan requires explicit confirm —
  same principle as chip removability in `phrases.rs`.
- No new operation types. v1 verbs map 1:1 onto the existing
  `ops::FileOp` variants (`Move`, `Copy`, `Rename`, trash-delete) — no
  new execution engine.

## Architecture

### 1. Intent classification — filename vs command

fastText-style: character n-grams (2-4), feature-hashed into a
fixed-size sparse vector, scored by a linear (logistic regression)
classifier. Trained offline, weights shipped as a small embedded asset
(target: under 200 KB), loaded once at startup — no training or
fine-tuning happens on-device.

Runtime cost is one sparse dot product per keystroke, cheaper than the
existing fuzzy pass. Expected to be within noise, but per the CLAUDE.md
rule on search-as-you-type latency, this gets a `criterion` bench before
merge, not an assumption.

Classification is **confidence-gated UI, not a hard switch**:

- High-confidence filename → today's behavior, byte-for-byte unchanged.
- High/mid-confidence command → today's literal/fuzzy results still
  render if any exist, *plus* a Magic card with the parsed plan.
- Low confidence → no Magic card; falls back entirely to today's path.

Training data: real filenames (sampled) as the negative class; command
phrasings synthesized from the v1 grammar's own templates as the
positive class — cheap to generate in volume, no manual labeling corpus
needed for v1.

### 2. Slot extraction — command → structured plan

Extends `search_filter`'s value grammar and `phrases.rs`'s expansion
approach rather than inventing a parallel path: the same `Filter`
grammar (`kind:`, `ext:`, `size:`, `modified:`, comparators and ranges)
resolves *which files* a command targets.

New top layer on top of that: verb recognition (`delete`/`remove`/
`trash`, `move … to …`, `copy … to …`, `rename … to …`) maps matched
targets to one `ops::FileOp` per file, batched into a `Vec<FileOp>`.
Pure, rule-based, unit-testable exactly like `phrases.rs` — no model
here either.

Ambiguous input (verb recognized, destination missing — "move my
screenshots") does not guess. It suppresses the Magic card rather than
emitting a partial or best-guess plan.

### 3. Plan preview & execution — plugs into the existing undo Journal

The Magic card renders the resolved plan as a checklist (source →
destination per op, or a file list for delete), individually
deselectable. Confirm calls `ops::apply_with_progress` per selected op
on the background executor — the same path drag-and-drop and paste
already use, not a new one.

The batch is recorded as one entry in `ops::Journal`, so a bad Magic
plan is undoable with the same Ctrl+Z as any other action. This is the
real safety backstop for classifier or grammar mistakes — the preview
step is the first line of defense, undo is the second.

### 4. Deferred: local LLM fallback, and the bar it must clear

**Not in v1.** If the rule-based grammar's miss rate on real queries
turns out too high, the fallback is a tiny local model — OS-provided
where available (Apple's on-device Foundation Models framework on
macOS 15.1+, Phi Silica on Windows Copilot+ PCs), falling back to a
bundled quantized model (via `candle`, to avoid a per-platform C++
toolchain dependency) only on Linux/older OS where nothing is
OS-provided. Invoked only on explicit submit for phrases the grammar
failed to parse — never on the keystroke path — with grammar/schema-
constrained decoding so its output always lands in the same `FileOp`
plan structure above, never freeform text.

**Gate before building any of this:** ship v1's rule-based grammar,
measure its real miss rate against actual Magic-mode queries in use.
Only pursue the model fallback if misses are frequent enough to matter.
Same measure-before-build discipline as the embeddings gate at the end
of `docs/design-search-ranking.md`.

## Relationship to prior decisions

- **Supersedes:** the `docs/ui-enhance-roadmap.md` "Explicitly out of
  scope" line from 2026-07-21 — the search bar now gets the classifier
  + Magic card described here.
- **Unchanged:** the Phase 3 gate on embeddings, semantic/content
  search, and local ML inference on the always-on keystroke path
  (`docs/roadmap.md` Phase 3; the deferred-embeddings bar at the end of
  `docs/design-search-ranking.md`). The classifier and grammar here are
  the same cost class as the already-shipped frecency/fuzzy/phrases
  work — cheap, deterministic, no model on the hot path — not a step
  toward embeddings. The one piece that *would* cross into "local ML
  inference" (§4, the LLM fallback) is explicitly deferred behind its
  own gate above, not approved by this doc.
- **Builds directly on:** `src/phrases.rs` (NL → filter expansion),
  `src/search_filter.rs` (`Filter` / value grammar), `src/ops.rs`
  (`FileOp` / `Journal`).

## Open questions / before implementation

- Exact v1 verb list and phrasing coverage — needs a short spec pass,
  not just move/copy/rename/delete assumed above.
- Where the classifier's training corpus lives and how it's
  regenerated (dev-time tool vs. checked-in weights only).
- Magic card visual design — separate UI pass, doesn't block the
  classifier/grammar work.
- Bench target for classifier latency before merge, per the CLAUDE.md
  rule that anything plausibly touching search-as-you-type latency must
  be measured and called out, not silently merged.

## Suggested phasing

1. N-gram intent classifier — isolated module, unit + bench tested, not
   yet wired to the UI.
2. Verb/plan grammar extending `phrases.rs` — pure logic, unit tested.
3. Magic suggestion card UI, wiring classifier → grammar → card.
4. Confirm → execute wiring into `ops::apply_with_progress` +
   `Journal`.
5. *(Separate gate, not this doc's scope)* LLM fallback per §4.
