# Key bindings

<!-- Generated from `HELP` in src/tui/ui.rs by tests/docs.rs; do not edit by hand. -->

Press `?` in the TUI to see these at any time.

## Everywhere

| Key | Action |
| --- | --- |
| `1-5 / Tab` | switch view |
| `j/k ↑/↓` | move |
| `g/G` | top / bottom |
| `p` | play queue (or the next episode under the cursor) |
| `y` | copy mpv command for the queue |
| `s` | set series status (in the Inbox: skip) |
| `r` | rescan media folders |
| `M` | refresh metadata for the series under the cursor |
| `ctrl-r` | refresh all metadata now (updates the anime database if old) |
| `?` | this help |
| `q / ctrl-c` | quit (asks first: q, y or ⏎ confirms); ctrl-c closes an open popup first |

## Up next

| Key | Action |
| --- | --- |
| `space` | queue next new episode (repeat for more) |
| `a` | queue all new episodes |
| `w` | mark next episode watched without playing |
| `P` | include paused series |
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
| `/` | fuzzy search |
| `f` | cycle filter (on disk, following, paused, …) |
| `m` | merge into another series |
| `U` | undo merges into this series |
| `R` | rename |
| `L` | link AniList id |
| `⏎` | open series |

## Episodes

| Key | Action |
| --- | --- |
| `space` | add / remove from queue |
| `⏎` | play this episode |
| `w` | toggle watched |
| `W` | mark everything up to here watched |
| `a` | queue all new |
| `x` | show / hide extras |
| `d` | show / hide episodes not on disk |
| `esc` | back |

## Queue

| Key | Action |
| --- | --- |
| `J/K` | move entry down / up |
| `d` | remove |
| `c` | clear |
| `⏎` | play |
