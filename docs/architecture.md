# Architecture

```
            ┌────────────── config.toml ──────────────┐
            ▼                                          ▼
  media roots ──scan──▶ index.db (files, metadata)    events/*.jsonl (history)
  (disk, NAS)                │                          │ replay
                             ▼                          ▼
                       classify(path) ───────▶  Library = files ⨝ State
                                                        │
                                   ┌────────────────────┼─────────────────┐
                                   ▼                    ▼                 ▼
                                  TUI                  CLI         mpv (JSON IPC)
                                   │                                      │
                                   └──── watched/progress events ◀────────┘
```

All logic lives in the library crate (`src/lib.rs`); `src/main.rs` only
calls `cli::main()`.

| Module | Responsibility |
| --- | --- |
| `parse` | File name → title, episode, version, kind (episode / special / extra / movie). Purely lexical. |
| `identity` | Title → series key normalization (`series_key`), plus `legacy_series_key` for keys from before kana voiced marks were kept. |
| `index::scan` | Walks roots with a small thread pool (network mounts are latency bound); also reports whether a root is a mount point. |
| `index::classify` | Applies folder context: archive folder = series, numbered entry folders = series of their own, `Extras/` = extras, … |
| `index::db` | SQLite cache: files, scans (one row per root scan, with completeness), metadata, match attempts, and per root whether it was a mount point and at which path (`root_mounted:<name>` in `kv`, dropped with the root), which decides if an empty scan means "emptied" or "unmounted"; and when each root was last looked at, reached or not, and with which folder and `ignore_dirs` (`scan_tried:<name>`, dropped with the root), which tells the TUI whether it needs to scan on start (`scan_fresh_for`). Two counters, of scans that changed which files are present or roots becoming (un)reachable (`file_changes`) and of metadata writes (`meta_syncs`: updates, links, renames), let an open TUI notice what `anipv update` stored. Also when each AniList id was last asked about and whether it was found (`anilist_checks`), so unknown ids are retried daily rather than on every start. Disposable: a schema version change drops and rebuilds it; only purely additive versions are upgraded in place (`UPGRADABLE`). |
| `events` | Append-only per-device log, replay into `State`. The source of truth. |
| `library` | Joins classified files with the replayed events and the cached index; applies `split` folders; maps history recorded under legacy series keys onto the keys of the series on disk (`Ctx::library` then makes that permanent with an `alias` event and moves cached rows), and history recorded under a file's earlier key onto its series now; groups franchises; computes next-up, new counts, search text. |
| `folders` | The files on disk as a folder tree, with what is watched below each folder: the Files view's tree mode (listing, path search, the files below a folder to queue or mark). |
| `franchise` | Groups series into franchises (franchise folder, season markers, AniList prequel/sequel links) and orders them for watching. `Ctx::auto_complete` rolls on to the next entry; `app::catch_up` marks everything up to an entry watched. |
| `mpv` | Builds command lines, spawns mpv with `--input-ipc-server`, follows `path`/`time-pos`/`duration`/`pause` and `end-file` (including the file mpv is already on when anipv connects), and hands out a `Remote` that sends `keypress` commands over the same socket. |
| `import_fish` | Parses fish history, extracts `mpv` file arguments, plans watched events (full path first, then a unique base name; ambiguous names are skipped). |
| `meta` | Offline anime DB matching; anonymous AniList airing queries. `meta::ratelimit` and `meta::anilist` keep to [AniList's rate limits](https://docs.anilist.co/guide/rate-limiting) and decide when to give up on AniList; the rules, with their reasons and tests, are in [anilist.md](anilist.md). |
| `tui` | ratatui front-end: `app.rs` (state, keys, background work), `ui.rs` (rendering). |
| `cli` | clap commands. |
| `demo` | Generates a fake library for tests and screenshots. |

## Threads

The TUI's main loop polls the terminal every 100 ms and drains an `mpsc`
channel fed by background threads:

* **scan**: opens its own SQLite connection (WAL mode), reports progress, then `ScanDone`.
* **metadata**: matches unmatched series against the offline DB, then refreshes stale airing info from AniList, and the prequels and sequels of inbox series and of series you engage with (monthly).
* **another process**: `anipv update` (from a systemd timer) scans and syncs metadata on its own connection; every 30 s the UI thread compares the cache's change counters with what it last loaded, and reloads what changed.
* **mpv**: one per playback; forwards `PlayerEvent`s. Its first is `Connected`, carrying the `Remote` the UI thread uses to send keys in remote mode.

The UI thread is the only one that writes events, and the library is rebuilt
after every change (≈ 50 ms for 13k files). It keeps the event log, the
classified files and the offline database in memory: the log is read again
only when a log file changed (another device's events synced in, another
anipv process writing) or after a scan, and only new files are classified.

## Identity, briefly

* **Series** = normalized title key (deterministic, see `docs/events.md`).
  Different names for the same show are joined with `alias` events.
* **Item** = `(kind, episode, label)` within a series. Versions and differently
  named copies collapse into one item.
* **File** = absolute path in the local index only. Events keep at most the
  file's base name as evidence (`file` on `watched`/`progress`/`unwatched`),
  never its path, so history survives moves, renames and different mount
  points per machine.

## Code style

* Clippy runs with `pedantic` plus selected `nursery`/`restriction` lints
  (see `[lints.clippy]` in Cargo.toml and `clippy.toml`); CI fails on warnings.
* Format strings name their placeholders: `format!("{title} · {n} eps")`. Rust
  can only inline plain variables, so bind expressions to locals or pass named
  arguments (`format!("{root}: +{new}", root = r.root, new = s.new)`) instead of
  positional `{}` lists. Clippy only checks plain variables, so `tests/style.rs`
  checks the rest: it fails on any `{}`, `{:?}` or `{0}` in a format macro.
* No `unwrap()` outside tests; casts that can lose data use `try_from`.
* Suppress a lint only locally, with `#[expect(clippy::…, reason = "…")]`, which
  fails once it is no longer needed (`allow_attributes` and
  `allow_attributes_without_reason` enforce it).

## Testing

* Unit tests next to each module; property tests for event replay.
* `tests/parser_corpus.rs`: one synthetic file name per naming convention with expected parses (golden file).
* `tests/tui.rs`: renders every screen of the demo library on a `TestBackend` and snapshots it with `insta`.
* `tests/cli.rs`: runs the binary end-to-end, including playback against
  `anipv fake-mpv` (hidden; `src/fake_mpv.rs`), a stand-in that speaks mpv's IPC protocol.

The demo library (`src/demo.rs`, `anipv demo`) and the fake player exist only
with the `demo` Cargo feature, never in a regular build. Tests and examples
turn it on through the crate's dev-dependency on itself, so a plain
`cargo test` has them; `just demo`, `just record` and the recording workflow
pass `--features demo`.
