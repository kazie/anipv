//! Assigning a scanned file to a series and item using its path context.

use std::collections::HashMap;
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;

use crate::config::{Config, RootKind};
use crate::identity::{legacy_name_key, series_key, series_name};
use crate::index::FileRow;
use crate::model::{ItemKey, ItemKind};
use crate::parse::{Parsed, clean_dir_name, extra_label, parse};

/// A file's place in the library.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classified {
    /// Series key (before alias resolution).
    pub series: String,
    /// The key this file had before kana voiced marks were kept in keys (see
    /// [`crate::identity::legacy_series_key`]), when it differs from `series`
    /// (it rarely does).
    pub legacy_series: Option<String>,
    /// Human title candidate for the series.
    pub title: String,
    /// Parse of the file name with folder context applied.
    pub parsed: Parsed,
    /// The numbered entry folder of a franchise folder this file sits in.
    pub entry: Option<Entry>,
    /// The key an older version gave the file, when it differs: an archive
    /// file's series folder's (before entry folders, folders split off a
    /// series and seasons by title counted), an ongoing file's title alone
    /// (before `Season N` folders counted for it). History recorded under it
    /// follows the file (see `library::moved`).
    pub earlier: Option<String>,
}

impl Classified {
    /// Items this file provides (multi-episode files provide several).
    pub fn items(&self) -> Vec<ItemKey> {
        expand_items(&self.parsed)
    }
}

/// One file's classification and the kind of root it is in (`None` for a
/// file whose root is no longer configured).
pub type FileClass = Option<(RootKind, Classified)>;

/// Rows per thread below which [`classify_rows`] does not spread the work.
const CHUNK_MIN: usize = 1024;

/// Most threads [`classify_rows`] uses.
const MAX_THREADS: usize = 8;

/// Classify every row under its configured root, taking a row's
/// classification from `known` where it has one (made under the same `cfg`).
///
/// Parsing every file name is most of what loading the library costs, so a
/// large index is split over a few threads.
pub fn classify_rows<'a>(
    cfg: &Config,
    files: &[FileRow],
    known: impl Fn(&FileRow) -> Option<&'a FileClass> + Sync,
) -> Vec<FileClass> {
    let kinds = cfg.root_kinds();
    let classify = |rows: &[FileRow]| -> Vec<FileClass> {
        rows.iter().map(|row| known(row).cloned().unwrap_or_else(|| classify_row(cfg, &kinds, row))).collect()
    };
    let threads = std::thread::available_parallelism().map_or(1, usize::from).min(MAX_THREADS);
    let chunk = files.len().div_ceil(threads).max(CHUNK_MIN);
    if files.len() <= chunk {
        return classify(files);
    }
    std::thread::scope(|s| {
        #[expect(clippy::needless_collect, reason = "every thread starts before the first is joined")]
        let parts: Vec<_> = files.chunks(chunk).map(|rows| s.spawn(move || classify(rows))).collect();
        parts.into_iter().flat_map(|p| p.join().unwrap_or_else(|e| std::panic::resume_unwind(e))).collect()
    })
}

/// Classify one row under its root, given the kind of each configured root.
fn classify_row(cfg: &Config, kinds: &HashMap<&str, RootKind>, row: &FileRow) -> FileClass {
    let kind = *kinds.get(row.root.as_str())?;
    Some((kind, classify(cfg, kind, &row.rel)))
}

/// Expand `ep..=ep_end` into one key per episode (or special).
pub fn expand_items(p: &Parsed) -> Vec<ItemKey> {
    let first = p.item_key();
    match (p.kind, p.ep, p.ep_end) {
        (ItemKind::Episode | ItemKind::Special, Some(a), Some(b)) if b > a && a.is_whole() => {
            (a.whole()..=b.whole()).map(|n| ItemKey { ep: Some(crate::model::EpNo::new(n)), ..first.clone() }).collect()
        }
        _ => vec![first],
    }
}

/// Classify a file at `rel` (relative to its root).
pub fn classify(cfg: &Config, kind: RootKind, rel: &Path) -> Classified {
    classify_split(cfg, kind, rel, None)
}

/// [`classify`] for a file in a folder split off its series (see
/// [`crate::events::EventBody::Split`]): its first `split` folders hold it,
/// and it is named as a franchise entry is, by its own title, else that
/// folder's name.
pub fn classify_split(cfg: &Config, kind: RootKind, rel: &Path, split: Option<usize>) -> Classified {
    // Parse a lossy copy: a name that isn't valid UTF-8 still has a title and number.
    let owned: Vec<std::borrow::Cow<'_, str>> = rel.iter().map(|c| c.to_string_lossy()).collect();
    let comps: Vec<&str> = owned.iter().map(AsRef::as_ref).collect();
    let (dirs, file) = match comps.split_last() {
        Some((f, d)) => (d, *f),
        None => (&[][..], ""),
    };
    let mut parsed = parse(file);
    let stem = crate::parse::stem(file);
    // An archive file's series folder, cleaned.
    let top = match (kind, dirs.first()) {
        (RootKind::Archive, Some(top)) => Some(clean_dir_name(top)),
        _ => None,
    };
    let mut entry = None;
    // The name the series is called by, and the folders below that name
    // (whose `Season N` folders count). Only the name differs by layout: the
    // season comes from [`file_season`], the same way for every file.
    let (name, below) = if let Some(n) = split
        && let Some(dir) = n.checked_sub(1).and_then(|i| dirs.get(i))
    {
        (entry_name(&parsed, dir), &dirs[n..])
    } else if let (Some(top), Some((no, name))) = (&top, dirs.get(1).and_then(|d| entry_dir(d))) {
        // `Franchise/03 - Entry/…`: each numbered entry is a series of its own.
        entry = Some(Entry { franchise: top.clone(), no, name: name.to_string() });
        (entry_name(&parsed, name), &dirs[2..])
    } else if let Some(top) = &top {
        // The folder names the show. A named folder inside it may hold a
        // show of its own.
        (sub_series(cfg, top, &dirs[1..], &parsed).unwrap_or_else(|| top.clone()), &dirs[1..])
    } else if parsed.title.is_empty() {
        // `Batch/01 - Name.mkv`: borrow the nearest named folder (a `Season 2`
        // folder is not a name).
        let named = dirs.iter().rev().find(|d| !is_generic_dir(cfg, d));
        (named.map_or_else(|| file.to_string(), |d| clean_dir_name(d)), dirs)
    } else {
        (parsed.title.clone(), dirs)
    };
    let season = file_season(&parsed, &name, below);
    // `JoJos.Bizarre.Adventure.S06E02` → "JoJos Bizarre Adventure S6".
    let (mut series, title) = series_name(&name, season);
    let mut legacy_series = legacy_name_key(&series, &name, season);
    if series.is_empty() {
        series = series_key(file);
        legacy_series = legacy_name_key(&series, file, None);
    }
    // The key an older version gave the file, for its history to follow it;
    // only worked out when its name or season differs from today's.
    let earlier = match (&top, dirs) {
        (Some(top), [_, inner @ ..]) => {
            let then = parsed.season.or_else(|| season_from_dirs(inner));
            (name != *top || then != season).then(|| series_name(top, then).0)
        }
        // A titled ongoing file took no season from its folders.
        (None, [_, ..]) if kind == RootKind::Ongoing && !parsed.title.is_empty() => {
            (name != parsed.title || parsed.season != season).then(|| series_name(&parsed.title, parsed.season).0)
        }
        _ => None,
    }
    .filter(|k| !k.is_empty() && *k != series);

    let in_dir = |is: fn(&Config, &str) -> bool| dirs.iter().any(|d| is(cfg, d));
    if in_dir(Config::is_extras_dir) && parsed.kind != ItemKind::Extra {
        parsed.kind = ItemKind::Extra;
        parsed.label = extra_label(stem);
        // `Show/Extras/Show - Fanart.mkv` → `fanart`: the series names the show already.
        if let Some(rest) = parsed.label.strip_prefix(series.as_str()).and_then(|r| r.strip_prefix(' '))
            && rest.chars().count() >= 3
        {
            parsed.label = rest.to_string();
        }
        parsed.ep = None;
        parsed.ep_end = None;
    } else if parsed.kind == ItemKind::Episode && (in_dir(Config::is_specials_dir) || season == Some(0)) {
        // A `Season 0` / `S00` folder holds specials (like an `S00E05` name),
        // unless the file name says another season.
        parsed.make_special();
    }

    // Movies inside a series folder are told apart by their own title.
    if parsed.kind == ItemKind::Movie && parsed.label.is_empty() && parsed.title.is_empty() {
        parsed.title = stem.to_string();
    }
    Classified { series, legacy_series, title, parsed, entry, earlier }
}

/// The name of the series of a file in a named folder (not a season or kind
/// folder) inside the series folder `top`, when it isn't simply `top`'s:
/// - a later season of `top` by its own title (or the folder's name):
///   `Kusuriya no Hitorigoto/Kusuriya no Hitorigoto 2nd Season (BD) [Group]/
///   Kusuriya no Hitorigoto 2nd Season - 01.mkv` is season 2;
/// - another show, when the folder is named after it too:
///   `Kusuriya no Hitorigoto/MaomaoNoHitorigoto/S2/Maomao no Hitorigoto -
///   S02E01.mkv` is Maomao no Hitorigoto's season 2.
///
/// A release in a folder of its own (an English title of the same show, a
/// batch, omake) stays with `top`.
fn sub_series(cfg: &Config, top: &str, inner: &[&str], parsed: &Parsed) -> Option<String> {
    let at = inner.iter().position(|d| !is_generic_dir(cfg, d))?;
    let sub = clean_dir_name(inner[at]);
    let title = if parsed.title.is_empty() { sub.clone() } else { parsed.title.clone() };
    let top_key = series_key(top);
    // The title's own season words are in its key already.
    let key = series_name(&title, parsed.season.or_else(|| season_from_dirs(inner))).0;
    // A later season of `top` by its own title (or the folder's name).
    if crate::identity::season_suffix(&key).is_some_and(|(base, n)| base == top_key && n >= 2) {
        return Some(title);
    }
    let compact = |s: &str| s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect::<String>();
    // Named alike: one name starts with the other (a special's title runs on
    // past the show's name), and that takes more than a word or two.
    // Only a show folder of its own: one holding folders (`S1`, `S2`,
    // `USJ Special`), with files titled like it. A flat folder of one
    // release (`[Group] Mobile Fighter G Gundam COMPLETE`, an English title
    // of the same show) stays with the series folder's show.
    let nested = inner.len() > at + 1;
    let (sub, base) = (compact(&sub), compact(crate::identity::base_key(&key)));
    let alike = sub.len().min(base.len()) >= 8 && (sub.starts_with(&base) || base.starts_with(&sub));
    // And a show of the franchise, sharing a word of its name (Maomao no
    // Hitorigoto beside Kusuriya no Hitorigoto), not the same show by its
    // English title (The Apothecary Diaries), which shares none.
    let shares = key.split(' ').any(|w| w.chars().count() >= 3 && top_key.split(' ').any(|t| t == w));
    if !nested || parsed.title.is_empty() || !alike || !shares || key.starts_with(&top_key) {
        return None;
    }
    // A title running on past the folder's name (`Maomao no Hitorigoto -
    // USJ Special Part 1`): the show is the words that spell the name.
    let words: Vec<&str> = title.split_whitespace().collect();
    let named = (1..=words.len()).map(|n| words[..n].join(" ")).find(|w| compact(w) == sub);
    Some(named.unwrap_or(title))
}

/// The season of a parsed file, the same way for every layout: its name's
/// marker (`S02E05`, `2x05`), else its title's words (`2nd Season`, `Season
/// 2`, `S2`), else the innermost `Season N` folder of `dirs` (the folders
/// below the one that names the series).
///
/// The title's words count only when the title is the show `name` names:
/// `Kusuriya no Hitorigoto/Kusuriya no Hitorigoto 2nd Season - 01.mkv` is
/// season 2, but in `Mob Psycho 100 II/Mob Psycho 100 S2 - 01.mkv` the
/// folder names that season already, its own way.
fn file_season(parsed: &Parsed, name: &str, dirs: &[&str]) -> Option<u32> {
    let titled = || {
        // Every season marker has a digit: most titles skip folding.
        if !parsed.title.contains(char::is_numeric) {
            return None;
        }
        let key = series_key(&parsed.title);
        let n = crate::identity::key_season(&key)?;
        (name == parsed.title || bare_key(&key) == bare_key(&series_key(name))).then_some(n)
    };
    parsed.season.or_else(titled).or_else(|| season_from_dirs(dirs))
}

/// A series key without its season words (`s2`) and a trailing year (`Show
/// (2023)` is the show `Show 2nd Season` names).
fn bare_key(key: &str) -> String {
    let bare = key.split(' ').filter(|w| crate::identity::season_word(w).is_none()).collect::<Vec<_>>().join(" ");
    crate::identity::base_key(&bare).to_string()
}

/// A numbered entry folder (`Monogatari Series/03 - Nisemonogatari`, see
/// [`entry_dir`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The franchise folder's cleaned name (`Monogatari Series`).
    pub franchise: String,
    /// The entry's number (`3`).
    pub no: u32,
    /// The entry folder's name without its number (`Nisemonogatari`).
    pub name: String,
}

/// The number and name of a franchise entry folder (`03 - Nisemonogatari`).
///
/// An archive series folder whose subfolders are numbered this way holds
/// several anime (a franchise in watch order), not one. The spaces around the
/// dash, and a name that starts with something other than a digit and has a
/// letter, keep batch folders (`01-12`, `01 - 12 [BD 1080p]`) out.
pub fn entry_dir(name: &str) -> Option<(u32, &str)> {
    static ENTRY_DIR: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^(\d{1,3})\s+-\s+(\D.*\p{L}.*)$").expect("valid regex"));
    let c = ENTRY_DIR.captures(name.trim())?;
    Some((c[1].parse().ok()?, c.get(2)?.as_str()))
}

/// Name of a file in its own folder `dir` (a franchise entry, or a folder
/// split off its series): the file's own title, as in an ongoing root (an
/// entry may hold part of a longer anime, `Second Season - 06`), else the
/// folder's name.
fn entry_name(parsed: &Parsed, dir: &str) -> String {
    if parsed.title.is_empty() { clean_dir_name(dir) } else { parsed.title.clone() }
}

/// The season given by the innermost `Season N` folder, if any.
fn season_from_dirs(dirs: &[&str]) -> Option<u32> {
    dirs.iter().rev().find_map(|d| season_dir(d))
}

/// The season a folder name stands for (`Season 2`, `S02`), if it is only that.
fn season_dir(name: &str) -> Option<u32> {
    static SEASON_DIR: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)^(?:season\s*|s)0*(\d{1,2})$").expect("valid regex"));
    SEASON_DIR.captures(name.trim()).and_then(|c| c[1].parse().ok())
}

/// True for a folder that says what kind its files are (extras, specials):
/// not a name to borrow.
fn is_kind_dir(cfg: &Config, d: &str) -> bool {
    cfg.is_extras_dir(d) || cfg.is_specials_dir(d)
}

/// True for a folder name that says nothing about which show is in it: a
/// season (`Season 2`, `S02`) or a kind of file (extras, specials).
pub fn is_generic_dir(cfg: &Config, d: &str) -> bool {
    is_kind_dir(cfg, d) || season_dir(d).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::EpNo;

    fn c(kind: RootKind, rel: &str) -> Classified {
        classify(&Config::default(), kind, Path::new(rel))
    }

    /// A large index is classified on several threads, in order, and known
    /// classifications are taken as they are.
    #[test]
    fn classify_rows_keeps_the_order_across_threads() {
        let cfg = Config { roots: vec![crate::config::Root::test("a", "/a", RootKind::Ongoing)], ..Config::default() };
        let files: Vec<FileRow> =
            (0..CHUNK_MIN * 3 + 7).map(|n| FileRow::test("a", &format!("Show {n} - 01.mkv"))).collect();
        let kinds = cfg.root_kinds();
        let one_by_one: Vec<FileClass> = files.iter().map(|row| classify_row(&cfg, &kinds, row)).collect();
        assert_eq!(classify_rows(&cfg, &files, |_| None), one_by_one);
        let known = Some((RootKind::Archive, c(RootKind::Archive, "Known/Known - 01.mkv")));
        let rows = classify_rows(&cfg, &files, |row| (row.rel == Path::new("Show 5 - 01.mkv")).then_some(&known));
        assert_eq!(rows[5], known);
        assert_eq!(rows[6], one_by_one[6]);
    }

    /// A file without a title of its own keeps its season marker when it
    /// borrows the folder's name (the marker wins over a season folder too).
    #[test]
    fn untitled_files_keep_their_season_marker() {
        for kind in [RootKind::Ongoing, RootKind::Archive] {
            assert_eq!(c(kind, "Show/S02E05.mkv").series, "show s2", "{kind:?}");
            assert_eq!(c(kind, "Show/Season 2/S03E05.mkv").series, "show s3", "{kind:?}");
            assert_eq!(c(kind, "Show/Season 2/05.mkv").series, "show s2", "{kind:?}");
        }
    }

    /// A `Season 0` folder holds specials, like a `Specials` folder: they
    /// don't collide with the episodes of the same numbers.
    #[test]
    fn season_0_folders_hold_specials() {
        for kind in [RootKind::Ongoing, RootKind::Archive] {
            for rel in ["Show/Season 0/01.mkv", "Show/S00/Show - 01.mkv", "Show/Season 00/01.mkv"] {
                let x = c(kind, rel);
                assert_eq!((x.series.as_str(), x.parsed.kind), ("show", ItemKind::Special), "{kind:?} {rel}");
            }
            assert_eq!(c(kind, "Show/01.mkv").parsed.kind, ItemKind::Episode);
            // The file name's season marker wins over the folder's, either way.
            let x = c(kind, "Show/Season 0/Show S02E05.mkv");
            assert_eq!((x.series.as_str(), x.parsed.kind), ("show s2", ItemKind::Episode), "{kind:?}");
            // An `S00E05` is a special of the series its folder gives, as it
            // always was (series keys identify watch history).
            let x = c(kind, "Show/Season 2/S00E05.mkv");
            assert_eq!((x.series.as_str(), x.parsed.kind), ("show s2", ItemKind::Special), "{kind:?}");
        }
        let x = c(RootKind::Archive, "Show/Season 2/Show S00E05.mkv");
        assert_eq!((x.series.as_str(), x.parsed.kind), ("show s2", ItemKind::Special));
    }

    /// A range of specials provides each of them, like a range of episodes.
    #[test]
    fn special_ranges_provide_each_special() {
        let x = c(RootKind::Archive, "Show/Specials/Show - 01-02.mkv");
        assert_eq!(x.parsed.kind, ItemKind::Special);
        let eps: Vec<Option<EpNo>> = x.items().iter().map(|k| k.ep).collect();
        assert_eq!(eps, [Some(EpNo::new(1)), Some(EpNo::new(2))]);
    }

    #[test]
    fn archive_uses_top_folder() {
        let x = c(RootKind::Archive, "One Piece/[GroupA] One Piece - 1180 (1080p) [ABCD1180].mkv");
        assert_eq!(x.series, "one piece");
        assert_eq!(x.items(), vec![ItemKey::episode(EpNo::new(1180))]);
        let x = c(RootKind::Archive, "Futari wa Precure Splash Star/41 - Princess In Peril!.mkv");
        assert_eq!(x.series, "futari wa precure splash star");
        let x = c(RootKind::Archive, "[GroupX] Digimon Tamers (BD, 1080p)/19 - I Want To Be Stronger!.mkv");
        assert_eq!(x.series, "digimon tamers");
        assert_eq!(x.title, "Digimon Tamers");
        let x = c(
            RootKind::Archive,
            "[GroupA, GroupB, GroupC] Jewelpet Sunshine 1-52 Batch/[GroupB]Jewelpet Sunshine 17-28/[GroupB]Jewelpet Sunshine 24.mkv",
        );
        assert_eq!(x.series, "jewelpet sunshine");
        let x = c(RootKind::Archive, "Astro Boy (Tetsuwan Atom) 2003/Astro Boy 01.mkv");
        assert_eq!(x.series, "astro boy 2003");
        assert_eq!(c(RootKind::Archive, "Blue Seed 2/Blue Seed 2 - 01.mkv").series, "blue seed 2");
        assert_eq!(c(RootKind::Archive, "Pocket Monsters (2023)/x 01.mkv").series, "pocket monsters 2023");
    }

    /// Numbered entry folders inside a series folder are anime of their own,
    /// named by their files (several folders can hold one anime).
    #[test]
    fn franchise_entries_are_series_of_their_own() {
        let top = "Monogatari Series (BD 1080p)";
        let x = |rel: &str| c(RootKind::Archive, &format!("{top}/{rel}"));
        let bake = x("01 - Bakemonogatari/[GroupA] Bakemonogatari - 01v2 [ABCD0001].mkv");
        assert_eq!(bake.series, "bakemonogatari");
        let entry = Entry { franchise: "Monogatari Series".into(), no: 1, name: "Bakemonogatari".into() };
        assert_eq!(bake.entry, Some(entry));
        assert_eq!(bake.items(), vec![ItemKey::episode(EpNo::new(1))]);
        for (dir, ep) in [("05 - Nekomonogatari (White)", 1), ("06 - Kabukimonogatari", 6), ("10 - Koimonogatari", 23)]
        {
            let f = x(&format!("{dir}/[GroupA] Monogatari Series Second Season - {ep:02} [ABCD0005].mkv"));
            assert_eq!(f.series, "monogatari series second season", "{dir}");
            assert_eq!(f.items(), vec![ItemKey::episode(EpNo::new(ep))]);
        }
        assert_eq!(
            x("13 - Owarimonogatari S1/[GroupA] Owarimonogatari S1 - 13 [ABCD0013].mkv").series,
            "owarimonogatari"
        );
        let s2 = x("14 - Owarimonogatari S2/[GroupA] Owarimonogatari S2 - 01 [ABCD0014].mkv");
        assert_eq!((s2.series.as_str(), s2.entry.map(|e| e.no)), ("owarimonogatari s2", Some(14)));
        // An untitled file takes the entry folder's name.
        assert_eq!(x("02 - Kizumonogatari/01.mkv").series, "kizumonogatari");
        // Folders that aren't numbered entries keep the top folder's name.
        let jewel = "[GroupA, GroupB, GroupC] Jewelpet Sunshine 1-52 Batch/[GroupB]Jewelpet Sunshine 17-28/[GroupB]Jewelpet Sunshine 24.mkv";
        assert_eq!(c(RootKind::Archive, jewel).series, "jewelpet sunshine");
        let batch = c(RootKind::Archive, "Show/01-12/Other Name - 03.mkv");
        assert_eq!((batch.series.as_str(), batch.entry), ("show", None));
        // Directly in the franchise folder, and in an ongoing root: as before.
        assert_eq!(c(RootKind::Archive, "Show/Show - 01.mkv").entry, None);
        assert_eq!(c(RootKind::Ongoing, "Show/01 - Other/Other - 01.mkv").entry, None);
    }

    #[test]
    fn entry_dirs() {
        assert_eq!(entry_dir("03 - Nisemonogatari"), Some((3, "Nisemonogatari")));
        assert_eq!(entry_dir("16 - Monogatari Series Off & Monster Season S1").map(|e| e.0), Some(16));
        for not in ["01-12", "01 - 12", "01 - 12 [BD 1080p]", "Season 1", "Show 01", "1-3 Batch", "Show - 01"] {
            assert_eq!(entry_dir(not), None, "{not}");
        }
    }

    #[test]
    fn files_know_the_key_they_had_before_voiced_marks_were_kept() {
        let x = c(RootKind::Archive, "バカ/[G] バカ - 01.mkv");
        assert_eq!((x.series.as_str(), x.legacy_series.as_deref()), ("バカ", Some("ハカ")));
        let x = c(RootKind::Ongoing, "[G] バカ S2 - 01.mkv");
        assert_eq!((x.series.as_str(), x.legacy_series.as_deref()), ("バカ s2", Some("ハカ s2")));
        let x = c(RootKind::Archive, "One Piece/One Piece - 1180.mkv");
        assert_eq!(x.legacy_series, None);
        // Item labels are identity too and keep their old form.
        let x = c(RootKind::Archive, "バカ/Extras/バカ Voice Drama.mkv");
        assert_eq!(x.parsed.label, "ハカ voice drama");
    }

    #[test]
    fn extras_and_specials_dirs() {
        let x = c(RootKind::Archive, "Futari wa Precure Max Heart/DVD Extras/Clean Opening B.mkv");
        assert_eq!(x.parsed.kind, ItemKind::Extra);
        let x = c(RootKind::Archive, "Aikatsu Stars!/Extras/Aikatsu Stars! BD-BOX 2-1 (BD 1920x1080 x264 AAC).mp4");
        assert_eq!(x.parsed.kind, ItemKind::Extra);
        // Named without the release tags, and without the show's name.
        let x = c(
            RootKind::Archive,
            "Grand Blue/Extras/[GroupA] Grand Blue (Creditless ED 01) [BDRip 1080p][ABCD0001].mkv",
        );
        assert_eq!((x.parsed.kind, x.parsed.label.as_str()), (ItemKind::Extra, "creditless ed 01"));
        // Unless too little would be left.
        let x = c(RootKind::Archive, "Bartender/Extras/[GroupA] Bartender M [BDRip 1080p].mkv");
        assert_eq!(x.parsed.label, "bartender m");
        let x = c(RootKind::Archive, "Show/Specials/Show - 01.mkv");
        assert_eq!(x.parsed.kind, ItemKind::Special);
        let x = c(
            RootKind::Archive,
            "Meitantei Precure!/[GroupB] Meitantei Precure! - 1-12 Batch (1080p)/Extras/[GroupB] Meitantei Precure! - 06v2 Fanart Corner (1080p).mkv",
        );
        assert_eq!(x.parsed.kind, ItemKind::Extra);
        assert_eq!(x.series, "meitantei precure");
    }

    #[test]
    fn ongoing_uses_file_title() {
        let x = c(RootKind::Ongoing, "[GroupA] Grand Blue S3 - 07 (1080p) [ABCD0007].mkv");
        assert_eq!(x.series, "grand blue s3");
        let x = c(RootKind::Ongoing, "BEASTARS.S03.1080p.WEB-GRP/BEASTARS.S03E05.1080p.WEB-GRP.mkv");
        assert_eq!(x.series, "beastars s3");
        assert_eq!(x.title, "BEASTARS S3");
        let x = c(RootKind::Ongoing, "Chainsmoker.Cat.S01E07.1080p.WEB.AAC2.0.H.264-GRP.mkv");
        assert_eq!(x.series, "chainsmoker cat");
        let x = c(RootKind::Ongoing, "[GroupX] Digimon Tamers (BD, 1080p)/19 - I Want To Be Stronger!.mkv");
        assert_eq!(x.series, "digimon tamers");
        // A file at the top of an archive root behaves like an ongoing file.
        let x = c(RootKind::Archive, "[GroupA] One Piece - 1181 (1080p).mkv");
        assert_eq!(x.series, "one piece");
    }

    #[test]
    fn same_series_across_roots() {
        let a = c(RootKind::Ongoing, "[GroupA] One Piece - 1181 (1080p) [AAAAAAAA].mkv");
        let b = c(RootKind::Archive, "One Piece/[GroupA] One Piece - 1181 (1080p) [AAAAAAAA].mkv");
        assert_eq!(a.series, b.series);
        assert_eq!(a.items(), b.items());
    }

    #[test]
    fn season_folders_are_not_series_names() {
        let x = c(RootKind::Ongoing, "Show/Season 2/01 - Title.mkv");
        assert_eq!((x.series.as_str(), x.title.as_str()), ("show s2", "Show S2"));
        let x = c(RootKind::Ongoing, "Show/S01/01 - Title.mkv");
        assert_eq!(x.series, "show");
    }

    #[test]
    fn non_utf8_names_are_classified() {
        use std::os::unix::ffi::OsStrExt;
        let rel = Path::new(std::ffi::OsStr::from_bytes(b"Show \xe9/Show \xe9 - 03.mkv"));
        let x = classify(&Config::default(), RootKind::Archive, rel);
        assert_eq!(x.items(), vec![ItemKey::episode(EpNo::new(3))]);
        assert!(x.series.starts_with("show"));
    }

    #[test]
    fn archive_seasons_inside_a_series_folder_stay_apart() {
        let key = |rel: &str| c(RootKind::Archive, rel).series;
        assert_eq!(key("Show/Show S02E01.mkv"), "show s2");
        assert_eq!(key("Show/Show S01E01.mkv"), "show");
        assert_eq!(key("Show/Season 2/01 - Title.mkv"), "show s2");
        assert_eq!(key("Show/Season 1/01 - Title.mkv"), "show");
        assert_eq!(key("Show S2/Show S2 - S02E01.mkv"), "show s2", "no doubled marker");
    }

    /// A named folder inside a series folder holds a later season of the show
    /// by its own title, or another show named like the folder; anything
    /// else in it stays the folder's show.
    #[test]
    fn named_folders_inside_a_series_folder() {
        let key = |rel: &str| c(RootKind::Archive, rel).series;
        let top = "Kusuriya no Hitorigoto";
        let s2 = "Kusuriya no Hitorigoto 2nd Season (BD 1080p HEVC Opus) [GroupK]";
        assert_eq!(
            key(&format!("{top}/{s2}/Kusuriya no Hitorigoto 2nd Season - 01 (BD 1080p) [GroupK].mkv")),
            "kusuriya no hitorigoto s2"
        );
        assert_eq!(
            key(&format!("{top}/MaomaoNoHitorigoto/S1/[GroupB] Maomao no Hitorigoto - S01E01.mkv")),
            "maomao no hitorigoto"
        );
        assert_eq!(
            key(&format!("{top}/MaomaoNoHitorigoto/S2/[GroupB] Maomao no Hitorigoto - S02E01.mkv")),
            "maomao no hitorigoto s2"
        );
        let usj = c(
            RootKind::Archive,
            &format!("{top}/MaomaoNoHitorigoto/USJ Special/[GroupB] Maomao no Hitorigoto - USJ Special Part 1.mkv"),
        );
        assert_eq!(usj.series, "maomao no hitorigoto");
        // A later season by its own title, straight in the series folder.
        assert_eq!(
            key(&format!("{top}/Kusuriya no Hitorigoto 2nd Season - 23 (BD 1080p HEVC Opus) [GroupK].mkv")),
            "kusuriya no hitorigoto s2"
        );
        assert_eq!(key(&format!("{top}/Kusuriya no Hitorigoto - 23 (1080p).mkv")), "kusuriya no hitorigoto");
        // The show's own files, whatever they are titled, and anything else.
        assert_eq!(
            key(&format!("{top}/[GroupM] The Apothecary Diaries - 01 (BD 1080p) [AB12CD34].mkv")),
            "kusuriya no hitorigoto"
        );
        assert_eq!(
            key(&format!("{top}/Apothecary S2 BD/[GroupE] The Apothecary Diaries S2 - 01.mkv")),
            "kusuriya no hitorigoto",
            "no other show; its title isn't the folder's, so its season words don't count"
        );
        assert_eq!(key(&format!("{top}/Bonus/[GroupE] The Apothecary Diaries - 01.mkv")), "kusuriya no hitorigoto");
        assert_eq!(
            key(&format!("{top}/NC/[GroupR] Kusuriya no Hitorigoto (Creditless ED 01).mkv")),
            "kusuriya no hitorigoto"
        );
        // Release folders of the same show by another title, untitled files.
        let gundam = "Kidou Butouden G Gundam/[GroupJ] Mobile Fighter G Gundam COMPLETE (1994)/Mobile.Fighter.G.Gundam.S01E01.mkv";
        assert_eq!(key(gundam), "kidou butouden g gundam");
        let nadia =
            "Nadia - Secret of Blue Water/Fushigi no Umi no Nadia Omake/[GroupK] Fushigi no Umi no Nadia Omake 01.avi";
        assert_eq!(key(nadia), "nadia secret of blue water");
        assert_eq!(key("Cowboy Bebop/Remastered/01 - Asteroid Blues.mkv"), "cowboy bebop");
        let english = format!("{top}/The Apothecary Diaries/Season 2/[GroupE] The Apothecary Diaries S2 - 01.mkv");
        assert_eq!(key(&english), "kusuriya no hitorigoto s2", "the same show by its English title");
        assert_eq!(key("Bubblegum Crisis/Extras/Promo Videos/Promo Video (Bubblegum crisis).mkv"), "bubblegum crisis");
        // A later season's own folder keeps its title for the series.
        let x =
            c(RootKind::Archive, &format!("{top}/{s2}/Kusuriya no Hitorigoto 2nd Season - 01 (BD 1080p) [GroupK].mkv"));
        assert_eq!(x.title, "Kusuriya no Hitorigoto 2nd Season");
        // History of files whose key changed follows them from the key they had.
        let x = c(RootKind::Archive, &format!("{top}/Kusuriya no Hitorigoto 2nd Season - 23 (BD 1080p) [GroupK].mkv"));
        assert_eq!(x.earlier.as_deref(), Some("kusuriya no hitorigoto"));
        let x = c(RootKind::Ongoing, "Show/Season 2/Show - 05.mkv");
        assert_eq!((x.series.as_str(), x.earlier.as_deref()), ("show s2", Some("show")));
        assert_eq!(c(RootKind::Archive, &format!("{top}/[GroupM] The Apothecary Diaries - 01.mkv")).earlier, None);
        // History of these files follows them from the folder's key.
        let m =
            c(RootKind::Archive, &format!("{top}/MaomaoNoHitorigoto/S1/[GroupB] Maomao no Hitorigoto - S01E01.mkv"));
        assert_eq!(m.earlier.as_deref(), Some("kusuriya no hitorigoto"));
    }

    /// Every way of saying a season, in every layout, gives that season:
    /// which folder names the series must never decide whether the season
    /// counts. Also the guard for layouts and spellings added later.
    #[test]
    fn every_layout_keeps_every_season_spelling() {
        let ordinal = |n: u32| match n {
            2 => "2nd".to_string(),
            3 => "3rd".to_string(),
            n => format!("{n}th"),
        };
        for show in ["Show", "Kusuriya no Hitorigoto", "Re Zero kara Hajimeru Isekai Seikatsu"] {
            for n in [1, 2, 3, 4, 11] {
                // File names that say the season themselves.
                let named = [
                    format!("{show} S{n:02}E05.mkv"),
                    format!("[Grp] {show} - S{n:02}E05 (1080p).mkv"),
                    format!("{dotted}.S{n:02}E05.1080p.WEB.mkv", dotted = show.replace(' ', ".")),
                    format!("{show} {n}x05.mkv"),
                    format!("[Grp] {show} {nth} Season - 05 (BD 1080p) [ABCD1234].mkv", nth = ordinal(n)),
                    format!("{show} Season {n} - 05.mkv"),
                    format!("{under}_Season_{n}_-_05.mkv", under = show.replace(' ', "_")),
                    format!("[Grp] {show} S{n} - 05 [1080p].mkv"),
                ];
                let mut cases: Vec<(RootKind, String)> = Vec::new();
                for f in &named {
                    cases.extend([
                        (RootKind::Ongoing, f.clone()),
                        (RootKind::Ongoing, format!("{show}/{f}")),
                        (RootKind::Ongoing, format!("Downloads/{f}")),
                        (RootKind::Archive, f.clone()),
                        (RootKind::Archive, format!("{show}/{f}")),
                        (RootKind::Archive, format!("{show}/[Grp] {show} (BD 1080p)/{f}")),
                        (RootKind::Archive, format!("{show}/Batch/{f}")),
                        (RootKind::Archive, format!("Franchise/03 - {show}/{f}")),
                    ]);
                }
                // Untitled or plain files in a season folder.
                for f in ["05.mkv", "Episode 05.mkv", &format!("{show} - 05.mkv")] {
                    for dir in [format!("Season {n}"), format!("S{n:02}"), format!("Season {n:02}")] {
                        cases.extend([
                            (RootKind::Ongoing, format!("{show}/{dir}/{f}")),
                            (RootKind::Archive, format!("{show}/{dir}/{f}")),
                            (RootKind::Archive, format!("{show}/[Grp] {show} (BD)/{dir}/{f}")),
                        ]);
                    }
                }
                let base = series_key(show);
                let want = if n == 1 { base } else { format!("{base} s{n}") };
                for (kind, rel) in cases {
                    let x = c(kind, &rel);
                    assert_eq!(x.series, want, "{kind:?} {rel}");
                    assert_eq!(x.items(), vec![ItemKey::episode(EpNo::new(5))], "{kind:?} {rel}");
                }
            }
        }
    }

    /// A series folder that names its season its own way (`II`, `Part 2`,
    /// `2`, `Second Season`, `!!`) holds that season: the files' `S2` is
    /// the same season, not one more.
    #[test]
    fn a_folder_naming_its_season_is_that_season() {
        let key = |rel: &str| c(RootKind::Archive, rel).series;
        assert_eq!(key("Mob Psycho 100 II/[Grp] Mob Psycho 100 S2 - 01 (BD 1080p).mkv"), "mob psycho 100 ii");
        assert_eq!(key("Lupin III Part 2/Lupin III S2 - 001 - The Return of Lupin III [720p].mkv"), "lupin iii part 2");
        assert_eq!(key("Kemono Friends 2/[Grp] Kemono Friends S2 - 01 [1080p].mkv"), "kemono friends 2");
        assert_eq!(
            key("Yama no Susume Second Season/[Grp] S2 - 01 - Sleeping in a Tent [BD].mkv"),
            "yama no susume second season"
        );
        // A year in the folder's name is still the show's name.
        assert_eq!(
            key("Kusuriya no Hitorigoto (2023)/Kusuriya no Hitorigoto 2nd Season - 01.mkv"),
            "kusuriya no hitorigoto 2023 s2"
        );
        // Unless the folder's name is the show's alone: `K-ON!!` is keyed as
        // `K-ON!` is, so its files' season tells the two apart.
        assert_eq!(key("K-ON!!/[Grp] K-ON! S2 - 01 (BD 1080p).mkv"), "k on s2");
        assert_eq!(key("K-ON!/[Grp] K-ON! - 01 (BD 1080p).mkv"), "k on");
    }

    #[test]
    fn season_x_episode_names_give_the_season() {
        for kind in [RootKind::Archive, RootKind::Ongoing] {
            let x = c(kind, "Show/Show 2x05.mkv");
            assert_eq!(x.series, "show s2", "{kind:?}");
            assert_eq!(x.items(), vec![ItemKey::episode(EpNo::new(5))], "{kind:?}");
        }
    }

    #[test]
    fn season_zero_is_specials_of_the_same_series() {
        for kind in [RootKind::Archive, RootKind::Ongoing] {
            for rel in ["Show/Show S00E05.mkv", "Show/Show 0x05.mkv"] {
                let x = c(kind, rel);
                assert_eq!(x.series, "show", "{kind:?} {rel}");
                assert_eq!(
                    x.items(),
                    vec![ItemKey { kind: ItemKind::Special, ep: Some(EpNo::new(5)), label: "special".into() }],
                    "{kind:?} {rel}"
                );
            }
        }
    }

    #[test]
    fn multi_episode_files() {
        let x = c(RootKind::Archive, "Urusei Yatsura/[GroupE] Urusei Yatsura - 001&002 [BD].mkv");
        assert_eq!(x.items(), vec![ItemKey::episode(EpNo::new(1)), ItemKey::episode(EpNo::new(2))]);
    }
}
