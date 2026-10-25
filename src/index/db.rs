//! SQLite cache of scanned files and fetched metadata.
//!
//! Everything in here can be rebuilt (rescan + refetch); the event log is the
//! only thing that must be kept and synced. So there are hardly any
//! migrations: when [`SCHEMA_VERSION`] changes, the old tables are dropped and
//! the next scan and metadata sync fill them again. Only versions listed in
//! `UPGRADABLE` are upgraded in place instead (their changes were purely
//! additive), so the files and their first-seen times survive.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};

use super::scan::ScannedFile;

/// Bump on any schema change; older caches are dropped and rebuilt (unless
/// listed in `UPGRADABLE`).
pub const SCHEMA_VERSION: i32 = 9;

/// Older schema versions brought up to date in place rather than rebuilt:
/// [`SCHEMA`] only adds to them (`CREATE … IF NOT EXISTS`), and [`upgrade`]
/// fills what was added.
///
/// * 4 → 5: the `anilist_checks` table, seeded from `prequels`.
/// * 5 → 6: `series_meta.search_linked`.
/// * 6 → 7: `series_meta.score` and `series_meta.popularity`.
/// * 7 → 8: the `sequels` table (filled as series are asked about again).
/// * 8 → 9: `series_meta.titles`, `hashtags`, `studios` and `season` (filled
///   by one refresh, see [`SeriesMeta::titles`]).
const UPGRADABLE: [i32; 5] = [4, 5, 6, 7, 8];

/// `kv` key prefix (followed by the root name) remembering whether the root
/// directory was a mount point at its last non-empty scan, and which
/// directory that was (`"1:/mnt/anime"` or `"0:/home/me/anime"`).
const ROOT_MOUNTED: &str = "root_mounted:";

/// Whether an empty scan of a root that used to have files is believable
/// (the files really are gone) rather than an unmounted drive.
///
/// `was` is the mount state recorded at the last non-empty scan, `now` the
/// state at this one. The rule: believe it only if the state is known now and
/// is the same as then, no record counting as a plain folder.
///
/// So a plain folder that still is one was emptied by the user, and a mount
/// that is still mounted is a legitimately empty filesystem. Without a record
/// (an index from before mount states were kept, or a root whose path
/// changed) an empty plain folder is believed too: if it really was an
/// unmounted drive's mount point, its files only drop out of the index until
/// a scan with the drive mounted brings them back. A mount that is no longer
/// one, a folder that has become a mount (maybe an automounter's empty
/// placeholder), or an unknown state (the stat failed) is treated as
/// unmounted, so nothing is lost.
pub fn trust_empty_scan(was: Option<bool>, now: Option<bool>) -> bool {
    now == Some(was.unwrap_or(false))
}

/// How a root's mount state at a scan of `base` is recorded (see [`recorded_mount`]).
fn mount_record(mounted: bool, base: &Path) -> String {
    format!("{from}:{base}", from = u8::from(mounted), base = base.to_string_lossy())
}

/// The mount state recorded for a root, if it was recorded for `base`.
/// Records of another directory (the root's path changed) and records
/// without a path (older versions) count as no record.
fn recorded_mount(value: &str, base: &Path) -> Option<bool> {
    let (state, path) = value.split_once(':')?;
    (path == base.to_string_lossy()).then_some(state == "1")
}

/// `kv` key holding the version of the downloaded offline database.
pub const OFFLINE_DB_VERSION: &str = "offline_db_version";

/// `kv` key prefix (followed by the root name) remembering the last attempt
/// to scan the root, reached or not, and what was scanned
/// (`"1760000000:ok:<fingerprint>"` or `"…:offline:…"`): an unmounted root
/// has no scan of its own, but was looked at all the same.
const SCAN_TRIED: &str = "scan_tried:";

/// `kv` key counting scans that changed which files are present, and roots
/// that became reachable or unreachable, so an open TUI notices them (and
/// not every scan that found nothing new).
const FILE_CHANGES: &str = "file_changes";

/// `kv` key counting the metadata writes (updates, links, renames), so an
/// open TUI notices one made by another process.
const META_SYNCS: &str = "meta_syncs";

/// `kv` key prefixes followed by a root name, forgotten with the root.
const ROOT_KEYS: [&str; 2] = [ROOT_MOUNTED, SCAN_TRIED];

const TABLES: [&str; 8] =
    ["files", "scans", "kv", "series_meta", "match_attempts", "prequels", "sequels", "anilist_checks"];

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS scans(
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    root        TEXT NOT NULL,
    started_at  INTEGER NOT NULL,
    finished_at INTEGER NOT NULL,
    complete    INTEGER NOT NULL,
    seen        INTEGER NOT NULL DEFAULT 0,
    new         INTEGER NOT NULL DEFAULT 0,
    gone        INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS scans_root ON scans(root, id);
CREATE TABLE IF NOT EXISTS files(
    path       BLOB PRIMARY KEY,
    root       TEXT NOT NULL,
    rel        BLOB NOT NULL,
    size       INTEGER NOT NULL,
    mtime      INTEGER NOT NULL,
    present    INTEGER NOT NULL DEFAULT 1,
    first_seen INTEGER NOT NULL,
    last_scan  INTEGER NOT NULL REFERENCES scans(id)
);
CREATE INDEX IF NOT EXISTS files_root ON files(root, present);
CREATE TABLE IF NOT EXISTS kv(k TEXT PRIMARY KEY, v TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS prequels(
    anilist    INTEGER PRIMARY KEY,
    prequels   TEXT NOT NULL,
    fetched_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS sequels(
    anilist    INTEGER PRIMARY KEY,
    sequels    TEXT NOT NULL,
    fetched_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS anilist_checks(
    anilist    INTEGER PRIMARY KEY,
    checked_at INTEGER NOT NULL,
    found      INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS match_attempts(
    series     TEXT PRIMARY KEY,
    db_version TEXT NOT NULL,
    at         INTEGER NOT NULL,
    result     TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS series_meta(
    series      TEXT PRIMARY KEY,
    anilist     INTEGER,
    title       TEXT,
    episodes    INTEGER,
    status      TEXT,
    format      TEXT,
    year        INTEGER,
    next_ep     INTEGER,
    next_airing INTEGER,
    refreshed_at INTEGER,
    search_linked INTEGER NOT NULL DEFAULT 0,
    score       INTEGER,
    popularity  INTEGER,
    titles      TEXT,
    hashtags    TEXT,
    studios     TEXT,
    season      TEXT
);
";

// SQLite integers are i64; sizes, counts and ids are unsigned in Rust.
fn to_sql(n: impl TryInto<i64>) -> i64 {
    n.try_into().unwrap_or(i64::MAX)
}

fn from_sql<T: TryFrom<i64> + Default>(n: i64) -> T {
    T::try_from(n).unwrap_or_default()
}

// Paths are stored as their raw bytes: file names need not be valid UTF-8
// (old Latin-1 or Shift-JIS names), and a lossy conversion would corrupt them.
fn path_bytes(p: &Path) -> &[u8] {
    use std::os::unix::ffi::OsStrExt;
    p.as_os_str().as_bytes()
}

fn bytes_path(b: Vec<u8>) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    PathBuf::from(std::ffi::OsString::from_vec(b))
}

/// A file known to the index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRow {
    /// Absolute path.
    pub path: PathBuf,
    /// Root name it was found under.
    pub root: String,
    /// Path relative to the root.
    pub rel: PathBuf,
    /// Size in bytes.
    pub size: u64,
    /// Modification time (unix seconds).
    pub mtime: i64,
    /// False once a full scan no longer finds it.
    pub present: bool,
    /// When it was first indexed.
    pub first_seen: i64,
}

#[cfg(test)]
impl FileRow {
    /// A present file at `/<root>/<rel>`, for unit tests.
    pub(crate) fn test(root: &str, rel: &str) -> Self {
        Self {
            path: PathBuf::from("/").join(root).join(rel),
            root: root.into(),
            rel: rel.into(),
            size: 100,
            mtime: 0,
            present: true,
            first_seen: 0,
        }
    }
}

impl SeriesMeta {
    /// True if this row holds data for `AniList` anime `id`.
    pub fn is_for(&self, id: u64) -> bool {
        self.anilist == Some(id)
    }

    /// True if the show has finished airing (and no next episode is known).
    pub fn is_finished(&self) -> bool {
        self.status.as_deref() == Some("FINISHED") && self.next_ep.is_none()
    }

    /// Every name it has: the canonical title, then the `AniList` titles.
    pub fn names(&self) -> Vec<&str> {
        distinct(self.title.iter().chain(self.titles.iter().flatten()).map(String::as_str))
    }
}

/// `names` without repeats, ignoring case: the first spelling stays.
pub fn distinct<'a>(names: impl IntoIterator<Item = &'a str>) -> Vec<&'a str> {
    let mut out: Vec<&str> = Vec::new();
    for name in names {
        if !out.iter().any(|n| n.to_lowercase() == name.to_lowercase()) {
            out.push(name);
        }
    }
    out
}

/// Cached facts about a series from metadata providers.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SeriesMeta {
    /// Series key.
    pub series: String,
    /// `AniList` id.
    pub anilist: Option<u64>,
    /// Canonical title.
    pub title: Option<String>,
    /// Total episodes, if known.
    pub episodes: Option<u32>,
    /// `FINISHED`, `RELEASING`, …
    pub status: Option<String>,
    /// `TV`, `MOVIE`, `OVA`, …
    pub format: Option<String>,
    /// Start year.
    pub year: Option<i32>,
    /// Next episode to air.
    pub next_ep: Option<u32>,
    /// When it airs (unix seconds).
    pub next_airing: Option<i64>,
    /// Last refresh from `AniList`; `None` while the row only holds data from
    /// the offline database (which may predate the show airing).
    pub refreshed_at: Option<i64>,
    /// The link came from a title search on `AniList`, which the offline
    /// database can't confirm (see [`crate::meta::search_unmatched`]): a match
    /// that finds nothing for the series keeps it. A row written for any other
    /// reason (the database, a manual link) replaces it, and the mark with it.
    pub search_linked: bool,
    /// Audience score, 0–100: `AniList`'s average once refreshed, the offline
    /// database's aggregate until then.
    pub score: Option<u8>,
    /// Users with the anime on their `AniList` list (from `AniList` only).
    pub popularity: Option<u32>,
    /// Every title `AniList` has for it (romaji, English, native), distinct;
    /// `None` until refreshed from `AniList` since these were kept, which
    /// makes the row due for one refresh.
    pub titles: Option<Vec<String>>,
    /// Official hashtags, with their `#` (from `AniList` only).
    pub hashtags: Vec<String>,
    /// Main animation studios (from `AniList` only).
    pub studios: Vec<String>,
    /// Airing season: `WINTER`, `SPRING`, `SUMMER`, `FALL` (from `AniList` only).
    pub season: Option<String>,
}

/// "Matching ran for this series and found nothing to link": against the
/// offline database (version `db_version`), or by title on `AniList`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchAttempt {
    /// Series key.
    pub series: String,
    /// Offline database version (`lastUpdate`, `""` without one) it ran with.
    pub db_version: String,
    /// When it ran.
    pub at: i64,
    /// What it came to.
    pub result: MatchResult,
}

/// What a match that linked nothing came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchResult {
    /// The offline database knows nothing by the series' names.
    None,
    /// The offline database has several equally good entries.
    Ambiguous,
    /// A title search on `AniList` found nothing by the series' names.
    SearchNone,
    /// A title search found several equally good anime.
    SearchAmbiguous,
    /// A title search's request failed.
    SearchFailed,
}

impl MatchResult {
    const ALL: [Self; 5] = [Self::None, Self::Ambiguous, Self::SearchNone, Self::SearchAmbiguous, Self::SearchFailed];

    /// How it is stored.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Ambiguous => "ambiguous",
            Self::SearchNone => "search-none",
            Self::SearchAmbiguous => "search-ambiguous",
            Self::SearchFailed => "search-failed",
        }
    }

    /// The result stored as `s`, if it is one this version knows.
    fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|r| r.as_str() == s)
    }

    /// A title search's answer (as opposed to the offline database's).
    pub const fn is_search(self) -> bool {
        matches!(self, Self::SearchNone | Self::SearchAmbiguous | Self::SearchFailed)
    }

    /// Several candidates, from the database or a title search.
    pub const fn is_ambiguous(self) -> bool {
        matches!(self, Self::Ambiguous | Self::SearchAmbiguous)
    }
}

/// "`AniList` was asked about this id": when, and whether it knew the id.
/// Decides when to ask again (see [`crate::meta::inbox_lookups`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnilistCheck {
    /// When it was asked (unix seconds).
    pub checked_at: i64,
    /// `AniList` returned the id.
    pub found: bool,
}

/// A finished scan of one root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanRecord {
    /// When the results were stored.
    pub finished_at: i64,
    /// Every directory could be listed.
    pub complete: bool,
    /// Counts.
    pub stats: ScanStats,
}

/// The last attempt to scan a root (see [`Db::set_scan_tried`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanTry {
    /// When it started.
    pub at: i64,
    /// The root could not be reached (unmounted, say).
    pub offline: bool,
    /// What was scanned (see [`crate::app::scan_fingerprint`]): a scan of
    /// another folder, or with other settings, says nothing about this one.
    pub fingerprint: String,
}

/// What [`Db::changes`] reports: equal values mean nothing new was stored.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Changes {
    /// Scans that changed something.
    pub files: Option<String>,
    /// Metadata updates.
    pub meta: Option<String>,
}

/// Outcome of applying a root scan.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanStats {
    /// Files seen in this scan.
    pub seen: usize,
    /// Files never seen before.
    pub new: usize,
    /// Previously present files now missing.
    pub gone: usize,
}

/// Handle to the cache database.
pub struct Db {
    conn: Connection,
    rebuilt: bool,
}

impl Db {
    /// Open or create the database at `path`.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path).with_context(|| format!("opening {path}", path = path.display()))?;
        Self::init(conn)
    }

    /// In-memory database (tests).
    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(mut conn: Connection) -> Result<Self> {
        // Wait for other connections (a background scan) before anything else.
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        let stale = user_version(&conn)? != SCHEMA_VERSION;
        // Only a new or outdated cache is written to: opening a current one
        // (every command, every background scan) takes no write lock.
        let mut rebuilt = false;
        if stale {
            // Persistent, so set once with the schema (and not inside a transaction).
            conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get::<_, String>(0))?;
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            // Read again under the lock: another process may have just done this.
            let version = user_version(&tx)?;
            if version != SCHEMA_VERSION {
                let upgrade_from = UPGRADABLE.contains(&version).then_some(version);
                if upgrade_from.is_none() {
                    let existing: i64 =
                        tx.query_row("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table'", [], |r| r.get(0))?;
                    rebuilt = existing > 0;
                    for t in TABLES {
                        tx.execute_batch(&format!("DROP TABLE IF EXISTS {t};"))?;
                    }
                }
                tx.execute_batch(SCHEMA)?;
                if let Some(v) = upgrade_from {
                    upgrade(&tx, v)?;
                }
                tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            }
            tx.commit()?;
        }
        Ok(Self { conn, rebuilt })
    }

    /// True if an older cache was dropped on open (a rescan refills it).
    pub fn was_rebuilt(&self) -> bool {
        self.rebuilt
    }

    /// Record the files found by a complete scan of `root`.
    ///
    /// When `complete` is false (some directories failed to list) nothing is
    /// marked as gone. An empty scan of a root that used to have files leaves
    /// existing rows alone unless [`trust_empty_scan`] says the root was really
    /// emptied. `base` is the directory that was walked and `mounted` whether
    /// it is a mount point now; both are remembered on every non-empty scan.
    pub fn apply_scan(
        &mut self,
        root: &str,
        files: &[ScannedFile],
        started_at: i64,
        complete: bool,
        base: &Path,
        mounted: Option<bool>,
    ) -> Result<ScanStats> {
        let tx = self.conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        // Each scan gets an id; files remember the last scan that saw them, so
        // "not seen by this scan" works even for two scans in the same second.
        tx.execute(
            "INSERT INTO scans(root, started_at, finished_at, complete) VALUES (?1, ?2, ?2, ?3)",
            params![root, started_at, complete],
        )?;
        let scan = tx.last_insert_rowid();
        let mut stats = ScanStats { seen: files.len(), ..ScanStats::default() };
        // New files are the rows the upserts add (an upsert of a known path
        // updates it in place).
        let count = |tx: &rusqlite::Transaction<'_>| -> Result<usize> {
            Ok(from_sql(tx.query_row("SELECT COUNT(*) FROM files", [], |r| r.get::<_, i64>(0))?))
        };
        let before = count(&tx)?;
        let present = |tx: &rusqlite::Transaction<'_>| -> Result<i64> {
            Ok(tx.query_row("SELECT COUNT(*) FROM files WHERE root = ?1 AND present = 1", [root], |r| r.get(0))?)
        };
        let present_before = present(&tx)?;
        {
            let mut upsert = tx.prepare_cached(
                "INSERT INTO files(path, root, rel, size, mtime, present, first_seen, last_scan)
                 VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, ?7)
                 ON CONFLICT(path) DO UPDATE SET root = ?2, rel = ?3, size = ?4, mtime = ?5, present = 1, last_scan = ?7",
            )?;
            for f in files {
                upsert.execute(params![
                    path_bytes(&f.path),
                    root,
                    path_bytes(&f.rel),
                    to_sql(f.size),
                    f.mtime,
                    started_at,
                    scan
                ])?;
            }
        }
        stats.new = count(&tx)? - before;
        // An empty scan of a root that had files is checked against the mount
        // state recorded at its last scan that found files (which records it).
        let key = format!("{ROOT_MOUNTED}{root}");
        let looks_unmounted = if files.is_empty() {
            let previously: i64 =
                tx.query_row("SELECT COUNT(*) FROM files WHERE root = ?1 AND present = 1", [root], |r| r.get(0))?;
            previously > 0 && !trust_empty_scan(get_kv(&tx, &key)?.and_then(|v| recorded_mount(&v, base)), mounted)
        } else {
            if let Some(m) = mounted {
                write_kv(&tx, &key, &mount_record(m, base))?;
            }
            false
        };
        if complete && !looks_unmounted {
            stats.gone = tx.execute(
                "UPDATE files SET present = 0 WHERE root = ?1 AND present = 1 AND last_scan < ?2",
                params![root, scan],
            )?;
        }
        // Files that came back (a remounted drive) are neither new nor gone.
        if stats.new > 0 || stats.gone > 0 || present(&tx)? != present_before {
            count_write(&tx, FILE_CHANGES)?;
        }
        tx.execute(
            "UPDATE scans SET finished_at = ?2, seen = ?3, new = ?4, gone = ?5 WHERE id = ?1",
            params![scan, crate::events::now(), to_sql(stats.seen), to_sql(stats.new), to_sql(stats.gone)],
        )?;
        tx.commit()?;
        Ok(stats)
    }

    /// All indexed files, present or not.
    pub fn files(&self) -> Result<Vec<FileRow>> {
        let mut st = self
            .conn
            .prepare_cached("SELECT path, root, rel, size, mtime, present, first_seen FROM files ORDER BY path")?;
        let rows = st.query_map([], |r| {
            Ok(FileRow {
                path: bytes_path(r.get(0)?),
                root: r.get(1)?,
                rel: bytes_path(r.get(2)?),
                size: from_sql(r.get(3)?),
                mtime: r.get(4)?,
                present: r.get::<_, i64>(5)? != 0,
                first_seen: r.get(6)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    }

    /// Forget files and mount records of roots that are no longer configured.
    /// Returns the number of files forgotten.
    pub fn retain_roots(&self, roots: &[&str]) -> Result<usize> {
        let all: Vec<String> = {
            let mut st = self.conn.prepare("SELECT DISTINCT root FROM files")?;
            st.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        };
        let mut n = 0;
        for r in all.iter().filter(|r| !roots.contains(&r.as_str())) {
            n += self.conn.execute("DELETE FROM files WHERE root = ?1", [r])?;
        }
        for prefix in ROOT_KEYS {
            let keys: Vec<String> = {
                let mut st = self.conn.prepare("SELECT k FROM kv WHERE substr(k, 1, ?2) = ?1")?;
                st.query_map(params![prefix, to_sql(prefix.len())], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
            };
            for k in keys.iter().filter(|k| !roots.contains(&&k[prefix.len()..])) {
                self.conn.execute("DELETE FROM kv WHERE k = ?1", [k])?;
            }
        }
        Ok(n)
    }

    /// Remember an attempt to scan `root` (see [`ScanTry`]). A root that
    /// became reachable or unreachable counts as a change (see [`Db::changes`]).
    pub fn set_scan_tried(&self, root: &str, tried: &ScanTry) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        let was = scan_tried(&tx, root)?;
        let state = if tried.offline { "offline" } else { "ok" };
        let value = format!("{at}:{state}:{fp}", at = tried.at, fp = tried.fingerprint);
        write_kv(&tx, &format!("{SCAN_TRIED}{root}"), &value)?;
        if was.is_none_or(|w| w.offline != tried.offline) {
            count_write(&tx, FILE_CHANGES)?;
        }
        Ok(tx.commit()?)
    }

    /// The last attempt to scan `root` (see [`Db::set_scan_tried`]).
    pub fn scan_tried(&self, root: &str) -> Result<Option<ScanTry>> {
        scan_tried(&self.conn, root)
    }

    /// Roots with files present in the index.
    pub fn indexed_roots(&self) -> Result<Vec<String>> {
        let mut st = self.conn.prepare_cached("SELECT DISTINCT root FROM files WHERE present = 1 ORDER BY root")?;
        Ok(st.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?)
    }

    /// Something that changes whenever a scan changes which files are
    /// present, or metadata is written, by this process or another one:
    /// compare two to know whether to reload.
    pub fn changes(&self) -> Result<Changes> {
        Ok(Changes { files: get_kv(&self.conn, FILE_CHANGES)?, meta: get_kv(&self.conn, META_SYNCS)? })
    }

    /// The most recent scan of `root`.
    pub fn last_scan(&self, root: &str) -> Result<Option<ScanRecord>> {
        Ok(self
            .conn
            .query_row(
                "SELECT finished_at, complete, seen, new, gone FROM scans WHERE root = ?1 ORDER BY id DESC LIMIT 1",
                [root],
                |r| {
                    Ok(ScanRecord {
                        finished_at: r.get(0)?,
                        complete: r.get(1)?,
                        stats: ScanStats {
                            seen: from_sql(r.get(2)?),
                            new: from_sql(r.get(3)?),
                            gone: from_sql(r.get(4)?),
                        },
                    })
                },
            )
            .optional()?)
    }

    /// Read a key/value setting.
    pub fn get_kv(&self, k: &str) -> Result<Option<String>> {
        get_kv(&self.conn, k)
    }

    /// Write a key/value setting.
    pub fn set_kv(&self, k: &str, v: &str) -> Result<()> {
        write_kv(&self.conn, k, v)
    }

    /// Store the outcome of matching in one transaction, as [`Db::store_sync`]
    /// does.
    #[cfg(test)]
    pub fn put_match_results(&self, rows: &[SeriesMeta], attempts: &[MatchAttempt]) -> Result<()> {
        self.store_sync(rows, attempts, None, &[], &[], &[], 0)
    }

    /// Store everything a metadata sync produced (links, "no match" attempts,
    /// the offline database version, prequel lists, which `AniList` ids were
    /// asked about and found) in one transaction, so a failure part-way (e.g.
    /// the database busy during a scan) leaves no half state. `ts` is when the
    /// prequels and checks were fetched.
    ///
    /// A row replaces its series' attempt; an attempt drops its series' row
    /// ("nothing to link").
    #[expect(clippy::too_many_arguments, reason = "all a sync produced, written in one transaction")]
    pub fn store_sync(
        &self,
        rows: &[SeriesMeta],
        attempts: &[MatchAttempt],
        db_version: Option<&str>,
        prequels: &[(u64, Vec<u64>)],
        sequels: &[(u64, Vec<u64>)],
        checks: &[(u64, bool)],
        ts: i64,
    ) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        {
            // A link (or fresh data for one) replaces "nothing to link", and
            // "nothing to link" drops a link that no longer matches.
            for m in rows {
                delete_attempt(&tx, &m.series)?;
            }
            for a in attempts {
                delete_meta_row(&tx, &a.series)?;
            }
            let mut put = tx.prepare_cached(
                "INSERT OR REPLACE INTO match_attempts(series, db_version, at, result) VALUES (?1, ?2, ?3, ?4)",
            )?;
            for a in attempts {
                put.execute(params![a.series, a.db_version, a.at, a.result.as_str()])?;
            }
        }
        write_metas(&tx, rows)?;
        write_relations(&tx, "prequels", prequels, ts)?;
        write_relations(&tx, "sequels", sequels, ts)?;
        {
            let mut put = tx.prepare_cached(
                "INSERT OR REPLACE INTO anilist_checks(anilist, checked_at, found) VALUES (?1, ?2, ?3)",
            )?;
            for &(id, found) in checks {
                put.execute(params![to_sql(id), ts, found])?;
            }
        }
        if let Some(v) = db_version {
            write_kv(&tx, OFFLINE_DB_VERSION, v)?;
        }
        count_meta_write(&tx)?;
        tx.commit()?;
        Ok(())
    }

    /// All known prequel lists, by `AniList` id.
    pub fn prequels(&self) -> Result<std::collections::HashMap<u64, Vec<u64>>> {
        let mut st = self.conn.prepare_cached("SELECT anilist, prequels FROM prequels")?;
        let rows = st.query_map([], |r| {
            Ok((from_sql(r.get(0)?), serde_json::from_str(&r.get::<_, String>(1)?).unwrap_or_default()))
        })?;
        rows.collect::<rusqlite::Result<_>>().map_err(Into::into)
    }

    /// All known sequel lists, by `AniList` id, with when they were fetched.
    pub fn sequels(&self) -> Result<std::collections::HashMap<u64, (Vec<u64>, i64)>> {
        let mut st = self.conn.prepare_cached("SELECT anilist, sequels, fetched_at FROM sequels")?;
        let rows = st.query_map([], |r| {
            let ids = serde_json::from_str(&r.get::<_, String>(1)?).unwrap_or_default();
            Ok((from_sql(r.get(0)?), (ids, r.get(2)?)))
        })?;
        rows.collect::<rusqlite::Result<_>>().map_err(Into::into)
    }

    /// When each `AniList` id was last asked about, and whether it was found.
    pub fn anilist_checks(&self) -> Result<std::collections::HashMap<u64, AnilistCheck>> {
        let mut st = self.conn.prepare_cached("SELECT anilist, checked_at, found FROM anilist_checks")?;
        let rows =
            st.query_map([], |r| Ok((from_sql(r.get(0)?), AnilistCheck { checked_at: r.get(1)?, found: r.get(2)? })))?;
        rows.collect::<rusqlite::Result<_>>().map_err(Into::into)
    }

    /// Run `f` with all its reads seeing one state of the database (a read
    /// transaction), so another process writing in between can't mix two.
    pub fn snapshot<T>(&self, f: impl FnOnce(&Self) -> Result<T>) -> Result<T> {
        let _read = self.conn.unchecked_transaction()?;
        f(self)
    }

    /// All recorded match attempts (but those with a result this version
    /// doesn't know, written by a newer one: the series is matched again).
    pub fn match_attempts(&self) -> Result<Vec<MatchAttempt>> {
        let mut st = self.conn.prepare_cached("SELECT series, db_version, at, result FROM match_attempts")?;
        let rows = st.query_map([], |r| {
            let (series, db_version, at, result): (String, String, i64, String) =
                (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?);
            Ok(MatchResult::parse(&result).map(|result| MatchAttempt { series, db_version, at, result }))
        })?;
        Ok(rows.filter_map(Result::transpose).collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Insert or replace cached metadata rows, in one transaction.
    pub fn put_metas(&self, rows: &[SeriesMeta]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        write_metas(&tx, rows)?;
        count_meta_write(&tx)?;
        tx.commit()?;
        Ok(())
    }

    /// Move metadata cached under old series keys to their new keys, in one
    /// transaction (`old → new` pairs, see [`crate::library::Library::legacy_keys`]).
    ///
    /// A row already under the new key wins; the old row is deleted either
    /// way, so nothing stays behind for a later sync to miss. "No match"
    /// attempts under an old key are dropped rather than moved: the new key is
    /// a different title, so it gets matched again; a row moved to the new key
    /// replaces that key's attempt, as a row written there would. (A row a
    /// title search made keeps its mark, [`SeriesMeta::search_linked`].)
    ///
    /// Pairs with no rows under the old key are looked up with reads only, and
    /// when no pair has any, no write transaction is started at all (callers
    /// pick the pairs from rows they loaded earlier, which another process may
    /// have moved since). Returns how many old keys had rows.
    pub fn rekey_legacy<'a>(&self, pairs: impl IntoIterator<Item = (&'a str, &'a str)>) -> Result<usize> {
        let pairs: Vec<(&str, &str)> = {
            let mut has_rows = self.conn.prepare_cached(
                "SELECT EXISTS(SELECT 1 FROM series_meta WHERE series = ?1)
                     OR EXISTS(SELECT 1 FROM match_attempts WHERE series = ?1)",
            )?;
            let mut keep = Vec::new();
            for (old, new) in pairs {
                if has_rows.query_row([old], |r| r.get::<_, bool>(0))? {
                    keep.push((old, new));
                }
            }
            keep
        };
        if pairs.is_empty() {
            return Ok(0);
        }
        let tx = self.conn.unchecked_transaction()?;
        {
            let mut rename = tx.prepare_cached("UPDATE OR IGNORE series_meta SET series = ?2 WHERE series = ?1")?;
            for &(old, new) in &pairs {
                // A row moved to the new key replaces its attempt, as a row
                // written there would (see `store_sync`).
                if rename.execute([old, new])? > 0 {
                    delete_attempt(&tx, new)?;
                }
                delete_meta_row(&tx, old)?;
                delete_attempt(&tx, old)?;
            }
        }
        count_meta_write(&tx)?;
        tx.commit()?;
        Ok(pairs.len())
    }

    /// Remove cached metadata for a series.
    pub fn delete_meta(&self, series: &str) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        delete_meta_row(&tx, series)?;
        count_meta_write(&tx)?;
        Ok(tx.commit()?)
    }

    /// All cached metadata.
    pub fn all_meta(&self) -> Result<Vec<SeriesMeta>> {
        let mut st = self.conn.prepare_cached(
            "SELECT series, anilist, title, episodes, status, format, year, next_ep, next_airing, refreshed_at, search_linked,
                    score, popularity, titles, hashtags, studios, season
             FROM series_meta",
        )?;
        let rows = st.query_map([], |r| {
            Ok(SeriesMeta {
                series: r.get(0)?,
                anilist: r.get::<_, Option<i64>>(1)?.map(from_sql),
                title: r.get(2)?,
                episodes: r.get(3)?,
                status: r.get(4)?,
                format: r.get(5)?,
                year: r.get(6)?,
                next_ep: r.get(7)?,
                next_airing: r.get(8)?,
                refreshed_at: r.get(9)?,
                search_linked: r.get(10)?,
                score: r.get(11)?,
                popularity: r.get(12)?,
                titles: r.get::<_, Option<String>>(13)?.map(|j| json_list(&j)),
                hashtags: r.get::<_, Option<String>>(14)?.map(|j| json_list(&j)).unwrap_or_default(),
                studios: r.get::<_, Option<String>>(15)?.map(|j| json_list(&j)).unwrap_or_default(),
                season: r.get(16)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    }
}

fn user_version(conn: &Connection) -> Result<i32> {
    Ok(conn.pragma_query_value(None, "user_version", |r| r.get(0))?)
}

/// Fill what [`SCHEMA`] just added to a cache of an `UPGRADABLE` `version`.
fn upgrade(conn: &Connection, version: i32) -> Result<()> {
    if version <= 4 {
        // Ids with a prequel list were asked about: don't ask again right away.
        // (Version 4 also stored an empty list for ids `AniList` didn't know;
        // those count as found and are simply asked again later.)
        conn.execute(
            "INSERT OR IGNORE INTO anilist_checks(anilist, checked_at, found) SELECT anilist, fetched_at, 1 FROM prequels",
            [],
        )?;
    }
    if version <= 5 {
        add_meta_column(conn, "search_linked", "INTEGER NOT NULL DEFAULT 0")?;
    }
    if version <= 6 {
        add_meta_column(conn, "score", "INTEGER")?;
        add_meta_column(conn, "popularity", "INTEGER")?;
    }
    if version <= 8 {
        for name in ["titles", "hashtags", "studios", "season"] {
            add_meta_column(conn, name, "TEXT")?;
        }
    }
    Ok(())
}

/// A JSON list of strings from the cache; unreadable reads as empty.
fn json_list(json: &str) -> Vec<String> {
    serde_json::from_str(json).unwrap_or_default()
}

/// Add a column to `series_meta` unless it is there already.
fn add_meta_column(conn: &Connection, name: &str, decl: &str) -> Result<()> {
    let has: bool =
        conn.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('series_meta') WHERE name = ?1)", [name], |r| {
            r.get(0)
        })?;
    if !has {
        conn.execute_batch(&format!("ALTER TABLE series_meta ADD COLUMN {name} {decl};"))?;
    }
    Ok(())
}

// Writers shared by the single-purpose methods and `store_sync`; they run
// inside whatever transaction the caller holds.

fn delete_meta_row(conn: &Connection, series: &str) -> Result<()> {
    conn.prepare_cached("DELETE FROM series_meta WHERE series = ?1")?.execute([series])?;
    Ok(())
}

fn delete_attempt(conn: &Connection, series: &str) -> Result<()> {
    conn.prepare_cached("DELETE FROM match_attempts WHERE series = ?1")?.execute([series])?;
    Ok(())
}

/// Count a metadata write (see [`META_SYNCS`]).
fn count_meta_write(conn: &Connection) -> Result<()> {
    count_write(conn, META_SYNCS)
}

/// Add one to the counter under `kv` key `k`.
fn count_write(conn: &Connection, k: &str) -> Result<()> {
    conn.execute("INSERT INTO kv(k, v) VALUES (?1, '1') ON CONFLICT(k) DO UPDATE SET v = CAST(v AS INTEGER) + 1", [k])?;
    Ok(())
}

fn scan_tried(conn: &Connection, root: &str) -> Result<Option<ScanTry>> {
    Ok(get_kv(conn, &format!("{SCAN_TRIED}{root}"))?.and_then(|v| {
        let (at, rest) = v.split_once(':')?;
        // Older records have no fingerprint: they match nothing.
        let (state, fingerprint) = rest.split_once(':').unwrap_or((rest, ""));
        Some(ScanTry { at: at.parse().ok()?, offline: state == "offline", fingerprint: fingerprint.into() })
    }))
}

fn get_kv(conn: &Connection, k: &str) -> Result<Option<String>> {
    Ok(conn.query_row("SELECT v FROM kv WHERE k = ?1", [k], |r| r.get(0)).optional()?)
}

fn write_kv(conn: &Connection, k: &str, v: &str) -> Result<()> {
    conn.execute("INSERT INTO kv(k, v) VALUES (?1, ?2) ON CONFLICT(k) DO UPDATE SET v = ?2", params![k, v])?;
    Ok(())
}

fn write_metas(conn: &Connection, rows: &[SeriesMeta]) -> Result<()> {
    let mut put = conn.prepare_cached(
        "INSERT OR REPLACE INTO series_meta(series, anilist, title, episodes, status, format, year, next_ep, next_airing, refreshed_at, search_linked,
                                            score, popularity, titles, hashtags, studios, season)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
    )?;
    for m in rows {
        put.execute(params![
            m.series,
            m.anilist.map(to_sql),
            m.title,
            m.episodes,
            m.status,
            m.format,
            m.year,
            m.next_ep,
            m.next_airing,
            m.refreshed_at,
            m.search_linked,
            m.score,
            m.popularity,
            m.titles.as_ref().map(serde_json::to_string).transpose()?,
            serde_json::to_string(&m.hashtags)?,
            serde_json::to_string(&m.studios)?,
            m.season
        ])?;
    }
    Ok(())
}

/// Write `AniList` relation lists into `table` (`prequels` or `sequels`,
/// whose list column has the table's name).
fn write_relations(conn: &Connection, table: &str, rows: &[(u64, Vec<u64>)], ts: i64) -> Result<()> {
    let mut put = conn
        .prepare_cached(&format!("INSERT OR REPLACE INTO {table}(anilist, {table}, fetched_at) VALUES (?1, ?2, ?3)"))?;
    for (id, pre) in rows {
        put.execute(params![to_sql(*id), serde_json::to_string(pre)?, ts])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The root directory the test scans walk.
    fn m() -> &'static Path {
        Path::new("/m")
    }

    fn f(rel: &str) -> ScannedFile {
        ScannedFile { path: PathBuf::from("/m").join(rel), rel: rel.into(), size: 1, mtime: 0 }
    }

    #[test]
    fn scan_lifecycle() {
        let mut db = Db::open_in_memory().unwrap();
        let s = db.apply_scan("dl", &[f("a.mkv"), f("b.mkv")], 10, true, m(), None).unwrap();
        assert_eq!(s, ScanStats { seen: 2, new: 2, gone: 0 });
        let s = db.apply_scan("dl", &[f("b.mkv"), f("c.mkv")], 20, true, m(), None).unwrap();
        assert_eq!(s, ScanStats { seen: 2, new: 1, gone: 1 });
        let files = db.files().unwrap();
        assert_eq!(files.len(), 3);
        assert!(!files.iter().find(|r| r.rel == Path::new("a.mkv")).unwrap().present);
        assert_eq!(files.iter().find(|r| r.rel == Path::new("b.mkv")).unwrap().first_seen, 10);
        let last = db.last_scan("dl").unwrap().unwrap();
        assert!(last.complete);
        assert_eq!(last.stats, ScanStats { seen: 2, new: 1, gone: 1 });
        db.apply_scan("dl", &[f("c.mkv")], 30, false, m(), None).unwrap();
        assert!(!db.last_scan("dl").unwrap().unwrap().complete);
        assert_eq!(db.last_scan("other").unwrap(), None);
    }

    #[test]
    fn scans_in_the_same_second_still_detect_removals() {
        let mut db = Db::open_in_memory().unwrap();
        db.apply_scan("dl", &[f("a.mkv"), f("b.mkv")], 10, true, m(), None).unwrap();
        let s = db.apply_scan("dl", &[f("a.mkv")], 10, true, m(), None).unwrap();
        assert_eq!(s.gone, 1);
    }

    /// Scan `dl` with `a.mkv` while it has the mount state `was`, then scan it
    /// empty with the state `now`; report whether the file is still present.
    fn present_after_empty_scan(was: Option<bool>, now: Option<bool>) -> bool {
        let mut db = Db::open_in_memory().unwrap();
        db.apply_scan("dl", &[f("a.mkv")], 10, true, m(), was).unwrap();
        let s = db.apply_scan("dl", &[], 20, true, m(), now).unwrap();
        let present = db.files().unwrap()[0].present;
        assert_eq!(s.gone, usize::from(!present));
        present
    }

    #[test]
    fn emptied_plain_folder_marks_files_gone() {
        assert!(!present_after_empty_scan(Some(false), Some(false)));
    }

    #[test]
    fn mount_that_disappeared_is_ignored() {
        assert!(present_after_empty_scan(Some(true), Some(false)));
        assert!(present_after_empty_scan(Some(true), None));
    }

    #[test]
    fn empty_mount_that_is_still_mounted_is_trusted() {
        assert!(!present_after_empty_scan(Some(true), Some(true)));
    }

    #[test]
    fn empty_scan_with_unknown_mount_state_is_ignored() {
        assert!(present_after_empty_scan(None, None));
        assert!(present_after_empty_scan(Some(false), None));
        assert!(present_after_empty_scan(Some(true), None));
        // No record and a mount now, or a plain folder that has become a
        // mount: maybe an automounter's empty placeholder.
        assert!(present_after_empty_scan(None, Some(true)));
        assert!(present_after_empty_scan(Some(false), Some(true)));
    }

    #[test]
    fn empty_plain_folder_without_a_record_is_trusted() {
        // An index from before mount states were recorded: ghost files clear.
        assert!(!present_after_empty_scan(None, Some(false)));
    }

    #[test]
    fn mount_records_from_older_versions_count_as_no_record() {
        let mut db = Db::open_in_memory().unwrap();
        db.apply_scan("dl", &[f("a.mkv")], 10, true, m(), None).unwrap();
        db.set_kv("root_mounted:dl", "1").unwrap();
        // Without a path the old "was a mount" record is ignored, so an empty
        // plain folder is trusted like any root with no record.
        assert_eq!(db.apply_scan("dl", &[], 20, true, m(), Some(false)).unwrap().gone, 1);
    }

    #[test]
    fn mount_record_of_another_path_is_ignored() {
        let mut db = Db::open_in_memory().unwrap();
        db.apply_scan("dl", &[f("a.mkv")], 10, true, Path::new("/old"), Some(true)).unwrap();
        // The root now points elsewhere; "/old was a mount" says nothing about
        // "/m", which is a mount now: untrusted with no record.
        assert_eq!(db.apply_scan("dl", &[], 20, true, m(), Some(true)).unwrap().gone, 0);
        // The same record does apply to the path it was made for: a mount
        // that is still mounted and empty.
        assert_eq!(db.apply_scan("dl", &[], 30, true, Path::new("/old"), Some(true)).unwrap().gone, 1);
    }

    #[test]
    fn recorded_mount_parses_state_and_path() {
        assert_eq!(recorded_mount("1:/mnt/a", Path::new("/mnt/a")), Some(true));
        assert_eq!(recorded_mount("0:/mnt/a", Path::new("/mnt/a")), Some(false));
        assert_eq!(recorded_mount("1:/mnt/a", Path::new("/mnt/b")), None);
        assert_eq!(recorded_mount("1", Path::new("/mnt/a")), None);
        assert_eq!(recorded_mount("1:/x:y", Path::new("/x:y")), Some(true));
    }

    #[test]
    fn mount_state_is_remembered_per_root_and_only_by_non_empty_scans() {
        let mut db = Db::open_in_memory().unwrap();
        db.apply_scan("dl", &[f("a.mkv")], 10, true, m(), Some(true)).unwrap();
        db.apply_scan("anime", &[f("b.mkv")], 10, true, m(), Some(false)).unwrap();
        assert_eq!(db.get_kv("root_mounted:dl").unwrap().as_deref(), Some("1:/m"));
        assert_eq!(db.get_kv("root_mounted:anime").unwrap().as_deref(), Some("0:/m"));
        // An ignored empty scan doesn't overwrite what was recorded.
        db.apply_scan("dl", &[], 20, true, m(), Some(false)).unwrap();
        assert_eq!(db.get_kv("root_mounted:dl").unwrap().as_deref(), Some("1:/m"));
        // A failed empty scan of an unmounted drive, then the drive returns.
        let s = db.apply_scan("dl", &[f("a.mkv")], 30, true, m(), Some(true)).unwrap();
        assert_eq!((s.new, s.gone), (0, 0));
    }

    #[test]
    fn incomplete_scan_marks_nothing_gone() {
        let mut db = Db::open_in_memory().unwrap();
        db.apply_scan("dl", &[f("a.mkv"), f("b.mkv")], 10, true, m(), None).unwrap();
        let s = db.apply_scan("dl", &[f("a.mkv")], 20, false, m(), None).unwrap();
        assert_eq!(s.gone, 0);
    }

    #[test]
    fn roots_are_independent_and_prunable() {
        let mut db = Db::open_in_memory().unwrap();
        db.apply_scan("dl", &[f("a.mkv")], 10, true, m(), None).unwrap();
        db.apply_scan("anime", &[f("x/b.mkv")], 10, true, m(), None).unwrap();
        db.apply_scan("dl", &[f("c.mkv")], 20, true, m(), None).unwrap();
        assert!(db.files().unwrap().iter().find(|r| r.root == "anime").unwrap().present);
        assert_eq!(db.retain_roots(&["dl"]).unwrap(), 1);
        assert!(db.files().unwrap().iter().all(|r| r.root == "dl"));
    }

    #[test]
    fn removed_roots_lose_their_mount_records() {
        let mut db = Db::open_in_memory().unwrap();
        db.apply_scan("dl", &[f("a.mkv")], 10, true, m(), Some(true)).unwrap();
        db.apply_scan("anime", &[f("x/b.mkv")], 10, true, m(), Some(false)).unwrap();
        db.set_kv("offline_db_version", "v1").unwrap();
        db.retain_roots(&["dl"]).unwrap();
        assert_eq!(db.get_kv("root_mounted:anime").unwrap(), None);
        assert_eq!(db.get_kv("root_mounted:dl").unwrap().as_deref(), Some("1:/m"));
        assert_eq!(db.get_kv("offline_db_version").unwrap().as_deref(), Some("v1"));
        // A record without files left (all removed by hand) is pruned too.
        db.set_kv("root_mounted:gone", "0:/g").unwrap();
        db.retain_roots(&["dl"]).unwrap();
        assert_eq!(db.get_kv("root_mounted:gone").unwrap(), None);
    }

    #[test]
    fn scan_attempts_are_kept_per_root_and_pruned_with_it() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.scan_tried("dl").unwrap(), None);
        let dl = ScanTry { at: 10, offline: false, fingerprint: "f1".into() };
        let anime = ScanTry { at: 20, offline: true, fingerprint: "f2".into() };
        db.set_scan_tried("dl", &dl).unwrap();
        db.set_scan_tried("anime", &anime).unwrap();
        assert_eq!(db.scan_tried("dl").unwrap(), Some(dl));
        assert_eq!(db.scan_tried("anime").unwrap(), Some(anime));
        // A record from before fingerprints matches none.
        db.set_kv("scan_tried:old", "5:ok").unwrap();
        assert_eq!(db.scan_tried("old").unwrap(), Some(ScanTry { at: 5, offline: false, fingerprint: String::new() }));
        db.retain_roots(&["dl"]).unwrap();
        assert_eq!(db.scan_tried("anime").unwrap(), None);
        assert!(db.scan_tried("dl").unwrap().is_some());
    }

    /// Scans that change which files are present, roots becoming
    /// (un)reachable and stored metadata each change what [`Db::changes`]
    /// reports, also when made through another connection; a scan that finds
    /// the same files, or the same state again, doesn't.
    #[test]
    fn changes_follow_scans_attempts_and_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let mut writer = Db::open(&path).unwrap();
        let reader = Db::open(&path).unwrap();
        let start = reader.changes().unwrap();
        writer.apply_scan("dl", &[f("a.mkv")], 10, true, m(), None).unwrap();
        let scanned = reader.changes().unwrap();
        assert_ne!(scanned.files, start.files);
        assert_eq!(scanned.meta, start.meta);
        writer.apply_scan("dl", &[f("a.mkv")], 11, true, m(), None).unwrap();
        assert_eq!(reader.changes().unwrap(), scanned, "nothing new");
        let tried = |at, offline| ScanTry { at, offline, fingerprint: String::new() };
        writer.set_scan_tried("dl", &tried(10, true)).unwrap();
        let tried_offline = reader.changes().unwrap();
        assert_ne!(tried_offline.files, scanned.files);
        writer.set_scan_tried("dl", &tried(20, true)).unwrap();
        assert_eq!(reader.changes().unwrap(), tried_offline, "still offline");
        writer.set_scan_tried("dl", &tried(30, false)).unwrap();
        let tried = reader.changes().unwrap();
        assert_ne!(tried.files, tried_offline.files, "back");
        // Gone, then back (a remounted drive): both are changes.
        writer.apply_scan("dl", &[f("b.mkv")], 40, true, m(), None).unwrap();
        let gone = reader.changes().unwrap();
        assert_ne!(gone.files, tried.files);
        writer.apply_scan("dl", &[f("a.mkv"), f("b.mkv")], 50, true, m(), None).unwrap();
        assert_ne!(reader.changes().unwrap().files, gone.files);
        let tried = reader.changes().unwrap();
        writer.store_sync(&[], &[], None, &[], &[], &[], 20).unwrap();
        let synced = reader.changes().unwrap();
        assert_eq!(synced.files, tried.files);
        assert_ne!(synced.meta, tried.meta);
        writer.store_sync(&[], &[], None, &[], &[], &[], 20).unwrap();
        assert_ne!(reader.changes().unwrap().meta, synced.meta, "two syncs in the same second");
    }

    #[test]
    fn meta_roundtrip() {
        let db = Db::open_in_memory().unwrap();
        let m = SeriesMeta {
            series: "one piece".into(),
            anilist: Some(21),
            episodes: None,
            status: Some("RELEASING".into()),
            next_ep: Some(1181),
            next_airing: Some(1_800_000_000),
            refreshed_at: Some(5),
            ..SeriesMeta::default()
        };
        db.put_metas(std::slice::from_ref(&m)).unwrap();
        assert_eq!(db.all_meta().unwrap(), vec![m.clone()]);
        // Titles: none fetched yet, fetched but none, and some; lists keep their order.
        let fetched_none = SeriesMeta { titles: Some(Vec::new()), ..m.clone() };
        db.put_metas(std::slice::from_ref(&fetched_none)).unwrap();
        assert_eq!(db.all_meta().unwrap(), vec![fetched_none]);
        let full = SeriesMeta {
            titles: Some(vec![
                "Kusuriya no Hitorigoto".into(),
                "The Apothecary Diaries".into(),
                "薬屋のひとりごと".into(),
            ]),
            hashtags: vec!["#薬屋のひとりごと".into(), "#kusuriya".into()],
            studios: vec!["Studio A".into(), "Studio B".into()],
            season: Some("FALL".into()),
            ..m
        };
        db.put_metas(std::slice::from_ref(&full)).unwrap();
        assert_eq!(db.all_meta().unwrap(), vec![full]);
        db.delete_meta("one piece").unwrap();
        assert!(db.all_meta().unwrap().is_empty());
    }

    #[test]
    fn old_caches_are_dropped_and_rebuilt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        {
            let mut db = Db::open(&path).unwrap();
            assert!(!db.was_rebuilt(), "a brand new file is not a rebuild");
            db.apply_scan("dl", &[f("a.mkv")], 10, true, m(), None).unwrap();
            db.conn.pragma_update(None, "user_version", 3).unwrap();
        }
        let db = Db::open(&path).unwrap();
        assert!(db.was_rebuilt());
        assert!(db.files().unwrap().is_empty());
        assert!(!Db::open(&path).unwrap().was_rebuilt());
    }

    /// A version 4 cache is upgraded in place: files (and their first-seen
    /// times) survive, and ids with prequel lists count as checked.
    #[test]
    fn version_4_caches_are_upgraded_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        {
            let mut db = Db::open(&path).unwrap();
            db.apply_scan("dl", &[f("a.mkv")], 10, true, m(), None).unwrap();
            db.store_sync(&[], &[], None, &[(7, vec![3])], &[(7, vec![9])], &[], 42).unwrap();
            db.conn.execute_batch("DROP TABLE anilist_checks;").unwrap();
            db.conn.pragma_update(None, "user_version", 4).unwrap();
        }
        let db = Db::open(&path).unwrap();
        assert!(!db.was_rebuilt());
        assert_eq!(db.files().unwrap()[0].first_seen, 10);
        assert_eq!(db.prequels().unwrap()[&7], vec![3]);
        assert_eq!(db.sequels().unwrap()[&7], (vec![9], 42));
        assert_eq!(db.anilist_checks().unwrap()[&7], AnilistCheck { checked_at: 42, found: true });
        assert_eq!(user_version(&db.conn).unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn anilist_checks_roundtrip() {
        let db = Db::open_in_memory().unwrap();
        db.store_sync(&[], &[], None, &[(1, vec![])], &[], &[(1, true), (2, false)], 9).unwrap();
        db.store_sync(&[], &[], None, &[], &[], &[(2, true)], 11).unwrap();
        let checks = db.anilist_checks().unwrap();
        assert_eq!(checks[&1], AnilistCheck { checked_at: 9, found: true });
        assert_eq!(checks[&2], AnilistCheck { checked_at: 11, found: true });
    }

    #[test]
    fn match_attempts_are_replaced_by_links() {
        let db = Db::open_in_memory().unwrap();
        let attempt =
            |s: &str| MatchAttempt { series: s.into(), db_version: "v1".into(), at: 1, result: MatchResult::None };
        db.put_match_results(&[], &[attempt("a"), attempt("b")]).unwrap();
        assert_eq!(db.match_attempts().unwrap().len(), 2);
        let row = SeriesMeta { series: "a".into(), anilist: Some(1), refreshed_at: Some(2), ..SeriesMeta::default() };
        db.put_match_results(&[row], &[]).unwrap();
        assert_eq!(db.match_attempts().unwrap(), vec![attempt("b")]);
        assert_eq!(db.all_meta().unwrap().len(), 1);
    }

    /// The mark that a title search made a link is stored with the row, kept
    /// by a re-key, and goes with the row ("no match" drops both).
    #[test]
    fn search_links_are_marked_on_their_row() {
        let db = Db::open_in_memory().unwrap();
        let row = |series: &str| SeriesMeta {
            series: series.into(),
            anilist: Some(5),
            search_linked: true,
            ..SeriesMeta::default()
        };
        let attempt = |series: &str| MatchAttempt {
            series: series.into(),
            db_version: "v".into(),
            at: 1,
            result: MatchResult::None,
        };
        db.put_match_results(&[row("old")], &[]).unwrap();
        assert_eq!(db.all_meta().unwrap(), vec![row("old")]);
        db.put_match_results(&[], &[attempt("new")]).unwrap();
        assert_eq!(db.rekey_legacy([("old", "new")]).unwrap(), 1);
        assert_eq!(db.all_meta().unwrap(), vec![row("new")]);
        assert!(db.match_attempts().unwrap().is_empty(), "the moved row replaces the new key's attempt");
        // A row from elsewhere replaces it, mark and all.
        let other = SeriesMeta { series: "new".into(), anilist: Some(6), ..SeriesMeta::default() };
        db.put_match_results(std::slice::from_ref(&other), &[]).unwrap();
        assert_eq!(db.all_meta().unwrap(), vec![other]);
        db.put_match_results(&[], &[attempt("new")]).unwrap();
        assert!(db.all_meta().unwrap().is_empty());
    }

    /// A version 5 cache gets the `search_linked` column, and keeps its rows.
    #[test]
    fn version_5_caches_are_upgraded_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        {
            let db = Db::open(&path).unwrap();
            db.conn
                .execute_batch(
                    "DROP TABLE series_meta;
                     CREATE TABLE series_meta(series TEXT PRIMARY KEY, anilist INTEGER, title TEXT, episodes INTEGER,
                         status TEXT, format TEXT, year INTEGER, next_ep INTEGER, next_airing INTEGER, refreshed_at INTEGER);
                     INSERT INTO series_meta(series, anilist) VALUES ('a', 5);",
                )
                .unwrap();
            db.conn.pragma_update(None, "user_version", 5).unwrap();
        }
        let db = Db::open(&path).unwrap();
        assert!(!db.was_rebuilt());
        let a = SeriesMeta { series: "a".into(), anilist: Some(5), ..SeriesMeta::default() };
        assert_eq!(db.all_meta().unwrap(), vec![a]);
        assert_eq!(user_version(&db.conn).unwrap(), SCHEMA_VERSION);
    }

    /// A version 6 cache gets the `score` and `popularity` columns, keeps its
    /// rows, and stores both from then on.
    #[test]
    fn version_6_caches_are_upgraded_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        {
            let db = Db::open(&path).unwrap();
            db.conn
                .execute_batch(
                    "DROP TABLE series_meta;
                     CREATE TABLE series_meta(series TEXT PRIMARY KEY, anilist INTEGER, title TEXT, episodes INTEGER,
                         status TEXT, format TEXT, year INTEGER, next_ep INTEGER, next_airing INTEGER, refreshed_at INTEGER,
                         search_linked INTEGER NOT NULL DEFAULT 0);
                     INSERT INTO series_meta(series, anilist, search_linked) VALUES ('a', 5, 1);",
                )
                .unwrap();
            db.conn.pragma_update(None, "user_version", 6).unwrap();
        }
        let db = Db::open(&path).unwrap();
        assert!(!db.was_rebuilt());
        let a = SeriesMeta { series: "a".into(), anilist: Some(5), search_linked: true, ..SeriesMeta::default() };
        assert_eq!(db.all_meta().unwrap(), vec![a]);
        assert_eq!(user_version(&db.conn).unwrap(), SCHEMA_VERSION);
        let b = SeriesMeta { series: "b".into(), score: Some(78), popularity: Some(123_456), ..SeriesMeta::default() };
        db.put_match_results(std::slice::from_ref(&b), &[]).unwrap();
        assert!(db.all_meta().unwrap().contains(&b));
    }

    /// A version 8 cache gets the `titles`, `hashtags`, `studios` and `season`
    /// columns and keeps its rows, without titles (so they are refreshed once).
    #[test]
    fn version_8_caches_are_upgraded_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        {
            let db = Db::open(&path).unwrap();
            db.conn
                .execute_batch(
                    "DROP TABLE series_meta;
                     CREATE TABLE series_meta(series TEXT PRIMARY KEY, anilist INTEGER, title TEXT, episodes INTEGER,
                         status TEXT, format TEXT, year INTEGER, next_ep INTEGER, next_airing INTEGER, refreshed_at INTEGER,
                         search_linked INTEGER NOT NULL DEFAULT 0, score INTEGER, popularity INTEGER);
                     INSERT INTO series_meta(series, anilist, refreshed_at, score) VALUES ('a', 5, 9, 80);",
                )
                .unwrap();
            db.conn.pragma_update(None, "user_version", 8).unwrap();
        }
        let db = Db::open(&path).unwrap();
        assert!(!db.was_rebuilt());
        let a = SeriesMeta {
            series: "a".into(),
            anilist: Some(5),
            refreshed_at: Some(9),
            score: Some(80),
            ..SeriesMeta::default()
        };
        assert_eq!(db.all_meta().unwrap(), vec![a]);
        assert_eq!(user_version(&db.conn).unwrap(), SCHEMA_VERSION);
        let b = SeriesMeta {
            series: "b".into(),
            titles: Some(vec!["B".into()]),
            studios: vec!["Studio B".into()],
            ..SeriesMeta::default()
        };
        db.put_metas(std::slice::from_ref(&b)).unwrap();
        assert!(db.all_meta().unwrap().contains(&b));
    }

    /// Results are stored by name; one this version doesn't know (from a newer
    /// one) is left out, so the series is matched again.
    #[test]
    fn match_results_round_trip() {
        let db = Db::open_in_memory().unwrap();
        let attempts: Vec<MatchAttempt> = MatchResult::ALL
            .into_iter()
            .map(|result| MatchAttempt { series: result.as_str().into(), db_version: "v".into(), at: 1, result })
            .collect();
        db.put_match_results(&[], &attempts).unwrap();
        db.conn.execute("INSERT INTO match_attempts VALUES ('future', 'v', 1, 'something-new')", []).unwrap();
        let mut back = db.match_attempts().unwrap();
        back.sort_by_key(|a| MatchResult::ALL.iter().position(|r| *r == a.result));
        assert_eq!(back, attempts);
        // `ALL` lists every result (adding one fails to compile here until it is listed).
        let index = |r: MatchResult| match r {
            MatchResult::None => 0,
            MatchResult::Ambiguous => 1,
            MatchResult::SearchNone => 2,
            MatchResult::SearchAmbiguous => 3,
            MatchResult::SearchFailed => 4,
        };
        assert!(MatchResult::ALL.into_iter().map(index).eq(0..5));
    }

    #[test]
    fn non_utf8_file_names_round_trip() {
        use std::os::unix::ffi::OsStrExt;
        let mut db = Db::open_in_memory().unwrap();
        let name = std::ffi::OsStr::from_bytes(b"Show - 01 \xe9t\xe9.mkv");
        let other = std::ffi::OsStr::from_bytes(b"Show - 01 \xe8t\xe8.mkv");
        let file =
            |n: &std::ffi::OsStr| ScannedFile { path: Path::new("/m").join(n), rel: n.into(), size: 1, mtime: 0 };
        db.apply_scan("dl", &[file(name), file(other)], 10, true, m(), None).unwrap();
        let files = db.files().unwrap();
        assert_eq!(files.len(), 2, "names differing only in invalid bytes stay distinct");
        assert!(files.iter().any(|f| f.rel.as_os_str() == name && f.path == Path::new("/m").join(name)));
    }

    /// If writing the links fails, the attempt rows deleted earlier in the same
    /// store come back: nothing is half-written.
    #[test]
    fn match_results_are_stored_atomically() {
        let db = Db::open_in_memory().unwrap();
        let attempt = MatchAttempt { series: "a".into(), db_version: "v1".into(), at: 1, result: MatchResult::None };
        db.put_match_results(&[], std::slice::from_ref(&attempt)).unwrap();
        db.conn
            .execute_batch(
                "CREATE TRIGGER fail BEFORE INSERT ON series_meta BEGIN SELECT RAISE(ABORT, 'disk full'); END;",
            )
            .unwrap();
        let row = SeriesMeta { series: "a".into(), anilist: Some(1), ..SeriesMeta::default() };
        assert!(db.put_match_results(&[row], &[]).is_err());
        assert_eq!(db.match_attempts().unwrap(), vec![attempt], "the attempt survives the failed store");
    }

    #[test]
    fn kv() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.get_kv("x").unwrap(), None);
        db.set_kv("x", "1").unwrap();
        db.set_kv("x", "2").unwrap();
        assert_eq!(db.get_kv("x").unwrap().as_deref(), Some("2"));
    }
}
