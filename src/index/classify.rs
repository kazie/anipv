//! Assigning a scanned file to a series and item using its path context.

use std::collections::HashMap;
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;

use crate::config::{Config, RootKind};
use crate::identity::{label_key, legacy_name_key, series_key, series_name};
use crate::index::FileRow;
use crate::model::{ItemKey, ItemKind};
use crate::parse::{Parsed, clean_dir_name, parse};

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

/// Classify every row under its configured root.
pub fn classify_rows(cfg: &Config, files: &[FileRow]) -> Vec<FileClass> {
    let kinds: HashMap<&str, RootKind> = cfg.roots.iter().map(|r| (r.name.as_str(), r.kind)).collect();
    files.iter().map(|row| classify_row(cfg, &kinds, row)).collect()
}

/// Classify one row under its root, given the kind of each configured root.
pub fn classify_row(cfg: &Config, kinds: &HashMap<&str, RootKind>, row: &FileRow) -> FileClass {
    let kind = *kinds.get(row.root.as_str())?;
    Some((kind, classify(cfg, kind, &row.rel)))
}

/// Expand `ep..=ep_end` into one key per episode.
pub fn expand_items(p: &Parsed) -> Vec<ItemKey> {
    let first = p.item_key();
    match (p.kind, p.ep, p.ep_end) {
        (ItemKind::Episode, Some(a), Some(b)) if b > a && a.is_whole() => {
            (a.whole()..=b.whole()).map(|n| ItemKey { ep: Some(crate::model::EpNo::new(n)), ..first.clone() }).collect()
        }
        _ => vec![first],
    }
}

/// Classify a file at `rel` (relative to its root).
pub fn classify(cfg: &Config, kind: RootKind, rel: &Path) -> Classified {
    // Parse a lossy copy: a name that isn't valid UTF-8 still has a title and number.
    let owned: Vec<std::borrow::Cow<'_, str>> = rel.iter().map(|c| c.to_string_lossy()).collect();
    let comps: Vec<&str> = owned.iter().map(std::convert::AsRef::as_ref).collect();
    let (dirs, file) = match comps.split_last() {
        Some((f, d)) => (d, *f),
        None => (&[][..], ""),
    };
    let mut parsed = parse(file);
    let stem = crate::parse::stem(file);

    let in_dir = |list: &[String]| dirs.iter().any(|d| list.iter().any(|x| x.eq_ignore_ascii_case(d)));
    if in_dir(&cfg.extras_dirs) && parsed.kind != ItemKind::Extra {
        parsed.kind = ItemKind::Extra;
        parsed.label = label_key(stem);
        if parsed.label.is_empty() {
            parsed.label = stem.to_string();
        }
        parsed.ep = None;
        parsed.ep_end = None;
    } else if in_dir(&cfg.specials_dirs) && parsed.kind == ItemKind::Episode {
        parsed.kind = ItemKind::Special;
        parsed.label = "special".into();
    }

    let ((series, title), legacy_series) = if let (RootKind::Archive, Some(top)) = (kind, dirs.first()) {
        let t = clean_dir_name(top);
        // The folder names the show; a later season inside it (`S02E01` in the
        // file name, or a `Season 2` folder) must not collapse into season 1.
        let season = parsed.season.or_else(|| season_from_dirs(&dirs[1..]));
        let named = series_name(&t, season);
        let legacy = legacy_name_key(&named.0, &t, season);
        (named, legacy)
    } else {
        let mut title = parsed.title.clone();
        let mut season = parsed.season;
        if title.is_empty() {
            // `Batch/01 - Name.mkv`: borrow the nearest named folder. A
            // `Season 2` folder is not a name: it gives the season, and the
            // show's folder above it gives the title.
            if let Some(d) = dirs.iter().rev().find(|d| !in_dir_list(cfg, d) && season_dir(d).is_none()) {
                title = clean_dir_name(d);
                season = season_from_dirs(dirs);
            }
        }
        if title.is_empty() {
            title = file.to_string();
        }
        // `JoJos.Bizarre.Adventure.S06E02` → "JoJos Bizarre Adventure S6".
        let named = series_name(&title, season);
        let legacy = legacy_name_key(&named.0, &title, season);
        (named, legacy)
    };

    // Movies inside a series folder are told apart by their own title.
    if parsed.kind == ItemKind::Movie && parsed.label.is_empty() && parsed.title.is_empty() {
        parsed.title = stem.to_string();
    }

    let (series, legacy_series) = if series.is_empty() {
        let key = series_key(file);
        let legacy = legacy_name_key(&key, file, None);
        (key, legacy)
    } else {
        (series, legacy_series)
    };
    Classified { series, legacy_series, title, parsed }
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

fn in_dir_list(cfg: &Config, d: &str) -> bool {
    cfg.extras_dirs.iter().chain(&cfg.specials_dirs).any(|x| x.eq_ignore_ascii_case(d))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::EpNo;

    fn c(kind: RootKind, rel: &str) -> Classified {
        classify(&Config::default(), kind, Path::new(rel))
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
            "[Ayako, Paca, Paradise] Jewelpet Sunshine 1-52 Batch/[Paca]Jewelpet Sunshine 17-28/[Paca]Jewelpet Sunshine 24.mkv",
        );
        assert_eq!(x.series, "jewelpet sunshine");
        let x = c(RootKind::Archive, "Astro Boy (Tetsuwan Atom) 2003/Astro Boy 01.mkv");
        assert_eq!(x.series, "astro boy 2003");
        assert_eq!(c(RootKind::Archive, "Blue Seed 2/Blue Seed 2 - 01.mkv").series, "blue seed 2");
        assert_eq!(c(RootKind::Archive, "Pocket Monsters (2023)/x 01.mkv").series, "pocket monsters 2023");
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
