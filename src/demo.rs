//! A self-contained demo library (empty files + history) for screenshots,
//! README recordings and UI tests. Never touches the user's real data.

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::app::Ctx;
use crate::config::{Config, Paths, Root, RootKind};
use crate::events::{Event, EventBody, now};
use crate::index::db::SeriesMeta;
use crate::model::{EpNo, ItemKey, SeriesStatus};

const DAY: i64 = 86_400;

fn touch(p: &Path) -> Result<()> {
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(p, b"")?;
    Ok(())
}

/// A demo context in a new temporary directory, which goes away when the
/// returned handle is dropped.
#[cfg(test)]
pub(crate) fn ctx() -> (tempfile::TempDir, Ctx) {
    let dir = tempfile::tempdir().expect("temporary directory");
    let ctx = setup(dir.path(), 0).expect("demo library");
    (dir, ctx)
}

/// Create a demo library under `home` and return a ready context.
/// `ts` is "now" for the generated history (pass `events::now()`).
pub fn setup(home: &Path, ts: i64) -> Result<Ctx> {
    // Never over an existing home: the demo writes a config and event logs.
    if std::fs::read_dir(home).is_ok_and(|mut d| d.next().is_some()) {
        anyhow::bail!("{home} is not empty; the demo needs a new or empty folder", home = home.display());
    }
    let media = home.join("media");
    let dl = media.join("Downloads");
    let anime = media.join("Anime");

    let mut files: Vec<PathBuf> = Vec::new();
    for e in 27..=31 {
        files.push(dl.join(format!("[GroupA] Sousou no Frieren - {e:02} (1080p) [ABCD{e:04}].mkv")));
    }
    for e in 5..=8 {
        files.push(dl.join(format!("[GroupA] Dandadan S2 - {e:02} (1080p) [DDDD{e:04}].mkv")));
    }
    files.push(dl.join("[GroupA] Dandadan S2 - 06v2 (1080p) [DDDE0006].mkv"));
    for e in 1..=2 {
        files.push(dl.join(format!("Ranma.1-2.2024.S03E{e:02}.1080p.WEB.DUAL.AAC2.0.H.264-GRP.mkv")));
    }
    files.push(dl.join("[GroupC] Kusuriya no Hitorigoto S3 - 01 [1080p][Multiple Subtitle].mkv"));
    for e in 1..=3 {
        files.push(dl.join(format!("[GroupB] Meitantei Precure! - {e:02} (1080p).mkv")));
        files.push(dl.join(format!("[GroupB] Meitantei Precure! - {e:02} Fanart Corner (1080p).mkv")));
    }
    files.push(dl.join("Laid-Back Camp The Movie (2022) (BD Remux 1080p AVC TrueHD) [ABCD2022].mkv"));
    // Inbox material: a new season of a followed show, a brand-new show, and
    // a side entry of a followed show.
    files.push(dl.join("[GroupA] Sousou no Frieren S2 - 01 (1080p) [ABCE0001].mkv"));
    for e in 1..=3 {
        files.push(dl.join(format!("[GroupB] Dungeon Meshi - {e:02} (1080p) [ABCF{e:04}].mkv")));
    }
    files.push(dl.join("[GroupA] One Piece Fan Letter (1080p) [ABD00001].mkv"));
    for e in 1170..=1182 {
        files.push(anime.join(format!("One Piece/[GroupA] One Piece - {e} (1080p).mkv")));
    }
    for e in 1..=26 {
        files.push(anime.join(format!("Cowboy Bebop/[GroupD] Cowboy Bebop - {e:02} [BD 1080p].mkv")));
    }
    for e in 1..=13 {
        files.push(anime.join(format!("Aria The Animation/Aria - The Animation - {e:02} [DVD].mkv")));
    }
    files.push(anime.join("Aria The Animation/Extras/NCOP.mkv"));
    for e in 1..=13 {
        files.push(anime.join(format!("K-On!/[GroupE] K-On! - {e:02} [BD][1080p FLAC].mkv")));
    }
    for e in 1..=12 {
        files.push(anime.join(format!("Yuru Camp/[GroupF] Yuru Camp - {e:02} [BD 1080p].mkv")));
    }
    // A franchise folder: numbered entries, each a series of its own.
    for (dir, title, eps) in [
        ("01 - Bakemonogatari", "Bakemonogatari", 15),
        ("02 - Nisemonogatari", "Nisemonogatari", 11),
        ("03 - Nekomonogatari (Black)", "Nekomonogatari (Black)", 4),
    ] {
        for e in 1..=eps {
            files.push(anime.join(format!("Monogatari Series/{dir}/[GroupG] {title} - {e:02} [BD 1080p].mkv")));
        }
    }
    for f in &files {
        touch(f)?;
    }

    let cfg = Config {
        device: Some("desktop".into()),
        mpv: "true".into(),
        anilist: false,
        roots: vec![
            Root { name: "Downloads".into(), path: dl, kind: RootKind::Ongoing },
            Root { name: "Anime".into(), path: anime, kind: RootKind::Archive },
        ],
        ..Config::default()
    };
    let paths = Paths::under(home);
    cfg.save(&paths.config_file)?;
    let mut ctx = Ctx::open(cfg, paths)?;
    ctx.scan(None, &|_, _| {})?;

    let ev = |t: i64, dev: &str, body: EventBody| Event::new(t, dev, body);
    let ep = |n: u32| ItemKey::episode(EpNo::new(n));
    let watched = |s: &str, n: u32| EventBody::watched(s, ep(n));
    let status = |s: &str, st: SeriesStatus, note: Option<&str>| EventBody::status(s, st, note.map(String::from));

    let mut events = vec![
        ev(ts - 60 * DAY, "desktop", status("sousou no frieren", SeriesStatus::Following, None)),
        ev(ts - 50 * DAY, "desktop", status("one piece", SeriesStatus::Following, None)),
        ev(ts - 40 * DAY, "desktop", status("dandadan s2", SeriesStatus::Following, None)),
        ev(ts - 30 * DAY, "laptop", status("ranma 1 2 2024 s3", SeriesStatus::Following, None)),
        ev(ts - 300 * DAY, "desktop", status("cowboy bebop", SeriesStatus::Completed, None)),
        ev(
            ts - 200 * DAY,
            "desktop",
            status("aria the animation", SeriesStatus::Paused, Some("slow, but want to finish")),
        ),
        ev(ts - 400 * DAY, "desktop", status("k on", SeriesStatus::Dropped, Some("not for me"))),
        ev(ts - 20 * DAY, "desktop", status("meitantei precure", SeriesStatus::Following, None)),
        ev(ts - 90 * DAY, "desktop", status("bakemonogatari", SeriesStatus::Completed, None)),
        ev(ts - 25 * DAY, "desktop", status("nisemonogatari", SeriesStatus::Following, None)),
    ];
    for n in 1..=15 {
        events.push(ev(ts - (110 - i64::from(n)) * DAY, "desktop", watched("bakemonogatari", n)));
    }
    for n in 1..=3 {
        events.push(ev(ts - (25 - i64::from(n)) * DAY, "desktop", watched("nisemonogatari", n)));
    }
    for n in 1..=28 {
        events.push(ev(ts - (40 - i64::from(n)) * DAY, "desktop", watched("sousou no frieren", n)));
    }
    for n in 1170..=1180 {
        events.push(ev(ts - (1190 - i64::from(n)) * DAY, "laptop", watched("one piece", n)));
    }
    for n in 1..=5 {
        events.push(ev(ts - (12 - i64::from(n)) * DAY, "desktop", watched("dandadan s2", n)));
    }
    for n in 1..=26 {
        events.push(ev(ts - 320 * DAY + i64::from(n) * DAY, "desktop", watched("cowboy bebop", n)));
    }
    for n in 1..=7 {
        events.push(ev(ts - 220 * DAY + i64::from(n) * DAY, "desktop", watched("aria the animation", n)));
    }
    events.push(ev(ts - 410 * DAY, "desktop", watched("k on", 1)));
    events.push(ev(
        ts - 2 * DAY,
        "laptop",
        EventBody::Progress {
            series: "ranma 1 2 2024 s3".into(),
            item: ep(1),
            pos: 590.0,
            dur: Some(1420.0),
            file: None,
        },
    ));
    events.push(ev(ts - 3 * DAY, "desktop", watched("meitantei precure", 1)));
    // Two devices, two files: merged on load.
    for dev in ["desktop", "laptop"] {
        let log = crate::events::EventLog::open(ctx.log.dir(), dev)?;
        let mine: Vec<Event> = events.iter().filter(|e| e.dev == dev).cloned().collect();
        log.append_events(&mine)?;
    }

    // Audience score and popularity, roughly as on AniList; the newest show has
    // too few ratings yet.
    let score = |id: u64| -> Option<(u8, u32)> {
        Some(match id {
            182_255 => (90, 412_000),
            21 => (88, 612_000),
            185_660 => (85, 198_000),
            190_000 => (74, 31_000),
            1 => (86, 455_000),
            477 => (78, 24_000),
            5680 => (77, 298_000),
            98444 => (83, 251_000),
            153_518 => (86, 287_000),
            _ => return None,
        })
    };
    let meta = |series: &str, id: u64, eps: Option<u32>, status: &str, next: Option<(u32, i64)>| SeriesMeta {
        series: series.into(),
        anilist: Some(id),
        title: None,
        episodes: eps,
        status: Some(status.into()),
        format: Some("TV".into()),
        year: None,
        next_ep: next.map(|n| n.0),
        next_airing: next.map(|n| n.1),
        refreshed_at: Some(ts),
        search_linked: false,
        score: score(id).map(|s| s.0),
        popularity: score(id).map(|s| s.1),
    };
    ctx.db.put_metas(&[
        meta("sousou no frieren", 182_255, Some(38), "RELEASING", Some((32, ts + 3 * DAY + 5 * 3600))),
        meta("one piece", 21, None, "RELEASING", Some((1183, ts + 5 * DAY + 2 * 3600))),
        meta("dandadan s2", 185_660, Some(12), "RELEASING", Some((9, ts + DAY + 7 * 3600))),
        meta("ranma 1 2 2024 s3", 190_000, Some(12), "RELEASING", Some((3, ts + 6 * DAY))),
        meta("cowboy bebop", 1, Some(26), "FINISHED", None),
        meta("aria the animation", 477, Some(13), "FINISHED", None),
        meta("k on", 5680, Some(13), "FINISHED", None),
        meta("yuru camp", 98444, Some(12), "FINISHED", None),
        meta("bakemonogatari", 5081, Some(15), "FINISHED", None),
        meta("nisemonogatari", 11597, Some(11), "FINISHED", None),
        meta("nekomonogatari", 15689, Some(4), "FINISHED", None),
        // In the Inbox: score and popularity help decide whether to follow it.
        meta("dungeon meshi", 153_518, Some(24), "FINISHED", None),
        meta("meitantei precure", 195_000, Some(48), "RELEASING", Some((4, ts + 4 * DAY))),
    ])?;
    Ok(ctx)
}

/// Convenience for tests: demo with the current time.
pub fn setup_now(home: &Path) -> Result<Ctx> {
    setup(home, now())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The demo never goes over an existing home (its config, its event logs).
    #[test]
    fn demo_needs_an_empty_folder() {
        let dir = tempfile::tempdir().unwrap();
        setup_now(dir.path()).unwrap();
        let events = std::fs::read_to_string(dir.path().join("data/events/desktop.jsonl")).unwrap();
        let err = setup_now(dir.path()).err().unwrap();
        assert!(err.to_string().contains("not empty"), "{err}");
        assert_eq!(std::fs::read_to_string(dir.path().join("data/events/desktop.jsonl")).unwrap(), events);
    }

    #[test]
    fn demo_library_is_sensible() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = setup_now(dir.path()).unwrap();
        let lib = ctx.library().unwrap();
        assert!(lib.up_next(false).iter().any(|s| s.key == "sousou no frieren"));
        let fr = lib.get("sousou no frieren").unwrap();
        assert_eq!(fr.next_up().unwrap().key, ItemKey::episode(EpNo::new(29)));
        assert_eq!(lib.get("dandadan s2").unwrap().new_episodes().len(), 3);
        assert_eq!(lib.get("aria the animation").unwrap().status, SeriesStatus::Paused);
        assert!(std::fs::read_dir(ctx.log.dir()).unwrap().count() >= 2);
        let (mono, pos) = lib.franchise_of("nisemonogatari").unwrap();
        assert_eq!((mono.name.as_str(), pos, mono.stops.len()), ("Monogatari Series", 1, 3));
    }

    /// Short names for a franchise's entries: a season by its number, the
    /// first one `S1`; folder entries and a movie keep their whole name. Each
    /// entry knows the ones before and after it.
    #[test]
    fn franchise_entries_have_short_names_and_neighbours() {
        let dir = tempfile::tempdir().unwrap();
        let lib = setup_now(dir.path()).unwrap().library().unwrap();
        let short = |key: &str| lib.short_series_label(lib.index_of(key).unwrap());
        assert_eq!(short("sousou no frieren"), Some("S1"));
        assert_eq!(short("sousou no frieren s2"), Some("S2"));
        assert_eq!(short("nisemonogatari"), Some("Nisemonogatari"));
        assert_eq!(short("one piece"), None, "not in a franchise");
        let keys = |key: &str| {
            let (prev, next) = lib.franchise_neighbours(lib.index_of(key).unwrap());
            let key = |i: Option<usize>| i.map(|i| lib.series[i].key.as_str());
            (key(prev), key(next))
        };
        assert_eq!(keys("nisemonogatari"), (Some("bakemonogatari"), Some("nekomonogatari")));
        assert_eq!(keys("sousou no frieren"), (None, Some("sousou no frieren s2")));
        assert_eq!(keys("one piece"), (None, None));
    }
}
