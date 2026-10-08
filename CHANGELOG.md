# Changelog

All notable changes to this project are documented here.
The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the
project uses [Semantic Versioning](https://semver.org/).

## [Unreleased]

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
