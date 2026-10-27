# Follow-ups

Known gaps and ideas not yet worked on. Each says what goes wrong, why it
was left, and what a fix needs.

## Seasons

Since the one season rule (`file_season` in `src/index/classify.rs`), a
file's season comes from its name's marker (`S02E05`), its title's words
(`2nd Season`, `Season 2`, `S2`) or a `Season 2` folder, the same way in
every folder layout. The title's words count only when the title names the
show the folder names, so a folder that names its season its own way
(`Mob Psycho 100 II`, `Lupin III Part 2`) holds that season, not one more.
The corners below are what that rule still gets wrong. Each one needs a
check against a real library (old vs new code over a copy of `index.db`,
every file whose series or episode changes) before it ships.

### A season in a title that runs on (`Season 3 Part 2`)

`Shingeki no Kyojin/Shingeki no Kyojin Season 3 Part 2 - 01.mkv` is keyed
`shingeki no kyojin`, so its episode 01 collides with season 1's. The title
without its season (`shingeki no kyojin part 2`) isn't the folder's show,
so its season words don't count. The same file in an ongoing root is keyed
`shingeki no kyojin s3 part 2`.

Left because a looser match (the folder's name starting the title) would
still drop `Part 2`, merging the season's two parts. That's only half
right, and it changes keys. It needs a decision on what a part is: its own
series (as AniList has it), or a run of episodes of the season.

### A later season by another title (`The Apothecary Diaries S2`)

`Kusuriya no Hitorigoto/The Apothecary Diaries S2 - 01.mkv` is keyed
`kusuriya no hitorigoto`, the same as season 1's
`The Apothecary Diaries - 01.mkv`, so the two collide. The title isn't the
folder's show by name, and nothing in the path says it is the same show in
English. (In a `Season 2` folder it does get season 2.)

Left because only metadata can tell an English title of the same show
from another show. A fix needs AniList: the linked entry's synonyms and
English title, matched against the file's title, would let its season
count.

### A title that is only a season (`[Group] S2 - 01`)

`Yama no Susume/[Group] S2 - 01 - Sleeping in a Tent.mkv` (title `S2`) is
keyed `yama no susume`, colliding with season 1. A bare `S2` can't be
checked against the folder's name.

Left because counting it would key the files in
`Yama no Susume Second Season/` as that season's `s2`, one season too
many: `Second Season` (a season in words) isn't read as a season. A fix
needs season words read in folder names first (`Second Season`, `II`,
`Part 2`?), and then a bare `S2` can count when the folder names no season
of its own.

### An `S02E01` marker in a folder that names its season (`Mob Psycho 100 II`)

`Mob Psycho 100 II/Mob.Psycho.100.S02E01.mkv` is keyed
`mob psycho 100 ii s2`, while `Mob Psycho 100 II/Mob Psycho 100 S2 - 01.mkv`
is `mob psycho 100 ii`: two releases of one season, two series. An
`SxxEyy` marker always counts, unlike a title's season words. This
predates the one season rule.

Left because applying the same-show check to `SxxEyy` markers changes keys
for libraries that rely on them (`Show/S02E05.mkv` has no title to check,
`Gundam/Mobile.Fighter.G.Gundam.S02E01.mkv` a title other than its folder's).
A fix needs the same reading of season names in folders as above, so a
marker counts unless the folder already names that season.

## History following a file whose key changed

When a new version keys a file differently, the history recorded under its
old key has to follow it. Today `classify` works that out: each file
carries one `earlier` key, rebuilt by hand from what an older version did
(an archive file: its series folder's name plus its season; a titled
ongoing file: its title without a `Season N` folder), and `library::moved`
moves events recorded under that key to wherever the file is now.

The cost is that `classify` has to keep a copy of every old keying rule,
one match arm per layout, and each future key change adds another arm that
must copy the old algorithm exactly. A file holds only one earlier key, so
one whose key changed twice keeps only one of its older keys, and history
recorded under the other is left behind.

A more general fix is to do the matching in `library::moved` and drop
`earlier`. For an item event whose `(key, file)` no longer gives any item,
look up the indexed files of that name, and move the event to one when the
recorded key and the file's key name the same show (equal once their
season words and a trailing year are dropped, as `bare_key` in
`src/index/classify.rs` compares them), or when the recorded key is one
the file's folders could give. Then `classify` doesn't need to know how
older versions keyed files. Before shipping it needs the same old-vs-new
check against a real library, comparing every item's watch state, since
it touches every event in the log rather than only files whose key changed.

## `anipv doctor`: a file's embedded title

Matroska files can carry a title of their own (`ffprobe` shows
`TAG:title`). In a sample of 40 files, 6 had one: some repeat the file
name, others are an episode's name, a disc menu's, or another release's
(`Idol Land PriPara 2: Amari's Overwhelming Promise!`, whose `2` is the
episode, not a season). So it must never decide a file's series or season.

It could help `doctor` when a file name says too little: showing the
embedded title next to a name the parser could not make sense of gives a
hint to rename the file or merge the series. Reading it means opening each
such file (slow on a network share), so only for the files `doctor`
already lists, not during a scan.
