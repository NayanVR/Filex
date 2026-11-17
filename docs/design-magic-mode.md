# Design: Magic mode — unified natural-language command search

Status: **v3 (2026-11-14): the bar also navigates, runs app commands
and answers questions, with new plan verbs and a grouped review screen —
see "v3" below.** Before that: **v1 shipped 2026-07-29 — grammar (`src/magic.rs`), card
(`src/ui/magic_card.rs`) and confirm→execute all in. Phase 1 (the
classifier) was cut on measurement rather than built; see §1. Phase 5
(LLM fallback) remains behind its own gate.** Written 2026-07-28 out of a
brainstorm session on making the explorer "feel like AI" without paying
for it. This is a **partial, explicit reversal** of the 2026-07-21
decision recorded in `docs/ui-enhance-roadmap.md` ("'Ask AI' / semantic
search — Phase 3. The search bar gets no AI affordances now.") — see
"Relationship to prior decisions" below for exactly what is and isn't
being reopened.

## v2 pivot: an explicit mode (decided 2026-07-31; foundation built same day)

**This supersedes "no manual mode toggle" below.** v1's premise was that
the app should infer intent from the query. Dogfooding said otherwise: a
command query and a filename query rendered as the same list of rows, so
it was never clear which one was happening — a `move` plan sat above 697
search results that were themselves a different question, and stale rows
from an earlier keystroke were indistinguishable from real matches.

What changes:

1. **A toggle in the search bar**, showing which mode is active. Magic
   mode **replaces** the results list with a plan view rather than
   rendering a card above unrelated search rows.
2. **Auto-switch stays, but only on strong evidence** (below).
3. **The plan view** shows source → destination per row, resolves
   collisions inline instead of voiding the batch, makes the parsed
   selection and destination editable, and groups by source folder with
   drill-down so 697 rows are reviewable.

### Build status (2026-07-31)

**Shipped:** items 1 and 2, and the plan view as a full pane replacing the
results list (items 3's first clause). The plan list is **virtualized**
(`uniform_list` over `plan.ops`) — a 342-op plan renders only the visible
rows, where building every row as a child had made scrolling sluggish.
Each row shows the source **name + its folder** and the destination, with
a full `source → destination` tooltip, so identical names in different
folders are distinguishable. `magic_mode: bool` on the
workspace is the explicit toggle (sparkles button in the search box);
`magic::parse_with_gate(query, now, !magic_mode)` selects the gate;
`in_magic_view()` (`magic_mode || magic.is_some()`) is the single
predicate the render and search paths share; `render_magic_pane` draws
the plan, a refusal, or a "type a command" hint, and suppresses the
details panel. The floating `render_magic_card` and its card-only atoms
were deleted.

**Move/copy collisions now resolve instead of refusing** (2026-07-31).
`transfer_ops` reads the destination folder once and de-dupes each file's
name in memory — against what is on disk *and* what earlier ops in the
plan claimed — retargeting to the first free `name 2` variant, the same
convention execute-time/paste/drag use. `move roadmap to Downloads` with
two `ROADMAP.md`s no longer dead-ends on "two files would both become …";
it plans `ROADMAP.md` and `ROADMAP 2.md`, and `describe_op` shows the
retargeted name so the rename is visible rather than silent.
`check_collisions` remains for rename, where a colliding pattern is a
signal the pattern is wrong, not something to silently number around.

**Search scope dropdown** (2026-07-31). A control at the search box's left
edge chooses **Anywhere** (default, every indexed root) or **Current Dir**
(only `cwd`'s subtree), applied to normal *and* magic searches alike.
Scoping is a real subtree scan, not a post-filter: `SearchScope::CurrentDir`
hands the directory down to `manager::search_all_scoped`, which resolves it
to a `ScanCtl::scope` entry *inside* each index's read lock and skips
entries outside it during the parallel scan — so there is no truncation
bug (the top-K is drawn from in-scope entries only) and no UI-thread lock
work. A live scoped query re-runs when you navigate (`load_dir`). This is
also the pragmatic answer to the "from downloads" location problem below:
scope to the folder instead of naming it in the sentence.

**Item 3's remaining clauses landed in v3** (below): a plan-wide conflict
choice, a one-click "create this destination", and grouped source-folder
drill-down.

### Where the delete gate goes

`clears_delete_gate` (§1) was never really about delete — it was about
*auto-detection*. It exists because `delete reminder` is indistinguishable
from the filename `delete-reminder.js`, and the grammar had to decide
alone. An explicit toggle removes that burden, so the gate moves rather
than disappearing:

- **Auto-switching into magic mode requires structured evidence** — at
  least one kind/date/size/ext filter, exactly today's rule. This is what
  keeps `delete reminder` from silently arming a delete command.
- **Once the user has toggled in explicitly, the gate is off.**
  `delete old logs` works, recovering the one recall regret v1 recorded.

This keeps `tests/magic_false_positives.rs` load-bearing: its 329,767
queries now measure "may this **auto-switch the mode**", which is the
question that still has a wrong answer. Dropping the gate outright while
keeping auto-switch would have reinstated all 58 false positives on the
most destructive verb with nothing left to catch them.

### Consequences to carry through

- The fuzzy-hit exclusion (`usable_in_plan`) becomes structural — magic
  mode runs its own query rather than filtering a shared result set.
- Magic mode need not run the ordinary 500-row search at all, which is
  where its latency came from (see the measurements in
  `docs/design-search-ranking.md`).
- `check_collisions` currently refuses the whole plan on one duplicate
  basename — near-certain at 697 files. Inline resolution replaces it;
  the plan-time refusal also contradicted the execute-time rule below
  ("occupied destinations retarget to the next free *name 2* variant").

---

## v3: the bar does everything (2026-11-14)

The premise changed from "a batch-file tool behind a toggle" to "the one
place you type what you want". Four kinds of thing are now understood
(`magic::Intent`), of which the file plan is one:

| Intent | Example | Runs |
|---|---|---|
| `Plan` | `move pdfs from Downloads to new folder Archive` | after review (click, or the *Apply Magic plan* shortcut) |
| `Go` | `go to Documents`, `open ~/code in new tab` | Enter |
| `App` | `dark mode`, `sort by size`, `new folder Tax`, `eject Backup` | Enter |
| `Ask` | `how big is Downloads`, `how many pdfs`, `largest videos` | answers itself |

### What may switch the mode by itself — nothing new

The 329,767-query measurement (§1) covers move/copy/rename and gated
delete, and those remain the **only** things `parse_with_gate(.., auto =
true)` accepts. Everything added here is explicit-only: `settings`,
`new tab`, `go to x`, `zip-files.sh` and `tag-release.yml` are ordinary
things to search for. So the false-positive numbers still describe what
they measured, and nothing needed re-measuring.

Explicit entry is the sparkles toggle, the *Magic mode* shortcut
(Cmd/Ctrl+J), or typing `>` first — no filename starts with it.

### New plan verbs

- **`zip` / `compress` / `archive`** — no per-file `FileOp`; the plan
  lists `targets` and names the archive (`Plan::archive`). Runs through
  the same job as the context menu's Compress.
- **`tag … as <name|colour|colour name>`**, **`untag …`**, **`remove tag
  X from …`** — also `targets`. Not journaled (the journal records file
  operations); the opposite command is the undo.
- **`organize <folder|selected|here> by type|extension|month|year`** —
  ordinary `Move` ops into buckets beside each file. Reads one folder's
  **direct children** off the disk rather than searching: the index
  scope is a subtree, and organizing a subtree would flatten it. Folders
  and dotfiles stay put.
- **`to new folder X`** on move/copy. `Plan::creates` rather than
  `CreateFolder` rows: a move can't be kept while the folder it lands in
  is unchecked. Execution makes only the folders a checked op lands in,
  first in the batch, so undo empties them before removing them.

`plans_only_ever_use_existing_file_op_variants` still holds: no new
`FileOp`, no new execution engine.

### Selection grammar

- **`from <folder>` / `in <folder>`** scopes the search
  (`Selection::within`) — resolved like a destination, then passed as the
  query scope. Fixes the documented example, which used to read
  "downloads" as filename text and match nothing. A tail that reads as a
  date stays a date (`photos from last week`). Cost, accepted: a
  filename containing " in " after a verb (`move notes in progress to
  Archive`) now reports an unknown folder rather than searching the
  words.
- **`selected` / `these`** (`Selection::picked`) is the browse
  selection, which typing a query never disturbed. Explicit-only: a file
  can be called `selected`.
- Plans built from the selection or a folder listing are `local`: there
  are no index hits, so the identity check that guards search-built
  plans is skipped and a vanished file simply fails its op.

### The review screen

- Rows are **grouped under their source folder**, with a header that
  folds the group and a tri-state checkbox for all of it
  (`PlanLayout`). The "In folder" column is gone — it repeated the
  header. A single-folder plan has no headers; the folder is in the
  subtitle. Still one `uniform_list`, over the flattened lines.
- **Conflicts**: when names are already taken in the destination, one
  plan-wide choice — Keep both (default) / Skip / Replace
  (`ConflictPolicy`). Replace trashes the existing file inside the same
  undo batch. Per-row "skip" is the row's checkbox, so no per-row
  control was added. Two matches sharing a name always keep both.
  The choice resets with each new command.
- **Unknown destination** offers *Create “X” here*, which rewrites the
  command to `… to new folder X`.
- Notes under the heading say what the rows can't: folders created,
  where the archive goes, the tag, what was left out.

### The idle pane

Magic on with no complete command shows `magic::suggest`: worked
templates plus every app command, narrowed by word-prefix as you type,
up/down + Enter. A test parses every row, so the list can't teach syntax
that doesn't work. Enter runs a command, and takes a template only while
it *completes* what is typed — it never replaces half a sentence.

### Answers

The index hit carries no size, so sizes are a stat per file on the
background executor, cancelled with the query. `largest …` asks the
index for files over 1 GB first and widens (100 MB, 10 MB, 1 MB, all)
only until it has 50, so a whole-disk question stats dozens of files,
not millions. `how many` stats only the rows it shows. `how big is X`
means the folder when one resolves; `how big are X` always describes
files (`videos` is not the Videos folder).

### Cost (measured, `benches/magic_bench.rs`, M-series)

| Path | Time |
|---|---|
| ordinary keystroke, Magic off (`parse_rejects_filename`) | 82 ns — unchanged path |
| `interpret`, nothing matches | 0.46 µs |
| `interpret`, app command | 0.27 µs |
| `interpret`, plan with scope + new folder | 2.1 µs |
| `suggest` | 19 µs |

All per debounced keystroke (50 ms), and the last four only while Magic
is explicitly on. No new dependency; no asset.

### A local model — looked at again, still not built

§4's gate is unchanged and unmet: nobody has measured the grammar's miss
rate on real queries. On top of that the constraint for this pass was
that size and speed are critical, and a model loses on both: the
smallest instruction-following models are hundreds of megabytes
quantized — many times the whole app — and answer in hundreds of
milliseconds against the microseconds above. The grammar's failure mode
is also the better one for a tool that deletes files: it says "not
understood" and shows what is, where a model produces a confident plan.
The suggestion list is now the place a miss is visible, which is where a
miss rate could be sampled if this is ever reopened.

### Review screen, second pass (2026-11-14)

Rated by a design-review subagent from screenshots of seven states, and
iterated until it scored 9/10 (6.5 → 7.5 → 8 → 8.5 → 9). What changed:

- **The title is the plan.** "Move 151 items to ~/Downloads", "27.7 GB",
  "Largest videos" sit in the pane's title; the pane under it no longer
  repeats verb, echo and counts on separate lines. Notes (read-as,
  creates, skipped) are one wrapping strip that vanishes when empty.
- **Rows.** No fill on ticked rows; a neutral checkbox with an accent
  tick, so the *un*ticked row is what stands out. Select all is the
  checkbox in the column header. Files sit indented under their folder,
  whose leaf name is bright and path dim, `~` for home.
- **One destination is said once.** When every row goes to the same
  folder it is in the title, not on each row; the column then shows only
  names that had to change ("Renamed to", amber, the end of the name),
  and those rows and their folders sort first.
- **App data is left out by default.** Rows under a dot folder or inside
  an `.app` bundle start unticked and sort last, with an amber "N in
  hidden or app folders left out · Include". Not applied to the
  selection or to a folder the sentence names. (`state::is_app_data`.)
- **Refusals lead with the reason** ("No folder called “X”") and, for a
  missing folder, the apply shortcut runs "Create “X” and continue".
- **Commands** are one highlighted row with their neighbours from the
  command table under "Related"; answers have column headers, a total,
  and sizes capped by what is allocated (a sparse 8 TB disk image is not
  8 TB). Every list state has the same keys — ↑↓, ↩, esc — and says so.
- The apply shortcut is drawn inside the primary button; the scope
  control shows what the sentence fixed ("This folder", "Downloads").

### Natural phrasing: rewrite, then parse (`magic/natural.rs`)

The grammar stays strict — one spelling per idea — and is not taught
paraphrases. Instead `understand` **rewrites the sentence into the
grammar's spelling and hands that to the strict parser**. It works
because Magic's language is a small closed vocabulary around a few open
slots (which files, which folder); everything flexible is a table.

1. *Trim* — punctuation, lead-ins ("could you please"), sign-offs.
2. *App phrases* — strip the framing verb ("turn on", "switch to", "make
   it", articles) and look the rest up in the command table.
3. *Paraphrase* — fixed rewrites: verbs ("get rid of" → `delete`),
   sentence shapes ("put X in Y" → `move X to Y`, "throw X away",
   "which files are the largest"), comparatives ("more than 30 days
   old" → `older than 30 days`), a bare trailing "here".
4. *Typos* — **only if the sentence still doesn't read.** A word one
   edit (two for 8+ letters) from exactly one vocabulary word is fixed
   and the sentence tried again. Never touched: a word with a digit or
   punctuation (a file name), and real words that happen to sit one edit
   from the vocabulary (`here`/`where`, `game`/`name`, `films`/`files` —
   the `REAL_WORDS` list, derived offline from a dictionary).

Also: a misspelt *folder* resolves to the one folder on screen or among
the well-known ones that is a single slip away; `here` / `this folder`
is the folder on screen; `find X` / `where is X` hands X to the ordinary
search; Enter on a sentence Magic can't read searches file names for it.

The safety property is unchanged: whatever was rewritten is shown back
as **Read as: …** in canonical words on the plan or card, in every
state, before anything runs. A plan that matches nothing offers the
typo-fixed sentence as *Did you mean …?* rather than applying it.

`Auto` mode does not use any of this — it still acts only on the strict
grammar and the measured gate.

**The measure** is the corpus in `natural.rs` (`the_corpus_reads_as_
intended`): ~135 phrasings with their intended reading. 62 were written
after the tables, as a held-out probe: 46 read correctly cold, 11 more
after table additions, and 5 are deliberately not read (below). New
phrasings go in the corpus first.

| Path | Time |
|---|---|
| `understand`, paraphrase + lead-in | 10 µs |
| `understand`, typo (second parse) | 87 µs |
| `understand`, nothing reads (every word tried) | 85 µs |

Only while Magic is explicitly on. Cost in the binary: the tables and a
~2.5 KB word list.

### Not done

- Compound commands ("sort by size descending"), "duplicate this"
  (a copy beside the original), "number these files".
- Magic follows the search scope: with *Anywhere* selected, "move the
  pdfs to Docs" proposes every pdf on the disk. Reviewable, but worth
  deciding whether a command with no `from` should default to the
  folder on screen.
- The `from`/`in` ambiguity above has no fallback to the literal words.
- Tag plans aren't in the undo journal.
- `organize … by month` buckets by UTC date.
- Destination editing beyond *Create here* is still "edit the sentence".

---

## Goal & scope

> **Superseded in part by the v2 pivot above** — the "no manual mode
> toggle" premise no longer holds. The rest (what a command is, how a
> plan is built and executed) is unchanged.

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
- **Fuzzy hits are excluded from a command's rows** (added 2026-07-31,
  `usable_in_plan`). The bullet above says the rows under the card are
  "exactly the set the plan acts on" — which made subsequence matching a
  liability rather than a feature, because those rows *are* the batch. In
  dogfooding, `rename gravloc to …` returned 148 rows of which 4 contained
  "gravloc"; the rest were fuzzy hits like
  `xstate-graph.development.cjs.js`, every one of which confirm would have
  renamed. Fuzzy is how you *find* a file you then look at; it is not
  evidence you meant to modify it. Ordinary searches are unchanged.
  *Since closed:* command queries now ask the daemon for non-fuzzy
  matches (`Query::fuzzy = false`), so the exclusion happens at the
  source on every platform and `usable_in_plan` is gone.
- **Filter chips describe the command, not the raw query.** Chips are
  derived from `selection.source` via `expand_as_description`, the same
  expansion the command itself used. Deriving them from the whole query
  advertised filters the plan does not apply — "move all pdfs from
  downloads to documents" grew a `kind:document` chip off the
  *destination* word while the command carried only `ext:pdf`.
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


## September 2026 usability pass

- Magic's status bar follows the plan checkboxes and shows loading progress or
  an actionable failure instead of an unrelated search selection.
- Separate Select all / Deselect all controls sit above the virtualized rows.
  The footer stays visible, includes the selected count on the apply button,
  and disables applying during loading or when nothing is selected.
- Failed searches leave loading and expose Retry / Cancel. Empty or oversized
  plans suggest narrowing the command or choosing Current folder. Refresh (and
  Cmd+R/Ctrl+R) rebuilds the preview; unchanged operations retain exclusions.
  Newly discovered operations in a refreshed plan start unchecked.
- Plan construction runs on a background executor. Preview avoids per-hit
  filesystem identity checks; execution retains both the whole-batch check and
  the check immediately before each operation.
- Exhaustive streams use complete child-range or literal-posting candidate sets
  when small. Overlay changes remain included. Exceeding the candidate bound
  falls back to the full scan rather than truncating the plan.
- Benchmarks on a synthetic 100,000-entry catalog: 100-file folder ~67 µs,
  rare literal name ~2.6 µs, full broad stream ~67 ms. These measure catalog
  retrieval, excluding IPC and plan construction.

Validation: app unit tests, index-v2 integration tests (including scope,
posting/overlay completeness, and large-scope fallback), and native macOS
inspection of loading progress, oversized-query guidance, ready-plan controls,
and selected counts. The existing long churn qualification remains opt-in.
