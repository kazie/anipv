# Key bindings

<!-- Generated from `HELP` in src/tui/ui.rs by tests/docs.rs; do not edit by hand. -->

Press `?` in the TUI to see these at any time.

## Everywhere

| Key | Action |
| --- | --- |
| `1-5 / Tab` | switch view; again: other mode / next filter |
| `j/k ↑/↓` | move |
| `g/G` | top / bottom |
| `p` | play queue (or the next episode here), resuming |
| `P` | the same, from the beginning |
| `y` | copy mpv command for the queue |
| `v` | while mpv plays: remote mode, keys go to mpv |
| `s` | set series status (in the Inbox: skip) |
| `r` | rescan media folders |
| `M` | refresh metadata for the series under the cursor |
| `o` | open the series under the cursor on AniList |
| `ctrl-r` | refresh all metadata (and an old anime DB) |
| `?` | this help |
| `q / ctrl-c` | quit (asks first); ctrl-c closes a popup first |

## Up next

| Key | Action |
| --- | --- |
| `space` | queue next new episode; down when none are left |
| `a` | queue all new episodes |
| `w` | mark next episode watched without playing |
| `i` | include paused series |
| `⏎` | open series |

## Inbox

| Key | Action |
| --- | --- |
| `f` | follow: moves it to Up next |
| `s` | skip: not interested, never started |
| `z` | later: mark it paused |
| `space` | queue its first episode |
| `⏎` | open series |
| `m` | merge with another series (duplicate names) |

## Series

| Key | Action |
| --- | --- |
| `/` | fuzzy search (⏎ keeps it, Esc clears) |
| `f / 3` | next status filter (works with a search) |
| `m` | merge into another series |
| `U` | undo merges into this series |
| `R` | rename |
| `L` | link AniList id |
| `F` | fold franchises to the entry you're at |
| `C` | catch up: this and earlier entries watched |
| `⏎ / space` | open series |

## Episodes

| Key | Action |
| --- | --- |
| `space` | add / remove from queue |
| `⏎` | play this episode (or open a ▲ ▼ season) |
| `P / shift-⏎` | play this episode from the beginning |
| `[ ]` | previous / next season |
| `w` | toggle watched |
| `W` | mark everything up to here watched |
| `C` | catch up: this and earlier entries watched |
| `a` | queue all new |
| `x` | show / hide extras |
| `d` | show / hide episodes not on disk |
| `esc` | back |

## Files

| Key | Action |
| --- | --- |
| `t / 4` | newest files ⇄ folders |
| `space` | queue a file, or a folder's unwatched |
| `w` | toggle watched (a folder: all of it) |
| `⏎ / → / l` | open folder (a file: its series) |
| `← / h / ⌫` | up a folder |
| `/` | search below here (⏎ keeps it, Esc clears) |

## Queue

| Key | Action |
| --- | --- |
| `J/K` | move entry down / up |
| `d` | remove |
| `c` | clear |
| `⏎` | play |
| `shift-⏎` | play from the beginning |

## Remote mode (while mpv plays)

| Key | Action |
| --- | --- |
| `any key` | pressed in mpv, so your input.conf applies |
| `ctrl-c` | keys back to anipv (v: back to mpv) |
