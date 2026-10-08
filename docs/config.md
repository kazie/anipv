# Configuration

anipv reads `~/.config/anipv/config.toml` (override with `ANIPV_CONFIG=/path`).
`anipv init` writes a starting point; every key is optional.

```toml
# Name of this machine in the event log (default: hostname).
device = "desktop"

# Where watch history lives. Point it at a folder you sync between machines
# (Syncthing, Nextcloud, a git repo, …). Default: ~/.local/share/anipv/events
events_dir = "~/Sync/anipv"

mpv = "mpv"
mpv_args = ["--fs"]
watched_threshold = 0.85
anilist = true

[[roots]]
name = "Downloads"
path = "~/Videos/Downloads"
kind = "ongoing"

[[roots]]
name = "Anime"
path = "~/Videos/Anime"
kind = "archive"
```

| Key | Default | Meaning |
| --- | --- | --- |
| `device` | hostname | Name used for this machine's event file (`<device>.jsonl`). Must be unique per machine. |
| `events_dir` | `~/.local/share/anipv/events` | Directory of per-device event logs: your watch history (absolute, or starting with `~`). Sync this folder to share history. |
| `mpv` | `"mpv"` | Player executable. |
| `mpv_args` | `[]` | Extra arguments for every mpv invocation, e.g. `["--fs", "--profile=anime"]`. |
| `watched_threshold` | `0.85` | Fraction of an episode you must reach for it to count as watched. mpv reaching the end of a file isn't enough on its own, so a partly downloaded episode stays in progress. |
| `anilist` | `true` | Network metadata: fetch airing info from AniList (anonymous, ids only) and let `ctrl-r`/`M` in the TUI download the offline anime database when it is missing or older than a week. With `false`, matching still uses an already downloaded database. |
| `roots` | `[]` | Media folders, scanned in order. See below. |
| `ignore_dirs` | `Screens`, `Screenshots`, … | Directory names never scanned (dot-directories are always skipped). |
| `extras_dirs` | `Extras`, `NC`, `Menus`, … | Files below these directories are extras (openings, menus, PVs). |
| `specials_dirs` | `Specials`, `OVA`, … | Numbered files below these directories are specials, not episodes. |

## Roots

Each root has a `name` (shown in the UI and stored in the index), a `path`
(absolute, or starting with `~`, which is expanded) and a `kind`:

* **`ongoing`**: a flat download folder. The series is parsed from each file
  name (`[GroupA] Grand Blue S3 - 07 (1080p).mkv` → *Grand Blue S3*,
  episode 7). Files in sub-folders without a usable title borrow the nearest
  named folder's; a `Season 2` / `S02` folder is not a name but gives the
  season, so `Show/Season 2/01 - Title.mkv` is *Show S2*. A file that has its
  own title keeps it: the sub-folders do not change the series.
* **`archive`**: one folder per series (`Anime/One Piece/…`). The top-level
  folder name *is* the series title, whatever the files are called, so
  `Futari wa Precure/41 - Princess In Peril!.mkv` is episode 41 of *Futari wa
  Precure*. Seasons inside that folder stay apart: a season 2 or later, from an
  `S02E01` marker in the file name or from a `Season 2` / `S02` folder below
  the series folder, becomes its own series of the same title (`show s2`, shown
  as *Show S2*), while season 1 (or no season) is plain `show`. So
  `Show/Season 1/…` and `Show/Season 2/…` are two entries, not one series with
  clashing episode numbers. The file name's marker wins over the folder's, and
  a folder that already says `Show S2` is not given a second `S2`.

Roots must not overlap: a root inside another one (`~/Anime` and
`~/Anime/Done`) is rejected, since its files would be indexed twice. Use
`ignore_dirs` to leave a sub-folder out instead.

When a show moves from `Downloads` to `Anime`, its history follows, because
history is stored per *series and episode*, not per file path.

### Missing, emptied and unmounted roots

A scan marks files that disappeared as gone (their history is kept), except:

* A root that is missing or unreadable is never touched; its files stay
  listed until the folder is back.
* A scan that finds *no* video files in a root that had some is only believed
  if it is plausible. anipv remembers, per root, whether the folder was a mount
  point (on unix: on a different device than its parent folder) at its last
  scan that found files, and which folder that was. A plain local folder you
  emptied is believed, so its files are marked gone. A drive that was mounted
  and is now not (an empty mount-point directory) is ignored as "not mounted".
  A mount that is still mounted and empty is believed. With no record (an
  index from an older version, or a root whose `path` changed) an empty plain
  folder is believed too; if it really was an unmounted drive, its files drop
  out of the lists until the next scan with the drive mounted brings them
  back, history intact. A root whose state can't be told now (a non-unix
  system, or the check failed) is ignored.
* A scan with unreadable sub-folders never marks anything gone.

Known limits of the mount check (accepted, not planned to change):

* **Only the root folder itself is checked.** A root that *contains* mounts
  (say `~/anime` with each drive mounted at `~/anime/<drive>`) is a plain
  folder; if one of those drives is unmounted, its now-empty mount point
  looks like an emptied folder and its files are marked gone (only listing
  state, not history; the next scan with it mounted restores them). Make each
  drive its own root to be safe.
* **Bind mounts** of the same filesystem are not detected as mount points, so
  a bind-mounted root behaves like a plain folder.
* **Non-unix systems** can't tell mount points apart, so there an empty scan
  of a root that had files is always ignored: a root you empty completely
  keeps listing its old files. Files removed while others remain are marked
  gone as usual; removing the root from the config (or renaming it) forgets
  its files.

## Environment

| Variable | Effect |
| --- | --- |
| `ANIPV_CONFIG` | Path of the config file. |
| `ANIPV_HOME` | Put config, data and cache under one directory (`config/`, `data/`, `cache/`). Handy for trying things out: `anipv demo /tmp/x && ANIPV_HOME=/tmp/x anipv`. |
| `NO_COLOR` | Disable colors in CLI output. |
| `XDG_RUNTIME_DIR` | Where mpv IPC sockets are created. |

## Files

| Path | Contents | Safe to delete? |
| --- | --- | --- |
| `~/.config/anipv/config.toml` | configuration | – |
| `events_dir/*.jsonl` | **your watch history** | **no** |
| `~/.local/share/anipv/index.db` | scanned files, metadata cache | yes: `anipv scan` + `anipv meta match` rebuild it |
| `~/.cache/anipv/anime-offline-database-minified.json` | offline anime database | yes: `anipv meta update` |
