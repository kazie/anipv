# Changelog

All notable changes to this project are documented here.
The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the
project uses [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- The episode list links the franchise entries around it: a `▲` row first
  and a `▼` row last open the one before or after (⏎), and `[` / `]` do from
  any row. `g` / `G` still go to the first and last episode (the links are
  one more `k` / `j`), and Esc goes back to where you opened the series from.
- `anipv update`: scans every media folder, then updates metadata as the TUI
  does on start (and the offline database weekly), made to run unattended.
  An unreachable folder or network trouble is a warning and changes nothing.
  `just install-timer` runs it every 15 minutes from a systemd user timer
  (`contrib/systemd/`). With `refresh_untracked = true` it also refreshes
  series you don't track, 50 at a time, a month apart.
- `anipv init` writes a commented config file: every setting with its
  default and what it does, your folders filled in
  ([`config.example.toml`](config.example.toml) is the same file).
- AniList rate limits are kept to: requests are paced by AniList's own
  count of what is left (other clients on your connection included) with a
  small gap against its burst limiter, and a "too many requests" answer is
  waited out and asked again instead of failing (up to 2 minutes in all per
  update while you watch, with a countdown in the TUI header; 10 minutes in
  `anipv update`; title searches may use half of it). Pacing only makes a big
  update slower, never failed. A timeout longer than what is left fails at
  once, and so do the requests after it until it ends. A server error or
  failed request is retried once; when two requests in a row fail, or the
  rate limit outlasts the wait, the update stops asking instead of failing
  the same way for every batch. The rules, with their reasons, are in
  [docs/anilist.md](docs/anilist.md).
- The TUI skips its scan on start when every folder was looked at within
  `scan_fresh_for` minutes (30 by default), and picks up what another
  process (the timer) stored while it is open.

- Remote mode in the TUI: while mpv plays, keys are pressed in mpv (over its
  IPC socket, as mpv key names), so your own `input.conf` decides what they
  do: pause, seek, quit and so on, without reaching for the player window.
  It starts with every playback (`remote_auto = false` turns that off);
  `ctrl-c` hands the keys back to anipv and `v` hands them to mpv again. The
  playing bar shows when mpv is paused.
- The demo library (`just demo`) can play: its player is a fake mpv built
  into the demo build (linked or copied into the demo folder as
  `bin/anipv`, so a later build without the `demo` feature doesn't take it
  away), and remote mode can be tried without real videos. The
  README recording now shows playing an episode, skipping, pausing and
  quitting the player from anipv.

- `anilist_search = true` (off by default) and `anipv meta match --search`:
  series the offline database has no match for are looked up on AniList by
  title, so shows newer than the dataset can still get episode totals and
  airing times. This sends those series' titles (parsed from your file and
  folder names) to AniList, at most 10 per update and each at most once a
  month. A series links only when one answer goes by its name and can hold
  the episodes on disk; ambiguous ones are listed and left alone. Without a
  downloaded offline database every series without a link counts as
  unmatched; a failed search keeps the answer from the last one.

### Fixed

- The first file of a playback session was often not tracked: mpv had
  usually started it before anipv connected to its IPC socket, so that
  episode was neither marked watched nor counted in the playing bar. anipv
  now follows the file mpv is already on when it connects. Since mpv only
  reports a value that changed, each file also starts out with the last
  path and duration mpv reported (two files of the same length in a row
  could lose the duration).
- A special's kind in brackets by the episode number makes the file a
  special, as the bare word does: `Show - 01 (OVA).mkv` and
  `Show (OVA) - 01.mkv` are OVA 1, not a copy of episode 1 (playing one
  marked episode 1 watched, and either file could be played for it). Only the
  kind changes, never the title or number, and a tag after an episode's
  title changes nothing (`Show 01 Title [Special]` is still episode 1).
- A release version in brackets (`[v2]`, `(V2)`) counts, so of two copies of
  an episode the newer version is played, as with `05v2`.
- With nothing followed, Up next points to the Series tab as `3`, not `2`
  (the Inbox).
- Ctrl-C and Ctrl-R work with Caps Lock on.

### Changed

- A queued episode stays in the queue until it is watched (played to the
  end, or past `watched_threshold`), not just until it starts. One started
  by mistake, stopped partway or skipped in mpv is still queued, and `p`
  resumes it. The Queue view highlights the file mpv is on.

- How you set a status, when not by hand, is recorded on the event (`via`:
  `fish_import`, `catch_up`) instead of in its note, so a note is always
  yours and shows as such, whatever it says. A log written before this
  keeps such statuses as notes ("from fish history", "caught up"), which now
  show as yours: rewrite those lines once to `"via":"fish_import"` /
  `"via":"catch_up"` without the note (see docs/events.md).

- The series header is calmer. The franchise's name is in its border, the
  title is coloured by status (no status word), and the small facts (★,
  members, the AniList id as `ⓐ12345`, `+2 names` merged into it) sit at the
  right, giving way one by one on narrow terminals instead of wrapping. The
  second line has progress, new episodes, the format and your own note; the
  franchise's entries are at its right, named
  short (`S1 ✓ → S2 ✓ → ▸S3◂`, `Pretty Gray → ▸Full Gate!◂`). When the next
  episode airs is in the bottom border. AniList's name for the series shows
  only when it's another name (`ⓐ JoJo no Kimyou na Bouken…`). Notes anipv
  wrote itself ("all 12 episodes watched", "caught up") and the series key
  are gone, and
  statuses from fish history show a puffer fish `ϵ( 'Θ' )϶` in the border.
  A folder numbering on past AniList's count (later seasons in one folder)
  shows `0/51 (ⓐ24 eps)`, not `0/24`.
- Franchises without a folder are named without a season (`Grand Blue`, not
  `Grand Blue S2`).
- `U` (undo merges) lists the names it would split off and asks first.
- The integration tests' stand-in for mpv is now written in Rust (the hidden
  `anipv fake-mpv`) instead of Python, so the tests no longer need (or
  quietly skip without) `python3`. Its `--demo` mode plays in real time and
  answers remote-mode keys.
- `anipv demo` and `anipv fake-mpv` are only built with the new `demo` Cargo
  feature: a regular build carries no demo or test tooling. Tests and
  examples enable it themselves; `just demo` and `just record` pass it.

- The offline anime database now comes from the community continuation
  (`cedya77/anime-offline-database`) because the original manami-project
  dataset was archived after its 2026-27 release (2026-07-04). The archived
  original is the fallback when nothing is cached yet. New config key
  `offline_db_url` picks another source; `anipv meta update` says when the
  dataset it got is more than 90 days old. Series that couldn't be matched
  are matched again after the first update.
- `meta link` also fetches the series from AniList (with network metadata
  on), like linking in the TUI.
- Every CLI command that writes (`mark`, `merge`, `import-fish`, `meta
  link`, …) completes followed series that are finished and fully watched,
  like the TUI (except a `status` you set on a series that was already
  finished).
- The TUI header's "offline" warning for a media root that isn't mounted comes
  from the scan (started at launch, so it shows once the scan has reached the
  root), not from a check while loading; a scan that fails to record its
  result no longer counts as offline.
- Loading a large library (every CLI command, TUI start) parses file names
  on several threads, each about 10% faster (the release group and checksum
  tags, which nothing used, are no longer kept).
- `export` writes its output buffered (about half the time on a large
  library), `meta search` no longer keys every title in the offline database
  first (about half its time), and `import-fish` compares paths faster when
  many files share a name (`Season 1/01.mkv` in every show).
- Quitting the TUI with `q` or Ctrl-C always asks first; `q`, `y` or Enter
  confirms, any other key cancels. Ctrl-C with any other popup
  open closes it (like Esc) instead of asking, so a typed draft isn't lost.
- Files in an extras folder (`Extras`, `Bonus`, `NC`, …) are named without
  the release group, checksum and release tags, and without the show's
  name: `creditless ed 01` instead of `groupa show creditless ed 01 bdrip
  1920x1080 hevc truehd`. Watch state recorded under the old name moves to
  the new one, found by the file name the log recorded; the log itself is
  not rewritten.

### Fixed

- A special word after an episode number followed by more words is the
  episode's title: `Show - 05 Special Training` is episode 5, not a special
  (`Show - 05 OVA` still is one). Watch state recorded for such a file as a
  special stays with the special.
- `Show Movie 02` and `Show Movie 2` are the same movie.
- A file without a title of its own in a show's folder keeps the season in
  its name: in a download folder `Show/S02E05.mkv` is *Show S2* episode 5,
  not episode 5 of season 1 (it already was in an archive folder).
- Files in a `Season 0` / `S00` folder are specials, not episodes that
  collide with season 1's (a season in the file name still wins: an
  `S02E05` there is season 2; an `S00E05` in a `Season 2` folder stays a
  special of season 2's series).
- A range of specials (`Specials/Show - 01-02.mkv`) provides each of them.
- A file linked from elsewhere in the same root is indexed once, under its
  own path.
- An empty scan of a folder that has become a mount since its last scan is
  no longer believed (it may be an automounter's placeholder), so its files
  aren't marked gone.
- With mpv's `keep-open`, quitting on an episode's last frame counts it as
  watched (it never did with `watched_threshold = 1.0`: no end of file is
  reported then).
- An event log file that can't be read (no permission, a folder named
  `*.jsonl`) is a warning (in the TUI's status line, on stderr for CLI
  commands) instead of stopping anipv from loading any history. While it
  can't be read, nothing is completed automatically and CLI commands that
  write refuse (what they'd decide from the rest may be wrong, and its
  history may hold a status you chose since); the TUI reads it as soon as
  it can. `doctor` lists it once, and says how many malformed lines it
  didn't list.
- If the event log folder disappears while the TUI runs (an unmounted
  share), the history already loaded stays, with an error, instead of
  turning into an empty library.
- A custom offline database without a `lastUpdate` no longer makes every
  start match the whole library again.
- A status chosen for a series that is already finished and fully watched
  stays (e.g. following it for a rewatch) instead of being completed again
  at once, until you change the status again. Older versions ignore the new `keep` field in the event log.
- Progress on the playing episode is kept when the connection to mpv breaks
  (e.g. mpv crashes).
- The position of each new episode in a queue shows up right away.
- Events from the same second in a sync-conflict copy of this device's log
  apply in the same order before and after a restart.
- `mark --unwatched` counts episodes anipv doesn't know separately ("2
  already were, 1 not known"), and an episode listed twice counts once.
- fish shell completions load again (a status description broke them).
- A CLI command that fails after recording something (e.g. `play` with
  its output closed) still completes the series it finished.
- `play -n 0` is an error instead of claiming there is nothing new.
- `anipv export | head` no longer ends with "error: Broken pipe".
- `anipv demo DIR` refuses a folder that isn't empty: run on an existing
  home it replaced the config and added the demo's history to the logs.
- Titles with emoji no longer push table columns out of line.
- `init` never names two roots alike but for case, and `scan --root` takes
  a root's exact name over one that differs only in case.
- The config file is replaced whole when saved, never left half written.
- An empty `NO_COLOR` keeps colors (no-color.org), and an empty or relative
  `XDG_RUNTIME_DIR` is ignored.
- `import-fish`:
  - A shared folder like `Season 1` no longer counts as proof of the show:
    another show's `Season 1/01.mkv` was imported as the indexed one's.
  - A file that isn't indexed and has no title in its name or folders
    (`01.mkv`) is skipped and listed, instead of becoming a series named
    after the file.
  - Plays behind fish's `and`, `or` and `not`, `prime-run` and
    `gamemoderun`, and after a line continuation, are found; file names in
    a `# comment` and brace expansions (`{01,02}.mkv`) are not imported.
  - A relative or empty `XDG_DATA_HOME` is ignored when finding the
    history, like the XDG spec says.
- TUI:
  - Every note of a metadata update shows (they replaced each other), and an
    error from the same update no longer hides them.
  - A warning that stays (a read-only events folder, say) shows when it
    appears instead of on every reload, where it hid each action's message.
  - A scan reports folders it could not read and roots it could not record,
    also when new shows arrived (as an error; an unmounted root is still only
    shown in the header).
  - After a metadata update that only brought a new offline database, series
    are matched against it without waiting for a rescan.
  - Ctrl-C in the search box clears the search, like Esc.
  - Tab and Shift-Tab in a series move on from the tab it was opened from.
  - The merge picker lists every series (similarly named ones first), not
    only those whose title shares the first word.
  - `f` in the Series view ends a search first (a search lists every series,
    so the filter did nothing), and the title says so.
  - "the queue is unchanged" is only said when there is a queue.
  - The queue scrolls like the other lists: after it shrinks while scrolled
    down, it fills the view again instead of showing blank rows.

## [0.1.0] - 2026-10-08

First release.

### Added

#### TUI
- Views: Up next (the next unwatched episode of every followed series, new
  arrivals and next airing time), Inbox (new shows in your download folders,
  new seasons of shows you watch first; follow, skip or later), Series
  (filterable by status, fuzzy search), Files and Queue, plus an episode view
  per series with extras grouped under their episode.
- Queue episodes from several series and play them in one mpv session, or play
  a single episode; copy the equivalent `mpv …` command.
- Set statuses (following, paused, dropped, completed, skipped) with an
  optional note; merge, unmerge, rename and link series to AniList.
- Background scanning and metadata refresh; `?` shows every key binding.

#### CLI
- Everything the TUI does is scriptable: `tui` (the default), `init`, `scan`,
  `next`, `new`, `ls`, `show`, `play` (with `--print`), `mark`, `status`,
  `merge`, `unmerge`, `rename`, `import-fish`, `meta`, `doctor`, `export`
  (JSON), `completions`, `man` and `demo` (a fake library to try it out).

#### Parsing and library
- File name parser for bracketed, dotted, underscored and bare-number names:
  `S02E01` and `1x05` markers, season 0 as specials, `v2` versions,
  multi-episode files, OVAs, movies and extras (openings, endings, previews).
- Ongoing roots (series from file names) and archive roots (one folder per
  series), with `Season N` folders as separate seasons.
- History is keyed on series and episode, not file paths, so moving or
  renaming files keeps it.

#### Sync and event log
- History is an append-only JSON-lines log, one file per device. Sync the
  folder with any tool; merging is conflict-free and replay is deterministic.

#### Playback (mpv)
- Tracks mpv over its IPC socket: finished episodes are marked watched, and
  playback resumes where you stopped.

#### Metadata
- Episode totals and airing dates from the offline anime-offline-database and
  anonymous AniList queries (no account; can be turned off with
  `anilist = false`). Automatic matching, manual linking by id or URL, search.
- Followed series are completed automatically once they have finished airing
  and every episode is watched.

#### Import
- `import-fish` imports watch history from `mpv …` commands in fish shell
  history, with a dry run and optional suggested statuses.

#### Configuration and diagnostics
- `anipv init` writes a config; the config is validated when it loads
  (unknown keys, invalid roots and values are rejected).
- `anipv doctor` checks the config, roots, event log and cache, and lists file
  names the parser could not make sense of.
- Shell completions and a man page.
