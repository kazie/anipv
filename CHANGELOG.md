# Changelog

All notable changes to this project are documented here.
The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the
project uses [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Changed

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
- Quitting the TUI with `q` or Ctrl-C always asks first; `q`, `y` or Enter
  confirms, any other key cancels. Ctrl-C with any other popup
  open closes it (like Esc) instead of asking, so a typed draft isn't lost.

### Fixed

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
