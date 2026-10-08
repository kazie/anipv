//! The manami-project *anime-offline-database*: a weekly JSON dump of ~40k
//! anime with titles, synonyms, episode counts and cross-site ids.
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

/// Release asset URL of the minified database.
pub const URL: &str = "https://github.com/manami-project/anime-offline-database/releases/latest/download/anime-offline-database-minified.json";

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
        format!(
            "{} ({}{}, {} eps) anilist:{}",
            self.title,
            self.format,
            self.year().map(|y| format!(" {y}")).unwrap_or_default(),
            self.episodes,
            self.anilist_id().unwrap_or(0)
        )
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
    /// Normalized title/synonym → (entry index, is main title).
    index: HashMap<String, Vec<(usize, bool)>>,
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
    /// True if the database was never downloaded or is older than
    /// [`super::OFFLINE_DB_MAX_AGE`].
    pub fn needs_update(cache_dir: &Path) -> bool {
        let max_age = std::time::Duration::from_secs(super::OFFLINE_DB_MAX_AGE.unsigned_abs());
        std::fs::metadata(Self::path(cache_dir))
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_none_or(|age| age > max_age)
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
        let mut index: HashMap<String, Vec<(usize, bool)>> = HashMap::new();
        let by_anilist: HashMap<u64, usize> = ids.iter().enumerate().map(|(i, &id)| (id, i)).collect();
        for (i, e) in entries.iter().enumerate() {
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
        Self { version: None, entries, index, by_anilist }
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

    /// Download the latest release into the cache. Returns the loaded database.
    pub fn download(cache_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(cache_dir)?;
        // A stalled connection must not keep a metadata update "running" forever.
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_connect(Some(std::time::Duration::from_secs(20)))
            .timeout_global(Some(std::time::Duration::from_secs(600)))
            .build()
            .into();
        let mut resp = agent.get(URL).call().context("downloading anime-offline-database")?;
        let json = resp
            .body_mut()
            .with_config()
            .limit(512 * 1024 * 1024)
            .read_to_vec()
            .context("downloading anime-offline-database")?;
        // Validate before replacing the old copy.
        let db = Self::from_slice(&json)?;
        Self::store(cache_dir, &json)?;
        Ok(db)
    }

    /// Atomically replace the cached database with `json`.
    ///
    /// The copy is written to a temporary file unique to this call (process
    /// id, time and a counter), so two downloads running at once (two anipv
    /// processes) never write into each other's file; the last rename wins.
    fn store(cache_dir: &Path, json: &[u8]) -> Result<()> {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = cache_dir.join(format!("anime-offline-database.json.part.{}.{nanos}.{seq}", std::process::id()));
        let stored = std::fs::write(&tmp, json).and_then(|()| std::fs::rename(&tmp, Self::path(cache_dir)));
        if let Err(e) = stored {
            // A partly written copy must not stay behind in the cache.
            let _ = std::fs::remove_file(&tmp);
            return Err(e).context("storing anime-offline-database");
        }
        Ok(())
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
        let Some(cands) = self.index.get(key) else { return Match::None };
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
    pub fn download(&self, cache_dir: &Path) -> Result<Arc<OfflineDb>> {
        let db = Arc::new(OfflineDb::download(cache_dir)?);
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = modified(cache_dir).map(|t| (t, Arc::clone(&db)));
        Ok(db)
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

    #[test]
    fn load_missing_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(OfflineDb::load(dir.path()).unwrap().is_none());
        std::fs::write(OfflineDb::path(dir.path()), SAMPLE).unwrap();
        assert_eq!(OfflineDb::load(dir.path()).unwrap().unwrap().entries.len(), 5);
    }
}
