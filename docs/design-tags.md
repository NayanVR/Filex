# Design: Tags (Phase 2d, block 8 item 1)

Status: **design, not yet implemented.** Written 2026-07-22 as the
gate the roadmap requires before any tag code (see
`docs/ui-enhance-roadmap.md` block 8). This settles storage,
rename/move behavior, watcher interaction, the read/write interface,
how `tag:` search works, and the UI surfaces, so implementation is a
matter of following it rather than discovering the hard parts mid-code.

## Goal & scope

Let a user attach named (optionally colored) **tags** to files and
folders, see them in the details panel and a sidebar section, and
filter by them (`tag:work`). Tags are **metadata**, not content — this
stays inside Phase 2d and nowhere near content parsing / semantic search
(still Phase 3, per CLAUDE.md).

Hard requirements carried from the roadmap:

- **macOS: interoperate with Finder tags** so a tag set in filex shows
  in Finder and vice-versa.
- **Windows/Linux: a sidecar store** in the data dir (no native tag
  system to interop with).
- Tag chips in the details panel, a sidebar "TAGS" section, and a
  `tag:` search filter.
- Local-only; tags are private metadata over private filenames (the
  telemetry stance).

Non-goals: tag colors as first-class theming, smart/saved searches,
tag hierarchies, syncing. Colors are read/preserved (Finder has them)
but filex's own tag creation can stay colorless in v1.

## Storage — per-OS behind a trait

Mirror the indexer's shape: one `TagStore` trait, a per-OS
implementation selected by `cfg`, and a shared behavioral test suite so
the platforms can't drift (CLAUDE.md's "single suite of behavioral
tests" rule).

```rust
// filex::tags
pub struct Tag { pub name: String, pub color: Option<TagColor> }

pub trait TagStore: Send + Sync {
    /// Tags on `path`, in a stable order. Empty when none / unreadable.
    fn tags(&self, path: &Path) -> Vec<Tag>;
    /// Replace the whole tag set on `path`.
    fn set_tags(&self, path: &Path, tags: &[Tag]) -> Result<()>;
    /// Every (path, tags) the store knows about — powers the sidebar
    /// TAGS section and the `tag:` filter. Bounded by what the store
    /// tracks (see per-OS notes on enumeration).
    fn all(&self) -> Vec<(PathBuf, Vec<Tag>)>;
}
```

### macOS — Finder-interop via the `_kMDItemUserTags` xattr

Finder stores user tags in the extended attribute
`com.apple.metadata:_kMDItemUserTags`, whose value is a **binary
property list** holding an array of strings. Each string is
`"Name"` or `"Name\n<colorIndex>"` where the color index is 0–7
(0 = none, 1 = grey, 2 = green, 3 = purple, 4 = blue, 5 = yellow,
6 = red, 7 = orange). Writing this xattr makes the tag appear in
Finder; reading it picks up tags Finder set.

Two viable access paths — **decide: NSURL resource values** (higher
level, Apple-blessed, already have objc2-foundation):

- `NSURL` `getResourceValue:forKey:NSURLTagNamesKey` /
  `setResourceValue:forKey:` handles the plist encoding and color
  suffix for us; colors also available via `NSURLLabelNumberKey`.
- Fallback / lower level: `getxattr`/`setxattr` (libc, now a macOS dep)
  + a tiny binary-plist array reader/writer if NSURL proves awkward.

Enumeration (`all`) on macOS: xattrs aren't centrally listed. Spotlight
*does* index `kMDItemUserTags`, so `all()` can run an `NSMetadataQuery`
/ `mdfind "kMDItemUserTags == '*'"` to find tagged files without
walking the disk. If we'd rather not depend on Spotlight, `all()` is
backed by the same sidecar index described next (written alongside the
xattr on every `set_tags`), and the xattr stays the interop channel.
**Decision: keep a sidecar index on every platform** (below) as the
enumeration source of truth; on macOS the xattr is written *in addition*
for Finder interop and is authoritative for a single file's tags on
read (so Finder-side edits win).

### Windows / Linux — sidecar store

No Finder to interop with. Store tags in a single JSON file in the data
dir, `<data_local_dir>/filex/tags.json`, shaped as a map from
**canonical absolute path** to tag list:

```json
{ "/home/x/report.pdf": [{ "name": "Work" }, { "name": "Urgent" }] }
```

Rationale for one central file (vs. per-file sidecars like
`report.pdf.tags`): enumeration (`all`) is a single read; no littering
the user's folders; no interaction with the filename watcher (a stray
`.tags` file would show up as a real entry). Cost: the path key breaks
on external rename/move (addressed below).

Linux note: `user.*` xattrs exist and some tools use ad-hoc conventions,
but there is **no** cross-tool standard equivalent to Finder tags, so
interop buys nothing — the sidecar is the honest choice. (If a concrete
interop target emerges, a Linux xattr backend can slot behind the same
trait later.)

## Rename / move / delete behavior

This is the subtle part; enumerate every path that mutates a file's
identity.

- **macOS xattr**: travels with the file automatically on a same-volume
  `rename(2)` and on `NSFileManager` moves; `cp` may drop it (documents
  it as a limitation). So Finder-side and filex-side moves keep tags for
  free. Cross-volume copies lose them unless we opt to copy xattrs — v1
  accepts the loss (matches Finder's own copy behavior in some cases).
- **Sidecar (all platforms, since it's the enumeration index)**: keyed
  by path, so it must be **updated by our own file ops**. `filex::ops`
  already produces an `AppliedOp` for every move/rename/delete and an
  undo journal — hook tag-key migration into that same path:
  - `Moved{from,to}` / `Renamed{from,to}` ⇒ move the sidecar entry
    `from → to`.
  - `Deleted{original,..}` ⇒ drop the sidecar entry (undo restores it,
    so the journal carries the old tags to reinstate).
  - `Copied{to}` ⇒ copy the source's tags to `to` (Finder copies tags;
    match it).
- **External** renames/moves (done in another app, in the shell):
  filex can't observe the `from→to` pairing for the sidecar. Two
  mitigations, **decide**: (a) accept staleness — an externally-moved
  file loses its sidecar tags, and a lazy cleanup prunes keys whose path
  no longer exists; (b) key the sidecar by a stabler identity. On
  Unix the `(dev, inode)` pair survives renames but not copies/restores
  and isn't portable; on Windows the file id is similar. v1 chooses
  **(a) path-keyed + lazy prune** for simplicity and portability, and
  notes inode-keying as a future hardening. macOS is unaffected (its
  tags live in the xattr and move with the file); only the enumeration
  index goes briefly stale until the next `all()` prune.

## Watcher interaction

- Setting a tag must **not** trigger a filename reindex. The sidecar
  lives in the data dir (already outside indexed roots), so it's
  invisible to the indexers. The macOS xattr write touches the file's
  metadata, not its name/existence — FSEvents may emit a change event
  for the file, but the indexer keys on name/FRN and a metadata-only
  event is a no-op for it. Confirm during implementation that an xattr
  write doesn't cause a spurious delta; if it does, filter metadata-only
  events.
- Conversely, a **watcher-observed delete/rename** of a tagged file
  should reconcile the sidecar — but external ones aren't seen (above).
  The lazy prune on `all()` covers vanished paths.

## Search — the `tag:` filter

Tags are **not** in the filename index (which stores names only, by
design). Two options:

1. **Query the sidecar index directly.** `all()` already returns
   `(path, tags)`; a `tag:work` filter is a linear scan of that map
   (tens of thousands of entries at most — trivial) intersected with the
   text query's results. No index schema change. **Decision: this.**
2. Fold tags into the main index — rejected: coupling tag churn to the
   filename index's freshness/memory model for no speed win at these
   sizes.

Filter grammar (shared with the block-8 search-chips item, so define it
once): a leading/embedded `tag:NAME` token is parsed out of the query,
the remaining text runs the normal filename search, and results are
intersected with "paths carrying tag NAME". Multiple `tag:` tokens = AND.
This is the first concrete `key:value` filter; the search-chips design
doc reuses the same tokenizer for `kind:` etc.

## UI surfaces

- **Details panel**: a "Tags" row of chips (name, tinted by color when
  present) + an inline "add tag" affordance. Reuse the theme; a chip is
  a small rounded pill (`ui::details` gains `tag_chip`). Editing writes
  through `TagStore::set_tags`, which updates xattr (macOS) + sidecar.
- **Sidebar "TAGS" section** (collapsible, like the block-7 sections):
  the distinct tags across `all()`, each a colored dot + name; clicking
  runs a `tag:NAME` search. Refreshed when tags change (event from the
  tag store, same pattern as `SettingsStore`).
- **Search**: typing `tag:work` filters as above; a removable chip in
  the search field can come with the search-chips block.

## Colors

Read and preserve Finder's 0–7 color indices on macOS; map them to
theme-friendly chip tints. filex-created tags on Windows/Linux default
to no color in v1 (a later pass can add a color picker). `TagColor` is a
small enum mirroring Finder's palette so the two stay aligned.

## Testing plan

- Pure/portable: sidecar (de)serialization, path-key migration on
  Move/Rename/Copy/Delete `AppliedOp`s, the `tag:` tokenizer/intersect,
  color mapping — plain `#[test]`, no GPUI, fixture-driven.
- macOS xattr backend: round-trip against a real temp file on the dev
  machine (`#[cfg(target_os = "macos")]`), plus a recorded-plist fixture
  test for the encode/decode so CI (no macOS) still covers the format.
- Behavioral trait suite: one set of tests run against every `TagStore`
  impl (sidecar on CI's Linux/Windows; xattr on the dev Mac) so
  platforms hold the same contract.
- No perf regression to search-as-you-type: the `tag:` intersect is off
  the hot path unless a `tag:` token is present; measure the intersect
  cost at 100k tagged entries with a criterion bench before merging.

## Phasing (suggested implementation order)

1. `filex::tags` trait + sidecar backend + tests (portable, no UI).
2. Hook path-key migration into `filex::ops` (+ undo carries old tags).
3. macOS xattr backend + Finder-interop round-trip.
4. Details-panel chips (read + add/remove).
5. `tag:` search token + intersect (+ bench).
6. Sidebar TAGS section.

## Confirmed decisions (2026-07-22)

- **macOS access: NSURL resource values** (`NSURLTagNamesKey` /
  `NSURLLabelNumberKey` via objc2-foundation). Apple handles the
  binary-plist encoding and color mapping.
- **Sidecar keying: absolute path + lazy prune.** Our own ops migrate
  the key; external Windows/Linux moves lose tags and vanished keys are
  pruned on `all()`. Inode-keying is noted as future hardening.
- **Colors: full picker in v1.** filex-created tags can be colored, not
  just Finder-imported ones. `TagColor` mirrors Finder's 0–7 palette so
  a filex color round-trips into Finder. The details-panel chip UI ships
  with a color picker (phasing step 4).
