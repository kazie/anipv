//! The *anime-offline-database*: a weekly JSON dump of ~40k anime with
//! titles, synonyms, episode counts and cross-site ids.
//!
//! The original (manami-project) was archived after its `2026-27` release.
//! By default the community continuation (cedya77, same file format) is
//! downloaded, and the archived original only when nothing is cached yet.
//!
//! Downloaded once into the cache and queried locally; nothing about the
//! user's library is ever sent anywhere.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::SystemTime;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::identity::series_key;
use crate::index::db::SeriesMeta;

/// Release asset URLs of the minified database, in the order they are tried:
/// the community continuation, then the archived original (frozen at
/// `2026-27`, still downloadable).
pub const DEFAULT_URLS: [&str; 2] = [
    "https://github.com/cedya77/anime-offline-database/releases/latest/download/anime-offline-database-minified.json",
    "https://github.com/manami-project/anime-offline-database/releases/latest/download/anime-offline-database-minified.json",
];

/// Where to download the database from: `configured` (`offline_db_url`) alone
/// if set, else [`DEFAULT_URLS`].
pub fn source_urls(configured: Option<&str>) -> Vec<String> {
    configured.map_or_else(|| DEFAULT_URLS.map(String::from).to_vec(), |u| vec![u.to_string()])
}

/// How old a dataset may get before a download says its source looks
/// unmaintained.
pub const STALE_DATASET_DAYS: i64 = 90;

/// What to tell the user after a download from `source` when `first` was
/// tried first: that a fallback served it, and that the dataset (its
/// `lastUpdate`, `version`) is older than [`STALE_DATASET_DAYS`] at `now`.
/// The original database was archived in 2026-07, and a source that stops
/// updating can't match newer shows.
pub fn download_notes(first: &str, source: &str, version: Option<&str>, now: i64) -> Vec<String> {
    let fallback = (source != first).then(|| format!("{first} failed; downloaded from {source}"));
    let stale = version.and_then(|v| {
        let days = days_since(v, now).filter(|d| *d > STALE_DATASET_DAYS)?;
        Some(format!(
            "this dataset was last updated {v} ({days} days ago); its source may be unmaintained, \
             see `offline_db_url` in docs/config.md"
        ))
    });
    fallback.into_iter().chain(stale).collect()
}

/// Whole days from `date` (`YYYY-MM-DD`, a database's `lastUpdate`) until
/// `now` (Unix seconds), or `None` if `date` doesn't parse.
pub fn days_since(date: &str, now: i64) -> Option<i64> {
    let mut parts = date.splitn(3, '-').map(|p| p.parse::<i64>().ok());
    let (y, m, d) = (parts.next()??, parts.next()??, parts.next()??);
    // The date comes from a download: keep the arithmetic below in range.
    if !(1..=9999).contains(&y) {
        return None;
    }
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let month_days = [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    if !(1..=12).contains(&m) || !(1..=month_days[usize::try_from(m - 1).ok()?]).contains(&d) {
        return None;
    }
    // Days from the civil date to 1970-01-01 (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(now.div_euclid(super::DAY) - (era * 146_097 + doe - 719_468))
}

/// One anime entry (only the fields anipv uses).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Entry {
    /// Source URLs (`AniList`, MAL, `AniDB`…).
    #[serde(default)]
    pub sources: Vec<String>,
    /// Main title.
    pub title: String,
    /// `TV`, `MOVIE`, `OVA`, `ONA`, `SPECIAL`, `UNKNOWN`.
    #[serde(rename = "type", default)]
    pub format: String,
    /// Episode count (0 when unknown).
    #[serde(default)]
    pub episodes: u32,
    /// `FINISHED`, `ONGOING`, `UPCOMING`, `UNKNOWN`.
    #[serde(default)]
    pub status: String,
    /// Season of first airing.
    #[serde(rename = "animeSeason", default)]
    pub season: Option<AnimeSeason>,
    /// Alternative titles.
    #[serde(default)]
    pub synonyms: Vec<String>,
}

/// Airing season.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct AnimeSeason {
    /// Year, if known.
    pub year: Option<i32>,
}

impl Entry {
    /// One line about the entry: title, format, year, episodes and `AniList` id.
    pub fn describe(&self) -> String {
        let format = Some(self.format.as_str()).filter(|f| !f.is_empty());
        let episodes = (self.episodes > 0).then_some(self.episodes);
        super::describe_anime(&self.title, format, self.year(), episodes, self.anilist_id().unwrap_or(0))
    }

    /// `AniList` id from the sources, if any.
    pub fn anilist_id(&self) -> Option<u64> {
        // Other sites' URLs don't parse; a bare number never appears here.
        self.sources.iter().find_map(|s| super::anilist::parse_ref(s))
    }

    /// Start year.
    pub fn year(&self) -> Option<i32> {
        self.season.as_ref().and_then(|s| s.year)
    }

    /// Convert to a cache row for `series` (not yet refreshed from `AniList`).
    pub fn to_meta(&self, series: &str) -> SeriesMeta {
        SeriesMeta {
            series: series.to_string(),
            anilist: self.anilist_id(),
            title: Some(self.title.clone()),
            episodes: (self.episodes > 0).then_some(self.episodes),
            status: Some(match self.status.as_str() {
                "ONGOING" => "RELEASING".to_string(),
                "UPCOMING" => "NOT_YET_RELEASED".to_string(),
                s => s.to_string(),
            }),
            format: Some(self.format.clone()),
            year: self.year(),
            next_ep: None,
            next_airing: None,
            refreshed_at: None,
            search_linked: false,
        }
    }
}

#[derive(Deserialize)]
struct File {
    #[serde(rename = "lastUpdate", default)]
    last_update: Option<String>,
    data: Vec<Entry>,
}

/// Loaded database with a title index.
#[derive(Debug, Default)]
pub struct OfflineDb {
    /// Release date of this database (`lastUpdate`), used to expire "no match" results.
    pub version: Option<String>,
    /// All entries that have an `AniList` id.
    pub entries: Vec<Entry>,
    /// Normalized title/synonym → (entry index, is main title), built on
    /// the first match (see [`OfflineDb::title_index`]).
    index: std::sync::OnceLock<HashMap<String, Vec<(usize, bool)>>>,
    by_anilist: HashMap<u64, usize>,
}

/// Outcome of matching a series key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Match<'a> {
    /// One clear winner.
    Unique(&'a Entry),
    /// Several equally good candidates.
    Ambiguous(Vec<&'a Entry>),
    /// Nothing with this name.
    None,
}

impl OfflineDb {
    /// The version "no match" results are recorded under: `lastUpdate`, or
    /// `""` for a database without one (so they still count for it).
    pub fn version_key(&self) -> &str {
        self.version.as_deref().unwrap_or("")
    }

    /// True if the database was never downloaded or is older than
    /// [`super::OFFLINE_DB_MAX_AGE`].
    pub fn needs_update(cache_dir: &Path) -> bool {
        let max_age = std::time::Duration::from_secs(super::OFFLINE_DB_MAX_AGE.unsigned_abs());
        modified(cache_dir).and_then(|t| t.elapsed().ok()).is_none_or(|age| age > max_age)
    }

    /// Location of the cached database file.
    pub fn path(cache_dir: &Path) -> PathBuf {
        cache_dir.join("anime-offline-database-minified.json")
    }

    /// Parse the database JSON.
    pub fn from_slice(json: &[u8]) -> Result<Self> {
        let file: File = serde_json::from_slice(json).context("parsing anime-offline-database")?;
        Ok(Self { version: file.last_update, ..Self::from_entries(file.data) })
    }

    /// Build from entries (entries without an `AniList` id are dropped).
    pub fn from_entries(entries: Vec<Entry>) -> Self {
        // Each entry's sources are parsed for its id once.
        let (ids, entries): (Vec<u64>, Vec<Entry>) =
            entries.into_iter().filter_map(|e| Some((e.anilist_id()?, e))).unzip();
        let by_anilist: HashMap<u64, usize> = ids.iter().enumerate().map(|(i, &id)| (id, i)).collect();
        Self { version: None, entries, index: std::sync::OnceLock::new(), by_anilist }
    }

    /// The entries by the key of each title and synonym. Keying every title is
    /// most of what loading the database costs, so it is done on the first
    /// match: searching and updating never need it.
    fn title_index(&self) -> &HashMap<String, Vec<(usize, bool)>> {
        self.index.get_or_init(|| {
            let mut index: HashMap<String, Vec<(usize, bool)>> = HashMap::new();
            for (i, e) in self.entries.iter().enumerate() {
                let main = series_key(&e.title);
                index.entry(main.clone()).or_default().push((i, true));
                for s in &e.synonyms {
                    let k = series_key(s);
                    if !k.is_empty() && k != main {
                        let v = index.entry(k).or_default();
                        if !v.iter().any(|(j, _)| *j == i) {
                            v.push((i, false));
                        }
                    }
                }
            }
            index
        })
    }

    /// Load from the cache, if it has been downloaded.
    pub fn load(cache_dir: &Path) -> Result<Option<Self>> {
        let p = Self::path(cache_dir);
        match std::fs::read(&p) {
            Ok(json) => Ok(Some(Self::from_slice(&json)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {}", p.display())),
        }
    }

    /// Download the latest release into the cache from `urls`, the first one
    /// that works. Returns the loaded database and what to tell the user about
    /// it: that a fallback served it, or that it is old (see [`download_notes`]).
    ///
    /// Only the first URL is tried when a copy is cached already: falling back
    /// to the next one (an older, archived source) could replace the cached
    /// copy with an older one. Without a cached copy every URL is tried, and
    /// the error names each failure.
    pub fn download(cache_dir: &Path, urls: &[String]) -> Result<(Self, Vec<String>)> {
        let (db, source) = Self::download_with(cache_dir, urls, &fetch)?;
        let first = urls.first().map_or("", String::as_str);
        let notes = download_notes(first, &source, db.version.as_deref(), crate::events::now());
        Ok((db, notes))
    }

    /// [`OfflineDb::download`] reading each URL through `fetch`; returns the
    /// URL that served it.
    fn download_with(
        cache_dir: &Path,
        urls: &[String],
        fetch: &dyn Fn(&str) -> Result<Vec<u8>>,
    ) -> Result<(Self, String)> {
        std::fs::create_dir_all(cache_dir)?;
        let cached = Self::path(cache_dir).exists();
        let tried = if cached { &urls[..urls.len().min(1)] } else { urls };
        let mut errors = Vec::new();
        for url in tried {
            // Validate before replacing the old copy.
            match fetch(url).and_then(|json| Ok((Self::from_slice(&json)?, json))) {
                Ok((db, json)) => {
                    Self::store(cache_dir, &json)?;
                    return Ok((db, url.clone()));
                }
                Err(e) => errors.push(format!("{url}: {e:#}")),
            }
        }
        if errors.is_empty() {
            anyhow::bail!("no download source for anime-offline-database");
        }
        anyhow::bail!("downloading anime-offline-database failed ({})", errors.join("; "))
    }

    /// Atomically replace the cached database with `json`.
    ///
    /// The copy is written to a temporary file unique to this call (process
    /// id, time and a counter), so two downloads running at once (two anipv
    /// processes) never write into each other's file; the last rename wins.
    fn store(cache_dir: &Path, json: &[u8]) -> Result<()> {
        crate::config::replace_file(&Self::path(cache_dir), json).context("storing anime-offline-database")
    }

    /// Entry with the given `AniList` id.
    pub fn by_anilist(&self, id: u64) -> Option<&Entry> {
        self.by_anilist.get(&id).map(|&i| &self.entries[i])
    }

    /// Match a series key to an entry.
    ///
    /// Candidates are ranked by these criteria, most important first; the
    /// match is unique when exactly one candidate ranks highest:
    ///
    /// 1. it fits `max_ep` (the highest episode number seen): an entry that is
    ///    not ongoing and has a known count below it ranks below every entry
    ///    that fits, but is still matched when no candidate fits;
    /// 2. the key is its main title rather than a synonym;
    /// 3. with more than one numbered episode (`episodes`), it is TV or ONA,
    ///    which steers away from same-named movies;
    /// 4. `title` (the display title) equals its main title, then one of its
    ///    synonyms, which separates entries sharing a normalized name, e.g.
    ///    `Gintama` vs `Gintama'`;
    /// 5. with at most one numbered episode, it is TV;
    /// 6. it is known to more sites (better curated than sparse duplicates).
    pub fn match_key(&self, key: &str, title: &str, episodes: usize, max_ep: u32) -> Match<'_> {
        let Some(cands) = self.title_index().get(key) else { return Match::None };
        let title = title.to_lowercase();
        // Compared lexicographically, most important first.
        let score = |&(i, main): &(usize, bool)| {
            let e = &self.entries[i];
            let exact_main = e.title.to_lowercase() == title;
            let exact_syn = !exact_main && e.synonyms.iter().any(|s| s.to_lowercase() == title);
            let fits = !(e.episodes > 0 && e.status != "ONGOING" && max_ep > e.episodes);
            let series_format = episodes > 1 && matches!(e.format.as_str(), "TV" | "ONA");
            let single_tv = episodes <= 1 && e.format == "TV";
            (fits, main, series_format, exact_main, exact_syn, single_tv, e.sources.len())
        };
        let mut ranked: Vec<(_, &Entry)> = cands.iter().map(|c| (score(c), &self.entries[c.0])).collect();
        ranked.sort_by_key(|r| std::cmp::Reverse(r.0));
        let best = ranked[0].0;
        let top: Vec<&Entry> = ranked.iter().filter(|(s, _)| *s == best).map(|(_, e)| *e).collect();
        if top.len() == 1 { Match::Unique(top[0]) } else { Match::Ambiguous(top) }
    }

    /// Fuzzy search titles and synonyms.
    pub fn search(&self, query: &str, limit: usize) -> Vec<&Entry> {
        crate::app::fuzzy_rank(self.entries.iter(), query, |e| {
            format!("{title} {synonyms}", title = e.title, synonyms = e.synonyms.join(" ")).into()
        })
        .into_iter()
        .take(limit)
        .map(|(e, _)| e)
        .collect()
    }
}

/// Read `url` over HTTP.
fn fetch(url: &str) -> Result<Vec<u8>> {
    // A stalled connection must not keep a metadata update "running" forever.
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(std::time::Duration::from_secs(20)))
        .timeout_global(Some(std::time::Duration::from_secs(600)))
        .build()
        .into();
    let mut resp = agent.get(url).call()?;
    Ok(resp.body_mut().with_config().limit(512 * 1024 * 1024).read_to_vec()?)
}

/// The cached database once loaded, kept for later syncs in the same session
/// while the file's modification time stays the same (parsing it takes a while).
#[derive(Debug, Default)]
pub struct OfflineCache(Mutex<Option<(SystemTime, Arc<OfflineDb>)>>);

impl OfflineCache {
    /// [`OfflineDb::load`], or the copy loaded before if the file is unchanged.
    pub fn load(&self, cache_dir: &Path) -> Result<Option<Arc<OfflineDb>>> {
        let mut slot = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        // Read first: a file replaced while loading is loaded again next time.
        let mtime = modified(cache_dir);
        if let (Some((at, db)), Some(mtime)) = (slot.as_ref(), mtime)
            && *at == mtime
        {
            return Ok(Some(Arc::clone(db)));
        }
        let db = OfflineDb::load(cache_dir)?.map(Arc::new);
        *slot = mtime.zip(db.clone());
        Ok(db)
    }

    /// [`OfflineDb::download`], keeping the result for later loads.
    pub fn download(&self, cache_dir: &Path, urls: &[String]) -> Result<(Arc<OfflineDb>, Vec<String>)> {
        let (db, notes) = OfflineDb::download(cache_dir, urls)?;
        let db = Arc::new(db);
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = modified(cache_dir).map(|t| (t, Arc::clone(&db)));
        Ok((db, notes))
    }
}

/// Modification time of the cached database file.
fn modified(cache_dir: &Path) -> Option<SystemTime> {
    std::fs::metadata(OfflineDb::path(cache_dir)).and_then(|m| m.modified()).ok()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub const SAMPLE: &str = r#"{"lastUpdate":"2026-07-04","data":[
      {"sources":["https://anilist.co/anime/21","https://myanimelist.net/anime/21","https://anidb.net/anime/69"],"title":"One Piece","type":"TV","episodes":1168,"status":"ONGOING","animeSeason":{"season":"FALL","year":1999},"synonyms":["OP","ONE PIECE"]},
      {"sources":["https://animenewsnetwork.com/encyclopedia/anime.php?id=836"],"title":"One Piece","type":"TV","episodes":1174,"status":"FINISHED","synonyms":[]},
      {"sources":["https://anilist.co/anime/199111","https://myanimelist.net/anime/62542"],"title":"Grand Blue Season 3","type":"TV","episodes":12,"status":"UPCOMING","animeSeason":{"year":2026},"synonyms":["Grand Blue S3","Grand Blue"]},
      {"sources":["https://anilist.co/anime/100922","https://myanimelist.net/anime/37105","https://anidb.net/anime/13734"],"title":"Grand Blue","type":"TV","episodes":12,"status":"FINISHED","synonyms":["Grand Blue Dreaming"]},
      {"sources":["https://anilist.co/anime/1"],"title":"Twin A","type":"TV","episodes":1,"status":"FINISHED","synonyms":["Shared"]},
      {"sources":["https://anilist.co/anime/2"],"title":"Twin B","type":"TV","episodes":1,"status":"FINISHED","synonyms":["Shared"]}
    ]}"#;

    fn db() -> OfflineDb {
        OfflineDb::from_slice(SAMPLE.as_bytes()).unwrap()
    }

    #[test]
    fn drops_entries_without_anilist() {
        assert_eq!(db().entries.len(), 5);
        assert_eq!(db().version.as_deref(), Some("2026-07-04"));
    }

    #[test]
    fn matching() {
        let db = db();
        match db.match_key("one piece", "One Piece", 0, 0) {
            Match::Unique(e) => assert_eq!(e.anilist_id(), Some(21)),
            m => panic!("{m:?}"),
        }
        match db.match_key("grand blue s3", "Grand Blue S3", 0, 0) {
            Match::Unique(e) => assert_eq!(e.anilist_id(), Some(199_111)),
            m => panic!("{m:?}"),
        }
        // Main title beats the S3 entry's "Grand Blue" synonym.
        match db.match_key("grand blue", "x", 0, 0) {
            Match::Unique(e) => assert_eq!(e.anilist_id(), Some(100_922)),
            m => panic!("{m:?}"),
        }
        assert!(matches!(db.match_key("shared", "x", 0, 0), Match::Ambiguous(v) if v.len() == 2));
        assert!(matches!(db.match_key("shared", "Twin B", 0, 0), Match::Unique(e) if e.title == "Twin B"));
        // `max_ep` only ranks: both twins hold episode 1, so neither is
        // preferred, and an ongoing entry's episode count is no limit.
        assert!(matches!(db.match_key("shared", "x", 1, 1), Match::Ambiguous(_)));
        assert_eq!(db.match_key("one piece", "One Piece", 500, 1180), db.match_key("one piece", "One Piece", 0, 0));
        assert_eq!(db.match_key("nope", "", 0, 0), Match::None);
    }

    #[test]
    fn meta_conversion() {
        let db = db();
        let m = db.by_anilist(21).unwrap().to_meta("one piece");
        assert_eq!(m.anilist, Some(21));
        assert_eq!(m.status.as_deref(), Some("RELEASING"));
        assert_eq!(m.year, Some(1999));
        assert_eq!(m.episodes, Some(1168));
    }

    #[test]
    fn search() {
        let db = db();
        assert_eq!(db.search("grand blue dreaming", 3)[0].title, "Grand Blue");
    }

    fn dir_names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> =
            std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        v.sort();
        v
    }

    /// Each store writes its own temporary file, so concurrent downloads
    /// can't clobber each other's partial copy, and none is left behind.
    #[test]
    fn store_uses_unique_temp_files_and_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| OfflineDb::store(dir.path(), SAMPLE.as_bytes()).unwrap());
            }
        });
        assert_eq!(dir_names(dir.path()), ["anime-offline-database-minified.json"]);
        assert_eq!(OfflineDb::load(dir.path()).unwrap().unwrap().entries.len(), 5);

        // A failed rename (the target is a non-empty folder) removes the temp file.
        let dir = tempfile::tempdir().unwrap();
        let target = OfflineDb::path(dir.path());
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("x"), "").unwrap();
        assert!(OfflineDb::store(dir.path(), SAMPLE.as_bytes()).is_err());
        assert_eq!(dir_names(dir.path()), ["anime-offline-database-minified.json"]);
    }

    /// The cache gives the copy it loaded until the file changes.
    #[test]
    fn cached_loads_are_reused_until_the_file_changes() {
        let dir = tempfile::tempdir().unwrap();
        let cache = OfflineCache::default();
        assert!(cache.load(dir.path()).unwrap().is_none());
        std::fs::write(OfflineDb::path(dir.path()), SAMPLE).unwrap();
        let a = cache.load(dir.path()).unwrap().unwrap();
        assert!(Arc::ptr_eq(&a, &cache.load(dir.path()).unwrap().unwrap()), "reused");
        let file = std::fs::File::options().write(true).open(OfflineDb::path(dir.path())).unwrap();
        file.set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1)).unwrap();
        let b = cache.load(dir.path()).unwrap().unwrap();
        assert!(!Arc::ptr_eq(&a, &b), "loaded again");
        assert_eq!(b.entries.len(), 5);
        std::fs::remove_file(OfflineDb::path(dir.path())).unwrap();
        assert!(cache.load(dir.path()).unwrap().is_none());
    }

    fn urls(list: &[&str]) -> Vec<String> {
        list.iter().map(ToString::to_string).collect()
    }

    /// Answers `a` with the sample database, `b` with an older one, and
    /// anything else with an error; records what was asked.
    fn fake(asked: &std::cell::RefCell<Vec<String>>) -> impl Fn(&str) -> Result<Vec<u8>> + '_ {
        move |url: &str| {
            asked.borrow_mut().push(url.to_string());
            match url {
                "a" => Ok(SAMPLE.as_bytes().to_vec()),
                "b" => Ok(SAMPLE.replace("2026-07-04", "2026-01-01").into_bytes()),
                _ => anyhow::bail!("connection refused"),
            }
        }
    }

    #[test]
    fn download_falls_back_only_without_a_cached_copy() {
        let asked = std::cell::RefCell::new(Vec::new());
        // Nothing cached: the second source serves it, and the error would name both.
        let dir = tempfile::tempdir().unwrap();
        let (db, source) = OfflineDb::download_with(dir.path(), &urls(&["down", "a"]), &fake(&asked)).unwrap();
        assert_eq!(db.version.as_deref(), Some("2026-07-04"));
        assert_eq!(source, "a", "says which source served it");
        assert_eq!(*asked.borrow(), ["down", "a"]);
        assert_eq!(OfflineDb::load(dir.path()).unwrap().unwrap().entries.len(), 5);

        // Cached: only the first source is tried, and the cached copy stays.
        asked.borrow_mut().clear();
        let err = OfflineDb::download_with(dir.path(), &urls(&["down", "b"]), &fake(&asked)).unwrap_err();
        assert_eq!(*asked.borrow(), ["down"]);
        assert!(format!("{err:#}").contains("down: connection refused"), "{err:#}");
        assert_eq!(OfflineDb::load(dir.path()).unwrap().unwrap().version.as_deref(), Some("2026-07-04"));

        // Nothing cached and every source failing names each of them.
        let dir = tempfile::tempdir().unwrap();
        let err = OfflineDb::download_with(dir.path(), &urls(&["x", "y"]), &fake(&asked)).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("x: connection refused") && msg.contains("y: connection refused"), "{msg}");
        assert!(OfflineDb::load(dir.path()).unwrap().is_none(), "nothing stored");
        assert!(OfflineDb::download_with(dir.path(), &[], &fake(&asked)).is_err());
    }

    /// A source that answers with something that isn't the database is skipped.
    #[test]
    fn download_skips_a_source_that_sends_garbage() {
        let dir = tempfile::tempdir().unwrap();
        let get = |url: &str| -> Result<Vec<u8>> {
            Ok(if url == "html" { b"<html>rate limited</html>".to_vec() } else { SAMPLE.as_bytes().to_vec() })
        };
        let (db, source) = OfflineDb::download_with(dir.path(), &urls(&["html", "a"]), &get).unwrap();
        assert_eq!((db.entries.len(), source.as_str()), (5, "a"));
    }

    #[test]
    fn sources_default_to_the_continuation_then_the_archive() {
        let urls = source_urls(None);
        assert_eq!(urls.len(), 2);
        assert!(urls[0].contains("cedya77/") && urls[1].contains("manami-project/"), "{urls:?}");
        assert!(urls.iter().all(|u| u.ends_with("/releases/latest/download/anime-offline-database-minified.json")));
        // A configured URL is the only one.
        assert_eq!(source_urls(Some("https://example.org/db.json")), ["https://example.org/db.json"]);
    }

    #[test]
    fn downloads_say_when_a_fallback_served_them_and_when_they_are_old() {
        let now = 1_791_417_600; // 2026-10-08
        let notes = |source, version| download_notes("https://a/db.json", source, version, now);
        assert!(notes("https://a/db.json", Some("2026-10-04")).is_empty(), "first source, fresh");
        assert!(notes("https://a/db.json", Some("2026-07-10")).is_empty(), "90 days or less");
        assert_eq!(
            notes("https://b/db.json", Some("2026-10-04")),
            ["https://a/db.json failed; downloaded from https://b/db.json"]
        );
        let old = notes("https://a/db.json", Some("2026-07-04"));
        assert!(
            old.len() == 1 && old[0].contains("2026-07-04 (96 days ago)") && old[0].contains("offline_db_url"),
            "{old:?}"
        );
        assert!(notes("https://a/db.json", Some("unknown")).is_empty());
    }

    #[test]
    fn dataset_age_in_days() {
        // 2026-10-08T00:00:00Z
        let now = 1_791_417_600;
        assert_eq!(days_since("2026-10-08", now), Some(0));
        assert_eq!(days_since("2026-10-04", now + 3600), Some(4));
        assert_eq!(days_since("2026-07-04", now), Some(96));
        assert_eq!(days_since("2025-10-08", now), Some(365));
        assert_eq!(days_since("2026-10-09", now), Some(-1));
        assert_eq!(days_since("2024-02-29", now), Some(952), "a leap day");
        for bad in
            ["", "2026", "2026-13-01", "2026-00-10", "x-y-z", "2026-10", "2026-02-29", "2026-02-31", "2026-04-31"]
        {
            assert_eq!(days_since(bad, now), None, "{bad:?}");
        }
    }

    #[test]
    fn load_missing_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(OfflineDb::load(dir.path()).unwrap().is_none());
        std::fs::write(OfflineDb::path(dir.path()), SAMPLE).unwrap();
        assert_eq!(OfflineDb::load(dir.path()).unwrap().unwrap().entries.len(), 5);
    }
}
