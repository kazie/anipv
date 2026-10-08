//! One-time import of `mpv …` commands from fish shell history.
//!
//! fish stores history as YAML-like records:
//!
//! ```text
//! - cmd: mpv \\[GroupA\\]\\ One\\ Piece\\ -\\ 1180.mkv
//!   when: 1790536811
//! ```
//!
//! Each played file is matched against the index (so archive folders give the
//! right series): by its exact path first, then by the longest path suffix that
//! includes its folder (so relative paths, moved roots and symlinked mounts
//! resolve), then by base name. A typed folder that names nothing in the index
//! only falls back to the base name when parsing the typed path gives the same
//! series. Ambiguous names are skipped, never guessed. Files that are not
//! indexed at all are parsed directly.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};

use crate::config::{Config, RootKind};
use crate::events::{Event, EventBody};
use crate::index::classify::classify;
use crate::library::Library;
use crate::model::{EpNo, ItemKey, ItemKind, SeriesStatus};
use crate::parse::is_video;

/// One `mpv` invocation found in history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Played {
    /// Unix time of the command.
    pub when: i64,
    /// File arguments (not options).
    pub files: Vec<String>,
}

/// Undo fish's history escaping (`\\` → `\`, `\n` → newline).
fn unescape_history(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            match it.next() {
                Some('\\') | None => out.push('\\'),
                Some('n') => out.push('\n'),
                Some(o) => {
                    out.push('\\');
                    out.push(o);
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[derive(Debug, PartialEq)]
enum Tok {
    Word { text: String, glob: bool },
    Sep,
}

/// Split a command line using fish quoting rules.
fn tokenize(cmd: &str) -> Vec<Tok> {
    let mut toks = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut glob = false;
    let mut chars = cmd.chars().peekable();
    let flush = |toks: &mut Vec<Tok>, cur: &mut String, in_word: &mut bool, glob: &mut bool| {
        if *in_word {
            toks.push(Tok::Word { text: std::mem::take(cur), glob: *glob });
        }
        *in_word = false;
        *glob = false;
    };
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                in_word = true;
                if let Some(n) = chars.next()
                    && n != '\n'
                {
                    cur.push(n);
                }
            }
            '\'' => {
                in_word = true;
                while let Some(n) = chars.next() {
                    match n {
                        '\'' => break,
                        '\\' if matches!(chars.peek(), Some('\'' | '\\')) => cur.push(chars.next().unwrap_or('\\')),
                        _ => cur.push(n),
                    }
                }
            }
            '"' => {
                in_word = true;
                while let Some(n) = chars.next() {
                    match n {
                        '"' => break,
                        '\\' if matches!(chars.peek(), Some('"' | '\\' | '$')) => {
                            cur.push(chars.next().unwrap_or('\\'));
                        }
                        _ => cur.push(n),
                    }
                }
            }
            ';' | '|' | '&' | '\n' => {
                flush(&mut toks, &mut cur, &mut in_word, &mut glob);
                toks.push(Tok::Sep);
            }
            c if c.is_whitespace() => flush(&mut toks, &mut cur, &mut in_word, &mut glob),
            '*' | '?' => {
                in_word = true;
                glob = true;
                cur.push(c);
            }
            _ => {
                in_word = true;
                cur.push(c);
            }
        }
    }
    flush(&mut toks, &mut cur, &mut in_word, &mut glob);
    toks
}

/// A command that runs the command after it: `(name, flags it may take
/// without a value, flags followed by a value)`.
type Wrapper = (&'static str, &'static [&'static str], &'static [&'static str]);

/// Prefixes accepted before `mpv` (besides `VAR=value` assignments, which
/// are skipped anywhere before it, e.g. `env FOO=1 mpv …`). Any other flag
/// ends the search, so the command is not counted as an mpv play.
const WRAPPERS: &[Wrapper] = &[
    ("command", &[], &[]),
    ("exec", &[], &[]),
    ("nohup", &[], &[]),
    ("env", &[], &[]),
    ("time", &["-p"], &[]),
    ("nice", &[], &["-n"]),
    ("setsid", &[], &[]),
    ("caffeinate", &["-d", "-i", "-m", "-s", "-u"], &[]),
    ("systemd-run", &["--user", "--scope"], &[]),
];

/// Video file arguments of every `mpv` command in `cmd`, also behind the
/// common prefixes such as `nohup mpv …` or `nice -n 5 mpv …`.
pub fn mpv_files(cmd: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut at_start = true;
    let mut in_mpv = false;
    let mut wrapper: Option<&Wrapper> = None;
    let mut skip_value = false;
    for t in tokenize(cmd) {
        match t {
            Tok::Sep => {
                at_start = true;
                in_mpv = false;
                wrapper = None;
                skip_value = false;
            }
            Tok::Word { text, glob } => {
                if at_start {
                    if std::mem::take(&mut skip_value) {
                        continue;
                    }
                    if let Some((_, flags, valued)) = wrapper {
                        if flags.contains(&text.as_str()) {
                            continue;
                        }
                        if valued.contains(&text.as_str()) {
                            skip_value = true;
                            continue;
                        }
                    }
                    // `VAR=value mpv …`
                    if text.contains('=') && !text.starts_with('-') && !text.starts_with('=') {
                        continue;
                    }
                    let name = text.rsplit('/').next().unwrap_or(&text);
                    if let Some(w) = WRAPPERS.iter().find(|w| w.0 == name) {
                        wrapper = Some(w);
                        continue;
                    }
                    in_mpv = name == "mpv";
                    at_start = false;
                    continue;
                }
                if in_mpv && !glob && !text.starts_with('-') && !text.contains("://") {
                    let base = text.rsplit('/').next().unwrap_or(&text);
                    if is_video(base) {
                        out.push(text);
                    }
                }
            }
        }
    }
    out
}

/// Parse fish history text into mpv plays, oldest first.
pub fn parse_history(text: &str) -> Vec<Played> {
    let mut out = Vec::new();
    let mut cmd: Option<String> = None;
    let push = |out: &mut Vec<Played>, cmd: &mut Option<String>, when: i64| {
        if let Some(c) = cmd.take() {
            let files = mpv_files(&unescape_history(&c));
            if !files.is_empty() {
                out.push(Played { when, files });
            }
        }
    };
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("- cmd: ") {
            cmd = Some(rest.to_string());
        } else if let Some(rest) = line.trim_start().strip_prefix("when: ")
            && let Ok(when) = rest.trim().parse()
        {
            push(&mut out, &mut cmd, when);
        }
    }
    out.sort_by_key(|p| p.when);
    out
}

/// Default fish history location.
pub fn default_history_path() -> std::path::PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map_or_else(|| crate::config::expand_tilde(Path::new("~/.local/share")), std::path::PathBuf::from);
    base.join("fish").join("fish_history")
}

/// A watched item derived from history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedWatch {
    /// Series key (canonical).
    pub series: String,
    /// Item.
    pub item: ItemKey,
    /// Last time it was played.
    pub when: i64,
    /// File name as typed.
    pub file: String,
    /// True if the file was found in the index.
    pub matched: bool,
}

/// Suggested status for a series based on history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proposal {
    /// Series key.
    pub series: String,
    /// Display title.
    pub title: String,
    /// Proposed status.
    pub status: SeriesStatus,
    /// Last play.
    pub last: i64,
}

/// Everything an import would do.
#[derive(Debug, Default)]
pub struct ImportPlan {
    /// Items to mark watched.
    pub watches: Vec<ImportedWatch>,
    /// Status suggestions for currently untracked series.
    pub proposals: Vec<Proposal>,
    /// Number of mpv commands considered.
    pub commands: usize,
    /// File names that matched several indexed files and no path told them
    /// apart; skipped rather than guessed.
    pub ambiguous: BTreeSet<String>,
}

impl ImportPlan {
    /// Events for the watches, stamped with their historical times.
    pub fn events(&self, dev: &str) -> Vec<Event> {
        self.watches
            .iter()
            .map(|w| {
                let body =
                    EventBody::Watched { series: w.series.clone(), item: w.item.clone(), file: Some(w.file.clone()) };
                Event::new(w.when, dev, body)
            })
            .collect()
    }
}

/// Days without activity after which a followed series is proposed as paused.
pub const RECENT_DAYS: i64 = 60;

/// `(time, file name)` of every `watched` event already in the log. Imported
/// events keep the original time and file, so this makes importing idempotent:
/// re-running it (even after the index cache was rebuilt) adds nothing, and an
/// item un-watched after an import is not marked watched again.
pub fn already_imported(events: &[Event]) -> HashSet<(i64, String)> {
    events
        .iter()
        .filter_map(|e| match &e.body {
            EventBody::Watched { file: Some(f), .. } => Some((e.ts, f.clone())),
            _ => None,
        })
        .collect()
}

type ByName<'a> = HashMap<&'a str, Vec<&'a crate::library::IndexedFile>>;

/// How a played file relates to the index.
enum Resolution<'a> {
    Found(&'a crate::library::IndexedFile),
    Ambiguous,
    /// Not indexed; with its series and items if [`identify`] already ran.
    Unknown(Option<(String, Vec<ItemKey>)>),
}

/// The single file among `files` (present ones win), or `Ambiguous` if they
/// disagree about the series or items, `Unknown` if there are none.
fn unique<'a>(files: &[&'a crate::library::IndexedFile]) -> Resolution<'a> {
    let present: Vec<_> = files.iter().copied().filter(|f| f.file.present).collect();
    let pool = if present.is_empty() { files } else { &present };
    match pool {
        [] => Resolution::Unknown(None),
        [first, rest @ ..] if rest.iter().all(|f| f.series == first.series && f.items == first.items) => {
            Resolution::Found(first)
        }
        _ => Resolution::Ambiguous,
    }
}

/// The named components of `path` (no root, `.` or `..`).
fn named_parts(path: &Path) -> impl Iterator<Item = Component<'_>> {
    path.components().filter(|c| matches!(c, Component::Normal(_)))
}

/// The typed path with `~` expanded; a relative one keeps only its named
/// components (`./a.mkv` → `a.mkv`).
fn normalize(file: &str) -> PathBuf {
    let path = crate::config::expand_tilde(Path::new(file));
    if path.is_absolute() { path } else { named_parts(&path).collect() }
}

/// Number of trailing path components `a` and `b` share.
fn common_suffix(a: &Path, b: &Path) -> usize {
    a.components()
        .rev()
        .zip(b.components().rev())
        .take_while(|(x, y)| x == y && matches!(x, Component::Normal(_)))
        .count()
}

/// Series and items of a file that is not (or not certainly) in the index.
/// A path under a root is classified as the scan would; any other path is
/// classified as typed, so `Show/01.mkv` borrows the folder name the way a
/// file in a download folder would.
fn identify(cfg: &Config, lib: &Library, path: &Path) -> (String, Vec<ItemKey>) {
    if lib.file(path).is_some() || cfg.roots.iter().any(|r| path.starts_with(r.resolved())) {
        return lib.identify(cfg, path);
    }
    let rel: PathBuf = named_parts(path).collect();
    let c = classify(cfg, RootKind::Ongoing, &rel);
    (lib.resolve(&c.series).to_string(), c.items())
}

/// Match a played file (see [`normalize`]) to the index:
/// 1. an absolute path that is indexed as is;
/// 2. the indexed files with that base name sharing the longest path suffix,
///    if it includes the parent folder (relative paths, moved roots and
///    symlinked mounts);
/// 3. by base name: freely for a bare name, but for a path with folders only
///    among files of the series the typed path itself parses as, so
///    `OtherShow/01.mkv` never lands on the only indexed `ShowA/01.mkv`.
fn resolve<'a>(cfg: &Config, lib: &'a Library, by_name: &ByName<'a>, path: &Path) -> Resolution<'a> {
    if path.is_absolute()
        && let Some(f) = lib.file(path)
    {
        return Resolution::Found(f);
    }
    let named = path.file_name().and_then(|n| n.to_str()).and_then(|n| by_name.get(n)).map_or(&[][..], Vec::as_slice);
    let has_dir = named_parts(path).nth(1).is_some();
    if !has_dir {
        return unique(named);
    }
    let scored: Vec<_> = named.iter().map(|&f| (common_suffix(path, &f.file.path), f)).collect();
    let best = scored.iter().map(|(n, _)| *n).max().unwrap_or(0);
    if best >= 2 {
        let hits: Vec<_> = scored.iter().filter(|(n, _)| *n == best).map(|(_, f)| *f).collect();
        return unique(&hits);
    }
    let (series, items) = identify(cfg, lib, path);
    let same: Vec<_> = named.iter().copied().filter(|f| f.series == series).collect();
    match unique(&same) {
        Resolution::Unknown(_) => Resolution::Unknown(Some((series, items))),
        found => found,
    }
}

/// Build an import plan for plays not yet in the log (see [`already_imported`]).
pub fn plan(cfg: &Config, lib: &Library, plays: &[Played], already: &HashSet<(i64, String)>, now: i64) -> ImportPlan {
    let mut by_name: ByName = HashMap::new();
    for f in &lib.files {
        if let Some(n) = f.file.path.file_name().and_then(|n| n.to_str()) {
            by_name.entry(n).or_default().push(f);
        }
    }

    // Every play informs the status suggestions; only plays not yet in the log
    // become new events.
    let mut latest: HashMap<(String, ItemKey), ImportedWatch> = HashMap::new();
    let mut history: Vec<ImportedWatch> = Vec::new();
    let mut commands = 0;
    let mut ambiguous = BTreeSet::new();
    for p in plays {
        let mut new = false;
        for file in &p.files {
            let base = file.rsplit('/').next().unwrap_or(file);
            let path = normalize(file);
            let (series, items, matched) = match resolve(cfg, lib, &by_name, &path) {
                Resolution::Found(f) => (f.series.clone(), f.items.clone(), true),
                Resolution::Ambiguous => {
                    ambiguous.insert(base.to_string());
                    continue;
                }
                Resolution::Unknown(known) => {
                    let (series, items) = known.unwrap_or_else(|| identify(cfg, lib, &path));
                    (series, items, false)
                }
            };
            new |= !already.contains(&(p.when, base.to_string()));
            for item in items {
                let w = ImportedWatch {
                    series: series.clone(),
                    item: item.clone(),
                    when: p.when,
                    file: base.to_string(),
                    matched,
                };
                latest.insert((series.clone(), item), w.clone());
                history.push(w);
            }
        }
        commands += usize::from(new);
    }
    // Only an item's newest play is written. Pick it among *all* plays, then
    // skip it if it is already in the log: an older play of an item whose
    // newest play was imported earlier must not be written on a re-run.
    let mut watches: Vec<ImportedWatch> =
        latest.into_values().filter(|w| !already.contains(&(w.when, w.file.clone()))).collect();
    watches.sort_by(|a, b| (a.when, &a.series, &a.item).cmp(&(b.when, &b.series, &b.item)));

    // Per series: the last time an episode was played, and the highest one.
    let mut by_series: HashMap<&str, (i64, Option<EpNo>)> = HashMap::new();
    for w in history.iter().filter(|w| w.item.kind == ItemKind::Episode) {
        let (last, max_ep) = by_series.entry(&w.series).or_default();
        *last = (*last).max(w.when);
        *max_ep = (*max_ep).max(w.item.ep);
    }
    let mut proposals: Vec<Proposal> = by_series
        .into_iter()
        .filter_map(|(key, (last, max_watched))| {
            let s = lib.get(key);
            if s.is_some_and(|s| s.status != SeriesStatus::Untracked) {
                return None;
            }
            let title = s.map_or_else(|| crate::library::title_case(key), |s| s.title.clone());
            let more_on_disk =
                s.is_some_and(|s| s.episodes().any(|i| i.present() && i.key.ep > max_watched && !i.state.is_watched()));
            let age = now - last;
            let status = match (age < RECENT_DAYS * 86_400, more_on_disk) {
                (true, _) => SeriesStatus::Following,
                (false, true) if age < 365 * 86_400 => SeriesStatus::Paused,
                _ => return None,
            };
            Some(Proposal { series: key.to_string(), title, status, last })
        })
        .collect();
    proposals.sort_by(|a, b| b.last.cmp(&a.last).then_with(|| a.series.cmp(&b.series)));

    ImportPlan { watches, proposals, commands, ambiguous }
}

/// Read history from `path`.
pub fn read_history(path: &Path) -> Result<Vec<Played>> {
    let text = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(parse_history(&String::from_utf8_lossy(&text)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Root;
    use crate::index::db::FileRow;

    fn index(cfg: &Config, files: &[FileRow]) -> crate::app::Index {
        crate::app::Index::new(cfg, files.to_vec(), crate::app::MetaCache::default())
    }

    #[test]
    fn tokenizer_handles_fish_quoting() {
        assert_eq!(
            mpv_files(r"mpv \[GroupA\]\ One\ Piece\ -\ 1180\ \(1080p\).mkv"),
            vec!["[GroupA] One Piece - 1180 (1080p).mkv"]
        );
        assert_eq!(mpv_files("mpv '[GroupA] One Piece - 1173.mkv'"), vec!["[GroupA] One Piece - 1173.mkv"]);
        assert_eq!(
            mpv_files(r#"mpv "JoJo's Bizarre Adventure - 01.mkv" b.mkv --fs"#),
            vec!["JoJo's Bizarre Adventure - 01.mkv", "b.mkv"]
        );
        assert_eq!(mpv_files("mpv 1173"), Vec::<String>::new());
        assert_eq!(mpv_files("mpv *.mkv"), Vec::<String>::new());
        assert_eq!(mpv_files("mpv https://youtu.be/x.mp4"), Vec::<String>::new());
        assert_eq!(mpv_files("mpv song.flac"), Vec::<String>::new());
        assert_eq!(mpv_files("cd x; mpv a.mkv && echo b.mkv"), vec!["a.mkv"]);
        assert_eq!(mpv_files("/usr/bin/mpv dir/a.mkv"), vec!["dir/a.mkv"]);
        assert_eq!(mpv_files("vlc a.mkv"), Vec::<String>::new());
        assert_eq!(mpv_files("ENABLE_HDR_WSI=1 mpv --vo=gpu-next a.mkv"), vec!["a.mkv"]);
    }

    #[test]
    fn wrapped_mpv_commands_count() {
        let a = vec!["a.mkv".to_string()];
        for cmd in [
            "command mpv a.mkv",
            "exec mpv a.mkv",
            "nohup mpv a.mkv",
            "env mpv a.mkv",
            "env FOO=1 BAR=2 mpv a.mkv",
            "/usr/bin/env FOO=1 mpv a.mkv",
            "time mpv a.mkv",
            "time -p mpv a.mkv",
            "nice mpv a.mkv",
            "nice -n 10 mpv a.mkv",
            "setsid mpv a.mkv",
            "caffeinate -d mpv a.mkv",
            "systemd-run --user --scope mpv a.mkv",
            "FOO=1 nohup nice -n 5 mpv a.mkv",
            "cd x; nohup mpv a.mkv",
        ] {
            assert_eq!(mpv_files(cmd), a, "{cmd}");
        }
        for cmd in [
            "nohup vlc a.mkv",
            "env -u X mpv a.mkv",
            "nice -5 mpv a.mkv",
            "command -v mpv a.mkv",
            "echo mpv a.mkv",
            "nice -n mpv a.mkv",
        ] {
            assert_eq!(mpv_files(cmd), Vec::<String>::new(), "{cmd}");
        }
    }

    #[test]
    fn history_parsing() {
        let text = r"- cmd: ls
  when: 1
- cmd: mpv \\[GroupA\\]\\ Grand\\ Blue\\ S3\\ -\\ 06\\ \\(1080p\\)\\ \\[ABCD0006\\].mkv Chainsmoker.Cat.S01E07.1080p.WEB.AAC2.0.H.264-GRP.mkv
  when: 30
  paths:
    - x
- cmd: mpv '[GroupA] One Piece - 1180 (1080p) [ABCD1180].mkv'
  when: 20
";
        let plays = parse_history(text);
        assert_eq!(plays.len(), 2);
        assert_eq!(plays[0].when, 20);
        assert_eq!(plays[1].files[0], "[GroupA] Grand Blue S3 - 06 (1080p) [ABCD0006].mkv");
        assert_eq!(plays[1].files.len(), 2);
    }

    #[test]
    fn plan_matches_index_and_proposes() {
        let cfg = cfg();
        let row = |rel: &str| FileRow::test("anime", rel);
        let files = vec![
            row("Futari wa Precure/05 - Title.mkv"),
            row("Futari wa Precure/06 - Next.mkv"),
            row("Old Show/Old Show - 01.mkv"),
            row("Old Show/Old Show - 02.mkv"),
        ];
        let lib = Library::build(&cfg, &[], &index(&cfg, &files));
        let day = 86_400;
        let now = 1000 * day;
        let plays = vec![
            Played { when: now - 200 * day, files: vec!["Old Show - 01.mkv".into()] },
            Played { when: now - day, files: vec!["05 - Title.mkv".into(), "[X] Deleted Show - 03.mkv".into()] },
        ];
        let p = plan(&cfg, &lib, &plays, &HashSet::new(), now);
        assert_eq!(p.commands, 2);
        assert_eq!(p.watches.len(), 3);
        let fw = p.watches.iter().find(|w| w.series == "futari wa precure").unwrap();
        assert!(fw.matched);
        assert_eq!(fw.item, ItemKey::episode(EpNo::new(5)));
        let del = p.watches.iter().find(|w| w.series == "deleted show").unwrap();
        assert!(!del.matched);

        let status = |k: &str| p.proposals.iter().find(|x| x.series == k).map(|x| x.status);
        assert_eq!(status("futari wa precure"), Some(SeriesStatus::Following));
        assert_eq!(status("old show"), Some(SeriesStatus::Paused));
        assert_eq!(status("deleted show"), Some(SeriesStatus::Following));

        let evs = p.events("desk");
        assert_eq!(evs.len(), 3);
        assert!(evs.iter().all(|e| e.dev == "desk"));
        // Plays already in the log are skipped, so importing twice adds nothing.
        let again = plan(&cfg, &lib, &plays, &already_imported(&evs), now);
        assert!(again.watches.is_empty());
        assert_eq!(again.commands, 0);
        assert_eq!(again.proposals, p.proposals, "suggestions still come from the whole history");
    }

    /// An episode played twice is written once, and a re-run adds nothing:
    /// the older play must not be imported after the newer one was.
    #[test]
    fn replayed_episodes_import_once() {
        let cfg = cfg();
        let lib = Library::build(&cfg, &[], &index(&cfg, &[FileRow::test("anime", "Show/Show - 01.mkv")]));
        let plays = vec![
            Played { when: 100, files: vec!["Show - 01.mkv".into()] },
            Played { when: 200, files: vec!["Show - 01.mkv".into()] },
        ];
        let first = plan(&cfg, &lib, &plays, &HashSet::new(), 300);
        assert_eq!(first.watches.iter().map(|w| w.when).collect::<Vec<_>>(), [200]);
        let again = plan(&cfg, &lib, &plays, &already_imported(&first.events("desk")), 300);
        assert!(again.watches.is_empty(), "{:?}", again.watches);
    }

    /// A deleted file under a name that was merged into another series counts
    /// for that series, and doesn't get a status suggestion of its own that
    /// would overwrite the real one's.
    #[test]
    fn merged_away_names_resolve_to_their_series() {
        use crate::events::{Event, EventBody};
        let cfg = cfg();
        let files = vec![FileRow::test("anime", "Seitokai ni mo Ana wa Aru/01.mkv")];
        let events = [
            Event::new(
                1,
                "d",
                EventBody::Alias { from: "student council".into(), to: "seitokai ni mo ana wa aru".into() },
            ),
            Event::new(2, "d", EventBody::status("seitokai ni mo ana wa aru", SeriesStatus::Completed, None)),
        ];
        let lib = Library::build(&cfg, &events, &index(&cfg, &files));
        let plays = vec![Played { when: 100, files: vec!["[X] Student Council - 03.mkv".into()] }];
        let p = plan(&cfg, &lib, &plays, &HashSet::new(), 200);
        assert_eq!(p.watches[0].series, "seitokai ni mo ana wa aru");
        assert!(p.proposals.is_empty(), "{:?}", p.proposals);
    }

    fn two_series() -> (Config, Library) {
        one_series(&["ShowA/01.mkv", "ShowB/01.mkv", "ShowB/02.mkv", "ShowC/Unique - 07.mkv"])
    }

    fn play(when: i64, file: &str) -> Played {
        Played { when, files: vec![file.into()] }
    }

    /// The plan for `plays` (none imported yet) against a test library.
    fn plan_of((cfg, lib): &(Config, Library), plays: &[Played]) -> ImportPlan {
        plan(cfg, lib, plays, &HashSet::new(), 1_000)
    }

    /// The same base name in two series is told apart by the path typed.
    #[test]
    fn full_path_picks_the_series() {
        let plays = [play(100, "/anime/ShowA/01.mkv"), play(200, "ShowB/01.mkv"), play(300, "./ShowB/02.mkv")];
        let p = plan_of(&two_series(), &plays);
        assert!(p.ambiguous.is_empty());
        assert_eq!(got(&p), [("showa", Some(10), true), ("showb", Some(10), true), ("showb", Some(20), true)]);
    }

    /// With no path to go by, a name shared by two series is skipped and counted.
    #[test]
    fn ambiguous_names_are_skipped() {
        let p = plan_of(&two_series(), &[play(100, "01.mkv"), play(300, "Unique - 07.mkv")]);
        assert_eq!(p.ambiguous.iter().collect::<Vec<_>>(), ["01.mkv"]);
        assert_eq!(got(&p), [("showc", Some(70), true)]);
        assert_eq!(p.commands, 1, "skipped plays are not commands to import");
    }

    /// One archive root, `anime` at `/anime`.
    fn cfg() -> Config {
        Config { roots: vec![Root::test("anime", "/anime", RootKind::Archive)], ..Config::default() }
    }

    fn one_series(files: &[&str]) -> (Config, Library) {
        let cfg = cfg();
        let rows: Vec<_> = files.iter().map(|f| FileRow::test("anime", f)).collect();
        let lib = Library::build(&cfg, &[], &index(&cfg, &rows));
        (cfg, lib)
    }

    fn got(p: &ImportPlan) -> Vec<(&str, Option<u32>, bool)> {
        p.watches.iter().map(|w| (w.series.as_str(), w.item.ep.map(EpNo::tenths), w.matched)).collect()
    }

    /// A typed folder that names another show doesn't fall back to the only
    /// indexed file with that base name: the play is parsed on its own.
    #[test]
    fn other_folder_is_not_matched_by_base_name() {
        let plays = [play(100, "OtherShow/01.mkv"), play(200, "/elsewhere/OtherShow/01.mkv")];
        let p = plan_of(&one_series(&["ShowA/01.mkv"]), &plays);
        assert!(p.ambiguous.is_empty());
        assert_eq!(got(&p), [("othershow", Some(10), false)]);
    }

    /// A moved root (or a symlinked mount) still matches through the shared
    /// show folder, even when the base name alone is ambiguous.
    #[test]
    fn moved_root_and_symlinked_mount_match_by_suffix() {
        let plays = [
            play(100, "/old/disk/anime/ShowA/01.mkv"),
            play(200, "/mnt/nas-link/ShowB/01.mkv"),
            play(300, "~/media/ShowB/02.mkv"),
        ];
        let p = plan_of(&two_series(), &plays);
        assert!(p.ambiguous.is_empty(), "{:?}", p.ambiguous);
        assert_eq!(got(&p), [("showa", Some(10), true), ("showb", Some(10), true), ("showb", Some(20), true)]);
    }

    /// A file played from another folder matches by base name when its own
    /// name parses as the same series.
    #[test]
    fn other_folder_matches_when_the_name_agrees() {
        let p = plan_of(&one_series(&["Show/[X] Show - 05.mkv"]), &[play(100, "/home/me/Downloads/[X] Show - 05.mkv")]);
        assert_eq!(got(&p), [("show", Some(50), true)]);
    }

    /// An unindexed relative path with folders is parsed as typed, so the
    /// folder names the show.
    #[test]
    fn unindexed_relative_path_uses_its_folder() {
        let p = plan_of(&one_series(&["ShowA/01.mkv"]), &[play(100, "./Some Show/03.mkv")]);
        assert_eq!(got(&p), [("some show", Some(30), false)]);
    }

    /// A base name only one indexed file has still matches.
    #[test]
    fn unique_base_name_matches() {
        let p = plan_of(&two_series(), &[play(100, "Unique - 07.mkv"), play(200, "02.mkv")]);
        assert!(p.ambiguous.is_empty());
        assert_eq!(got(&p), [("showc", Some(70), true), ("showb", Some(20), true)]);
    }
}
