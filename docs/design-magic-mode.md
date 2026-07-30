# Design: Magic mode — unified natural-language command search

Status: **v1 shipped 2026-07-29 — grammar (`src/magic.rs`), card
(`src/ui/magic_card.rs`) and confirm→execute all in. Phase 1 (the
classifier) was cut on measurement rather than built; see §1. Phase 5
(LLM fallback) remains behind its own gate.** Written 2026-07-28 out of a
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

One new piece, cheap and deterministic: a **plan grammar** that turns a
recognized command into a batch of file operations against the existing
`ops::FileOp`/`Journal`. Deciding whether to show a Magic card at all
was originally a second piece — an intent classifier — which measurement
retired before it was built (§1). The grammar does both jobs: a query
that isn't an unambiguous complete command simply doesn't parse.

No model in the embeddings/inference sense anywhere — see below.

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

### 1. Intent classification — cut, and why (measured 2026-07-29)

**Not built. The grammar is the classifier.** This section originally
specified a fastText-style model: character n-grams (2-4), feature-
hashed, scored by a linear classifier, trained offline and shipped as an
embedded asset. Before building it, the premise got measured, and it did
not survive.

The measurement. With a classifier the card gate is `parse_succeeded AND
classifier_agrees`; without one it is just `parse_succeeded`. Those
differ *only* where the grammar produced a plan and the classifier would
veto it — so the classifier's entire possible contribution is suppressing
cards the grammar already built, and the question is how big that set is.
Harness: `tests/magic_false_positives.rs`, run over 123,559 real
filenames → 329,767 queries, taking every **word prefix** of each name,
because search is incremental and the gate fires on every keystroke.

| verb | false positives (of 329,767) |
|---|---|
| Move / Copy / Rename | **0** |
| Delete / Remove / Trash | **58** |

Move, copy and rename need no classifier at all: requiring a `to`/`into`
separator *plus* a resolvable destination is already a near-perfect
structural gate, because filenames do not have that shape. The veto set
is empty, so a classifier could only subtract.

Delete has no separator — `delete <anything>` parses — and produced 58,
every one on the most destructive verb (`delete reminder` ←
`delete-reminder.js`, `remove prefix` ← `remove-prefix.d.ts`). So delete
needed *something*. But a text classifier is the wrong something: those
false positives are command-shaped by any linguistic measure — verb plus
object, exactly like `delete old logs` — so the proposed training setup
(real filenames as negatives, grammar templates as positives) would be
fitting two classes that genuinely overlap on precisely this set.

What shipped instead is the **structured-evidence gate**
(`magic::clears_delete_gate`): a delete command's selection must carry at
least one `Filter` (kind, date, size, extension), not just free text.
Filter vocabulary is how a person describes a **set**; bare text is how
they name a **thing**. That cuts 58 → 2 end-to-end, costs one sampled
phrasing (`delete old logs`, which now needs `delete logs older than 30
days`), needs no asset, no training corpus, and no per-keystroke dot
product. The gate lives inside `parse`, so a caller cannot forget it.

Everything else in this section still holds: today's literal/fuzzy
results render unchanged alongside any card, and no card at all is
always the fallback.

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
real safety backstop for grammar mistakes — the preview step is the
first line of defense, undo is the second.

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
  scope" line from 2026-07-21 — the search bar now gets the Magic card
  described here.
- **Unchanged:** the Phase 3 gate on embeddings, semantic/content
  search, and local ML inference on the always-on keystroke path
  (`docs/roadmap.md` Phase 3; the deferred-embeddings bar at the end of
  `docs/design-search-ranking.md`). The grammar here is the same cost
  class as the already-shipped frecency/fuzzy/phrases work — cheap,
  deterministic, no model on the hot path — not a step toward
  embeddings. This gate got *stronger*, not weaker: the one piece of
  this design that was even model-adjacent (§1's n-gram classifier) has
  now been cut on measurement. The one piece that *would* cross into
  "local ML inference" (§4, the LLM fallback) remains deferred behind
  its own gate, not approved by this doc.
- **Builds directly on:** `src/phrases.rs` (NL → filter expansion),
  `src/search_filter.rs` (`Filter` / value grammar), `src/ops.rs`
  (`FileOp` / `Journal`).

## Open questions / before implementation

- ~~Exact v1 verb list and phrasing coverage~~ — **settled, see "v1
  grammar as built" below.**
- ~~Where the classifier's training corpus lives and how it's
  regenerated~~ — **moot, no classifier (§1).**
- Magic card visual design — separate UI pass, doesn't block the
  grammar work. Now the only thing between here and a usable feature.
- ~~Bench target for classifier latency~~ — moot. The CLAUDE.md
  search-as-you-type rule still applies to the grammar itself, which is
  what `benches/magic_bench.rs` covers.
- **The delete gate's recall cost is measured against 13 hand-written
  commands** — thin. The 329,767-query false-positive number is solid;
  "doesn't suppress real commands" is not, until there are real Magic
  queries to measure against. Revisit once the card ships and the
  phrasings people actually type can be sampled.

## v1 as built

### The card (2026-07-29)

- **Inline, never modal.** A dialog would demand an answer to a question
  the user did not ask — they typed into a search box. The card renders
  above the results and is dismissed by typing another character.
- **The search searches the command's *target*, not its words.** A
  command query runs `command.selection` through the ordinary search
  path, so the result list under the card is exactly the set the plan
  acts on. `delete screenshots older than 30 days` searched literally
  would match nothing at all.
- **Command queries raise the search limit to `MAX_PLAN_OPS + 1`**,
  rather than the usual 500. This is a correctness requirement, not a
  tuning knob: plans are built from the returned rows, so a truncated
  search would silently become a *partial* delete, and `build`'s
  too-many-to-review guard would never fire because it would only ever
  see the truncated count.
- **Rows are individually uncheckable**, all checked by default — review
  is about removing what you didn't mean, not opting in file by file.
- **A failed plan still shows a card**, carrying its `PlanError`. After
  someone types a real command, "no folder called Archive" is an answer;
  silence is not. "Still resolving" is a distinct state from "nothing
  matched", so an indexing root never claims an empty result.
- **Confirm reuses the paste path** — one `Job` with progress, sequential
  `apply_with_progress` off-thread, one `Journal::record`, so Ctrl+Z
  reverses a whole Magic batch. Occupied destinations retarget to the
  next free "name 2" variant rather than prompting per file: a plan is
  reviewed as a whole, and stopping to ask about file 40 of 200 would be
  worse than being uniformly predictable.
- **Enter does not confirm.** It still opens the selected result. A
  destructive batch should cost a deliberate click, not the key someone
  is already leaning on.

### Grammar (2026-07-28)

- **Verbs:** `delete`/`remove`/`trash`, `move … to|into …`, `copy …
  to|into …`, `rename … to <pattern>`. All four map 1:1 onto existing
  `ops::FileOp` variants, per the non-goal above.
- **Delete additionally requires structured evidence** — at least one
  kind/date/size/extension filter in its selection, not just free text.
  This is what replaced the §1 classifier; the measurement and the
  recall cost are there.
- **A command's target is read as a description, not a filename.**
  `phrases::expand_as_description` exists for exactly this: the search
  bar's rule 2 ("a lone word is a filename") is right for search and
  wrong after a verb, where it made `delete screenshots` mean "delete
  files *named* screenshots" — a silently much narrower plan than the
  one asked for.
- **Batch rename** takes a pattern with `{n}` (position), `{name}`
  (original stem) and `{ext}`. `{n}` auto-pads to the width of the batch
  so results sort in numbered order without extra syntax. An
  unrecognized placeholder is rejected rather than treated as literal —
  writing `shot-{nmae}.png` onto a hundred real files is not a thing to
  do on a guess. A pattern with no `{n}`/`{name}` is refused for a
  multi-file batch, since it collapses every match onto one name.
- **Destinations** resolve against the folder on screen first (by
  directory scan, exact case preferred), then the user's known folders
  via `dirs`. The scan rather than a `cwd.join(name)` probe is
  deliberate: a join succeeds on case-insensitive macOS/Windows volumes
  and hands back the *typed* casing while resolving nothing on Linux —
  the same command producing a different destination per OS.
- **Plan size is capped** at `magic::MAX_PLAN_OPS` (1000). The bound is
  about reviewability, not executor capacity: the preview is the first
  line of defense against a mis-parse, and nobody inspects ten thousand
  rows before confirming.
- **Conflict handling is not reimplemented here.** A plan may name an
  occupied destination; `ops` already answers that (refuse, or retarget
  via `next_free_name`) and paste/drag already drive it.

Two supporting changes landed in `phrases.rs`: comparative phrases
(`older than 30 days`, `bigger than 10mb`) — which the doc's own
headline example needs — and `modified`/`created`/`changed` as date
connectives. The phrase window widened from 3 words to 4 to fit the
comparatives; measured at +14 ns per query against a ~30 ms keystroke
budget, i.e. noise, but measured rather than assumed per CLAUDE.md.

## Suggested phasing

Phase 2 was built first, and phase 1 then measured away entirely, so the
original 1→5 order no longer describes the work.

1. ~~N-gram intent classifier~~ — **cut on measurement, see §1.** Its
   job is done by `magic::parse` plus the delete gate.
2. ~~Verb/plan grammar extending `phrases.rs` — pure logic, unit
   tested.~~ **Done — `src/magic.rs`, 37 unit tests.**
3. ~~Magic suggestion card UI, wiring grammar → card.~~ **Done —
   `src/ui/magic_card.rs` + `Workspace::render_magic_card`.**
4. ~~Confirm → execute wiring into `ops::apply_with_progress` +
   `Journal`.~~ **Done — `Workspace::confirm_magic`, one job, one undo
   batch, same path as paste.**
5. *(Separate gate, not this doc's scope)* LLM fallback per §4.
