# The event log

Everything you *decide* (watched, dropped, merged, renamed) is stored as an
append-only log of JSON lines, one file per device:

```
events/
├── desktop.jsonl
└── laptop.jsonl
```

A device only ever appends to its own file. Syncing the folder between
machines therefore can't produce conflicts: there is never a second writer.
On start-up anipv reads **all** files and replays the events in
`(ts, dev, file, line)` order to rebuild the current state: `file` is the
file's position among the `*.jsonl` files sorted by path, so when two files
carry the same `dev` (a Syncthing conflict copy, two machines with the same
name) their events with equal timestamps are not interleaved line by line.

## Format

Every line is an object with a unix timestamp `ts` (seconds), the device
name `dev`, and an event kind `e`:

```json
{"ts":1790536811,"dev":"desktop","e":"watched","series":"one piece","item":{"kind":"episode","ep":"1180"},"file":"One Piece - 1180.mkv"}
{"ts":1790537000,"dev":"desktop","e":"progress","series":"grand blue s3","item":{"kind":"episode","ep":"8"},"pos":612.0,"dur":1440.0}
{"ts":1790538000,"dev":"laptop","e":"series_status","series":"k on","status":"dropped","note":"not for me"}
{"ts":1790539000,"dev":"laptop","e":"alias","from":"even the student council has its holes","to":"seitokai ni mo ana wa aru"}
```

| `e` | Fields | Meaning |
| --- | --- | --- |
| `watched` | `series`, `item`, `file?` | Item finished. |
| `progress` | `series`, `item`, `pos`, `dur?`, `file?` | Stopped part-way. Ignored if the item is already watched (re-watching doesn't un-watch). |
| `unwatched` | `series`, `item`, `file?` | Reset an item. |
| `series_status` | `series`, `status`, `note?`, `auto?` | `following`, `paused`, `dropped`, `completed`, `skipped` (never started, not interested) or `untracked`. A status a reader doesn't know (written by a newer version) reads as `untracked`. `auto: true` marks a change anipv made itself: a followed series is completed once it has finished airing and every episode is watched. That happens at most once per series, so setting it back to following sticks. |
| `alias` | `from`, `to` | Treat series key `from` as part of `to` (e.g. English vs. romaji names). |
| `unalias` | `from` | Undo an alias. |
| `title` | `series`, `title` | Display title override. |
| `meta` | `series`, `anilist?`, `episodes?` | Link to an AniList id and/or set the episode total (whichever is present). |
| `unlink` | `series` | Remove the AniList link and stop matching the series automatically. |

Unknown kinds are ignored, so older versions can read logs written by newer ones.
The same goes for an item whose `kind` is not one of the four below
(`episode`, `special`, `extra`, `movie`): the line still parses and stays in the
log, but the event is ignored (no item, no watch state), and this version
never writes such a line itself.

### Series keys

`series` is a normalized title: lower-case, diacritics and punctuation
removed (but not the kana voiced marks), a leading "the" dropped,
season markers collapsed (`Season 3`, `3rd Season`, `S03` → `s3`):

| Title | Key |
| --- | --- |
| `Grand Blue Season 3` | `grand blue s3` |
| `Bakusou Kyoudai Let's & Go!!` | `bakusou kyoudai lets and go` |
| `POKÉTOON` | `poketoon` |
| `バカとテスト` | `バカとテスト` |

Keys are derived deterministically, so two machines that see the same show
agree on its key without talking to each other.

#### Key changes

Series keys are stable identity: the history in the log is never rewritten.
An older key spelling folded the kana voiced marks (dakuten and handakuten),
so `バカ` and `ハカ` both became `ハカ`; keys now keep them. This version
recognises the older spelling and attaches its history to the series on disk
automatically, making that permanent with one ordinary `alias` event from the
old key to the new one, written once (not again when the log already has an
`alias` or `unalias` from the old key). If that merges a different show, undo
it with `anipv unmerge` (or `U` in the TUI).

### Items

`item` identifies something watchable within a series, independent of the file:

```json
{"kind":"episode","ep":"7"}
{"kind":"episode","ep":"12.5"}
{"kind":"special","ep":"2","label":"ova"}
{"kind":"extra","ep":"20","label":"next ep pv"}
{"kind":"movie","label":"movie 2"}
```

Different files of the same episode (`- 03`, `- 03v2`, another copy)
share an item, so it doesn't matter which file you played.

## Merge rules

* Merges (`alias`/`unalias`) are settled first; then every event is applied,
  in `(ts, dev, file, line)` order, to its series' canonical key. Later events win
  for every field, including ones recorded under a name that was merged later,
  so e.g. un-watching after a merge sticks.
* Replaying is idempotent: a log duplicated by a sync tool gives the same state.
  Property tests in `src/events.rs` check this and file-order independence.

## Compatibility

The log is the one format that lives forever and is read by every version you
run on any machine, so it only ever changes in compatible ways:

* **Additive only.** New kinds and new optional fields may be added; existing
  kinds and fields are never renamed or given a new meaning. A new meaning gets
  a new kind (that is why `unlink` is not "a `meta` without an id").
* **Old readers skip what they don't know.** Unknown event kinds are ignored,
  unknown fields inside known kinds are ignored, and so are item events whose
  item `kind` is unknown (all tested). Skipped lines are never rewritten or
  dropped: the files are only ever appended to.
* **Identity is frozen.** Series keys and item labels are derived from file
  names by code; `tests/fixtures/filenames.tsv` pins them (`label`, `key`
  columns), so any change is a deliberate, reviewed decision. When one has to
  change, the old key is kept as `identity::legacy_series_key` and history is
  mapped onto the new one when the library is built (see above), instead of
  rewriting the log.
* **Evidence is kept.** `watched`, `progress` and `unwatched` record the file
  name involved, so if identity rules ever had to change, keys could be
  recomputed from the log itself instead of migrated.

Everything else (`index.db`) is a cache: it is dropped and rebuilt whenever
its schema version changes, so it hardly needs migrations either (a version
that only adds tables is upgraded in place instead, keeping the files'
first-seen times).

## Tips

* `anipv export` prints the replayed state as JSON.
* The logs are plain text: `grep`, `jq` and `git diff` all work on them.
* To move a device's history to a new name, rename its file *and* set
  `device` in the config.
