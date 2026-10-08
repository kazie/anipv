//! Application context shared by the CLI and the TUI.

use std::borrow::Cow;

use anyhow::{Context, Result, bail};

use crate::config::{Config, Paths};
use std::collections::HashSet;

use crate::events::{CachedLog, EventBody, EventLog, Recorded};
use crate::index::Db;
use crate::index::FileRow;
use crate::index::classify::{Classified, FileClass, classify_row, classify_rows};
use crate::index::db::{OFFLINE_DB_VERSION, ScanStats, SeriesMeta};
use crate::index::scan::scan_root;
use crate::library::{Library, Series};
use crate::model::{ItemKey, SeriesStatus, WatchState};

/// Start of the message for a failed event log write.
pub const WRITE_FAILED: &str = "could not write event log";

/// Loaded configuration, paths, event log and cache.
pub struct Ctx {
    /// User configuration.
    pub cfg: Config,
    /// Resolved paths.
    pub paths: Paths,
    /// Event log for this device.
    pub log: EventLog,
    /// Cache database.
    pub db: Db,
}

/// Index rows loaded from the cache database: the files, each classified
/// when the index is made (so rebuilding the library does not parse every
/// file again), and the cached metadata.
#[derive(Debug, Clone, Default)]
pub struct Index {
    /// All indexed files.
    files: Vec<FileRow>,
    /// The classification of each of `files`, in the same order.
    classes: Vec<FileClass>,
    /// Cached metadata, which changes without the files changing.
    pub meta: MetaCache,
}

/// The metadata part of an [`Index`].
#[derive(Debug, Clone, Default)]
pub struct MetaCache {
    /// Cached metadata rows.
    pub rows: Vec<SeriesMeta>,
    /// "No match" results for the current offline database version.
    pub attempts: Vec<crate::index::db::MatchAttempt>,
    /// `AniList` prequel ids by `AniList` id.
    pub prequels: std::collections::HashMap<u64, Vec<u64>>,
    /// When `AniList` was last asked about each id, and whether it knew it.
    pub anilist_checks: std::collections::HashMap<u64, crate::index::db::AnilistCheck>,
}

impl Index {
    /// An index of `files`, classified under `cfg` (the configuration the
    /// library is then built with), and `meta`.
    pub fn new(cfg: &Config, files: Vec<FileRow>, meta: MetaCache) -> Self {
        Self { classes: classify_rows(cfg, &files), files, meta }
    }

    /// [`Index::new`], reusing the classifications of `old` (made under the
    /// same `cfg`) for files it has too: after a scan, only new files are parsed.
    fn with_known(cfg: &Config, files: Vec<FileRow>, meta: MetaCache, old: &Self) -> Self {
        let known: std::collections::HashMap<(&str, &std::path::Path), &FileClass> =
            old.files.iter().zip(&old.classes).map(|(f, c)| ((f.root.as_str(), f.rel.as_path()), c)).collect();
        let kinds: std::collections::HashMap<&str, crate::config::RootKind> =
            cfg.roots.iter().map(|r| (r.name.as_str(), r.kind)).collect();
        let classes = files
            .iter()
            .map(|f| match known.get(&(f.root.as_str(), f.rel.as_path())) {
                Some(&c) => c.clone(),
                None => classify_row(cfg, &kinds, f),
            })
            .collect();
        Self { files, classes, meta }
    }

    /// All indexed files.
    pub fn files(&self) -> &[FileRow] {
        &self.files
    }

    /// Each file under a configured root with its classification.
    pub fn classified(&self) -> impl Iterator<Item = (&FileRow, crate::config::RootKind, &Classified)> {
        self.files.iter().zip(&self.classes).filter_map(|(row, c)| c.as_ref().map(|(kind, c)| (row, *kind, c)))
    }

    /// Move metadata rows cached under legacy keys to the current ones, as
    /// [`Db::rekey_legacy`] does in the database.
    fn rekey_legacy(&mut self, pairs: &[(&str, &str)]) {
        for &(old, new) in pairs {
            let taken = self.meta.rows.iter().any(|m| m.series == new);
            self.meta.rows.retain_mut(|m| {
                if m.series != old {
                    return true;
                }
                m.series = new.to_string();
                !taken
            });
            self.meta.attempts.retain(|a| a.series != old);
        }
    }
}

/// Outcome of scanning one root.
#[derive(Debug)]
pub struct RootScan {
    /// Root name.
    pub root: String,
    /// Counts, if the scan ran.
    pub stats: Result<ScanStats>,
    /// The root itself could not be scanned (not a directory we can reach:
    /// missing, unmounted or inaccessible), as opposed to failing to record
    /// the scan in the database.
    pub offline: bool,
    /// Directories that failed to list.
    pub errors: Vec<String>,
}

impl Ctx {
    /// Load everything from the default locations.
    pub fn load() -> Result<Self> {
        let paths = Paths::resolve(None)?;
        let cfg = Config::load(&paths.config_file)?;
        Self::with_config(cfg)
    }

    /// Build a context from an explicit config.
    pub fn with_config(cfg: Config) -> Result<Self> {
        let paths = Paths::resolve(Some(&cfg))?;
        Self::open(cfg, paths)
    }

    /// Build a context from explicit config and paths (tests, demos).
    pub fn open(cfg: Config, paths: Paths) -> Result<Self> {
        paths.ensure()?;
        let log = EventLog::open(&paths.events_dir, cfg.device_name())?;
        let db = Db::open(&paths.db_file)?;
        Ok(Self { cfg, paths, log, db })
    }

    /// Build the library from cache + events.
    ///
    /// This also makes the mapping of keys from before kana voiced marks were
    /// kept permanent (see `docs/events.md`): it records an alias from each
    /// mapped legacy key that has history to its current key (once; see
    /// [`Library::legacy_aliases`]) and moves metadata cached under legacy
    /// keys to the current ones (see [`Db::rekey_legacy`]).
    ///
    /// Any [`Library::warnings`] are printed to stderr: only the CLI builds the
    /// library this way (the TUI uses [`Ctx::library_completed`] and shows
    /// them itself).
    #[expect(clippy::print_stderr, reason = "CLI warnings go to the terminal, not to command output")]
    pub fn library(&self) -> Result<Library> {
        let lib = self.migrated_library(&mut self.index()?, &mut self.log.load_cached()?)?;
        for w in &lib.warnings {
            eprintln!("warning: {w}");
        }
        Ok(lib)
    }

    /// The cached index (files and metadata), which only changes on scans and
    /// metadata updates. The files are classified here, once per load.
    pub fn index(&self) -> Result<Index> {
        Ok(Index::new(&self.cfg, self.db.files()?, self.meta_cache()?))
    }

    /// [`Ctx::index`] after a scan: the files that `old` has keep their
    /// classification, so only new ones are parsed. The metadata is read again
    /// (another anipv process may have updated it meanwhile).
    pub fn index_from(&self, old: &Index) -> Result<Index> {
        Ok(Index::with_known(&self.cfg, self.db.files()?, self.meta_cache()?, old))
    }

    /// The metadata part of [`Ctx::index`], for after metadata updates.
    pub fn meta_cache(&self) -> Result<MetaCache> {
        // Attempts made against an older offline database don't count: a newer
        // database may know the series, so it gets matched again.
        let version = self.db.get_kv(OFFLINE_DB_VERSION)?;
        let attempts =
            self.db.match_attempts()?.into_iter().filter(|a| Some(&a.db_version) == version.as_ref()).collect();
        Ok(MetaCache {
            rows: self.db.all_meta()?,
            attempts,
            prequels: self.db.prequels()?,
            anilist_checks: self.db.anilist_checks()?,
        })
    }

    /// [`Ctx::migrated_library`], then
    /// [`Ctx::auto_complete`] on it. When anything was completed, the library
    /// is rebuilt from the same events plus the recorded ones. Whatever is
    /// recorded is merged into `log`. Returns the library and the completed
    /// titles; a failure to record them is a [`Library::warnings`] entry.
    pub fn library_completed(&self, index: &mut Index, log: &mut CachedLog) -> Result<(Library, Vec<String>)> {
        let mut lib = self.migrated_library(index, log)?;
        let (recorded, done) = match self.auto_complete(&lib) {
            Ok(x) => x,
            Err(e) => {
                lib.warnings.push(format!("{WRITE_FAILED}: {e:#}"));
                return Ok((lib, Vec::new()));
            }
        };
        if recorded.events.is_empty() {
            return Ok((lib, done));
        }
        self.log.merge(log, recorded);
        let mut rebuilt = Library::build(&self.cfg, log.events(), index);
        rebuilt.warnings = lib.warnings;
        Ok((rebuilt, done))
    }

    /// Build the library from an already loaded index and the events in `log`
    /// (read with [`EventLog::load_cached`] and kept up to date by the
    /// caller), and migrate legacy keys (like [`Ctx::library`]), merging the
    /// aliases it records into `log`.
    ///
    /// Rows moved in the database are moved in `index` too.
    ///
    /// Moving cache rows touches the database only for legacy keys that have
    /// rows in `index` (and writes only when they still have them). If that,
    /// or recording the aliases, fails (e.g. the database is locked), the
    /// library is returned anyway with a message in [`Library::warnings`]: it
    /// already reads those keys and rows as the current ones, and the next
    /// build tries again.
    pub fn migrated_library(&self, index: &mut Index, log: &mut CachedLog) -> Result<Library> {
        let mut lib = Library::build(&self.cfg, log.events(), index);
        let legacy = lib.legacy_keys();
        let mut warnings = Vec::new();
        if !legacy.is_empty() {
            // Under the legacy mapping these aliases change nothing, so the
            // library just built stays as it is, and a failed write (e.g. a
            // read-only events folder) only means the next build tries again.
            match self.log.append(lib.legacy_aliases()) {
                Ok(recorded) => self.log.merge(log, recorded),
                Err(e) => warnings.push(format!("could not record the current series keys in the event log: {e:#}")),
            }
            // Only keys with rows in the index can have any to move, so
            // rebuilds after the migration don't query for each key.
            let cached: HashSet<&str> = index
                .meta
                .rows
                .iter()
                .map(|m| m.series.as_str())
                .chain(index.meta.attempts.iter().map(|a| a.series.as_str()))
                .collect();
            let pairs: Vec<(&str, &str)> = legacy
                .iter()
                .filter(|(old, _)| cached.contains(old.as_str()))
                .map(|(old, new)| (old.as_str(), new.as_str()))
                .collect();
            if !pairs.is_empty() {
                match self.db.rekey_legacy(pairs.iter().copied()) {
                    // The library already reads them as moved.
                    Ok(_) => index.rekey_legacy(&pairs),
                    Err(e) => {
                        warnings.push(format!("could not move cached metadata to the current series keys: {e:#}"));
                    }
                }
            }
        }
        lib.warnings.extend(warnings);
        Ok(lib)
    }

    /// Split merged-away keys off their series again (with their legacy
    /// spellings, see [`Library::unmerge_events`]), without reading the
    /// event log again: `lib` knows which spellings it names. Returns the
    /// recorded events.
    pub fn unmerge<'a>(&self, lib: &Library, keys: impl IntoIterator<Item = &'a str>) -> Result<Recorded> {
        self.record(keys.into_iter().flat_map(|k| lib.unmerge_events(k)))
    }

    /// Store everything a [`crate::meta::sync`] produced in the cache.
    pub fn store_sync(&self, res: &crate::meta::SyncResult) -> Result<()> {
        let ts = crate::events::now();
        self.db.store_sync(&res.rows, &res.attempts, res.db_version.as_deref(), &res.prequels, &res.checked, ts)
    }

    /// Scan all configured roots (or only `only`), calling `progress(root, count)`.
    pub fn scan(&mut self, only: Option<&str>, progress: &(dyn Fn(&str, usize) + Sync)) -> Result<Vec<RootScan>> {
        scan_all(&self.cfg, &mut self.db, only, progress)
    }

    /// Record events for this device. Returns them as recorded.
    pub fn record(&self, bodies: impl IntoIterator<Item = EventBody>) -> Result<Recorded> {
        self.log.append(bodies)
    }

    /// Link a series to an `AniList` id by hand, or unlink it (`None`), and
    /// forget its cached metadata so the next sync fetches it fresh.
    ///
    /// `series` is resolved through `lib` first, so a key held from before a
    /// merge or reload links the series it is now. Returns the recorded events.
    pub fn link(&self, lib: &Library, series: &str, anilist: Option<u64>) -> Result<Recorded> {
        let series = lib.resolve(series).to_string();
        // The cache goes first: it is disposable, so an error here leaves the
        // link unrecorded (and the caller's "not linked" accurate) rather than
        // reporting a failure for a link that is already in the log.
        // Rows cached under a key from before kana voiced marks were kept go
        // too, but only keys that stand for this series: any other is another
        // series' own row.
        for key in lib.spellings(&series) {
            self.db.delete_meta(key)?;
        }
        self.record([match anilist {
            Some(id) => EventBody::Meta { series, anilist: Some(id), episodes: None },
            None => EventBody::Unlink { series },
        }])
    }

    /// Set a series' status. Returns the recorded event.
    ///
    /// A status other than completed chosen for a series that is already
    /// finished and fully watched (in `lib`, which is what the user is
    /// looking at, so actions need not read anything again) is kept: it is
    /// not auto-completed (see [`Ctx::auto_complete`]) until the status
    /// changes again, so the series can be followed for a rewatch.
    pub fn set_status(
        &self,
        lib: &Library,
        series: &str,
        status: SeriesStatus,
        note: Option<String>,
    ) -> Result<Recorded> {
        let keep = status != SeriesStatus::Completed && lib.get(series).is_some_and(Series::is_done);
        self.record([EventBody::SeriesStatus { series: series.into(), status, note, auto: false, keep }])
    }

    /// Mark followed series that are finished and fully watched as completed.
    /// Returns the recorded events and the series' titles.
    ///
    /// Due after anything that changes watch state, statuses or metadata: the
    /// TUI runs it on every reload ([`Ctx::library_completed`], which also
    /// returns the library with them completed), the CLI after every command
    /// that writes.
    pub fn auto_complete(&self, lib: &Library) -> Result<(Recorded, Vec<String>)> {
        let done: Vec<&crate::library::Series> = lib.series.iter().filter(|s| s.should_auto_complete()).collect();
        let recorded = self.log.append(done.iter().map(|s| EventBody::SeriesStatus {
            series: s.key.clone(),
            status: SeriesStatus::Completed,
            note: Some(format!("all {} episodes watched", s.whole_watched_count())),
            auto: true,
            keep: false,
        }))?;
        Ok((recorded, done.iter().map(|s| s.title.clone()).collect()))
    }

    /// Mark items of `s` watched (or unwatched), recording their file names.
    ///
    /// Items already in that state are left alone, so re-marking a range keeps
    /// when you actually watched them. Unwatching clears a resume point too,
    /// and skips items anipv has never heard of. An item named twice is
    /// recorded once. Returns the recorded events, one per item that changed.
    pub fn mark(&self, s: &Series, items: &[ItemKey], watched: bool) -> Result<Recorded> {
        let changes = |k: &ItemKey| match (s.item(k), watched) {
            (None, watched) => watched,
            (Some(i), true) => !i.state.is_watched(),
            (Some(i), false) => !matches!(i.state, WatchState::Unwatched),
        };
        let mut seen = HashSet::new();
        let items: Vec<&ItemKey> = items.iter().filter(|&k| seen.insert(k) && changes(k)).collect();
        self.record(items.iter().map(|&item| {
            let (series, item, file) =
                (s.key.clone(), item.clone(), s.item(item).and_then(super::library::Item::file_name));
            if watched {
                EventBody::Watched { series, item, file }
            } else {
                EventBody::Unwatched { series, item, file }
            }
        }))
    }
}

/// Scan configured roots into `db` (usable from a background thread with its own connection).
pub fn scan_all(
    cfg: &Config,
    db: &mut Db,
    only: Option<&str>,
    progress: &(dyn Fn(&str, usize) + Sync),
) -> Result<Vec<RootScan>> {
    if cfg.roots.is_empty() {
        bail!("no media roots configured; run `anipv init` first");
    }
    let names: Vec<&str> = cfg.roots.iter().map(|r| r.name.as_str()).collect();
    db.retain_roots(&names)?;
    let mut out = Vec::new();
    for root in &cfg.roots {
        if only.is_some_and(|o| !o.eq_ignore_ascii_case(&root.name)) {
            continue;
        }
        let ts = crate::events::now();
        let res = scan_root(cfg, root, &|n| progress(&root.name, n));
        let offline = res.is_err();
        let (stats, errors) = match res {
            Ok(r) => {
                let complete = r.complete();
                (db.apply_scan(&root.name, &r.files, ts, complete, &r.base, r.mounted), r.errors)
            }
            Err(e) => (Err(e), Vec::new()),
        };
        out.push(RootScan { root: root.name.clone(), stats, offline, errors });
    }
    if let (Some(o), true) = (only, out.is_empty()) {
        bail!("no root named {o:?} (roots: {})", names.join(", "));
    }
    Ok(out)
}

/// One entry in a play queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueEntry {
    /// Series key.
    pub series: String,
    /// Item to play.
    pub item: ItemKey,
    /// File chosen for it.
    pub file: std::path::PathBuf,
}

impl Ctx {
    /// mpv files for a queue, resuming started items.
    ///
    /// Each file is played once, even if the queue holds several of its
    /// episodes (multi-episode files): watching it marks all of them.
    pub fn play_files(&self, lib: &Library, queue: &[QueueEntry]) -> Vec<crate::mpv::PlayFile> {
        let mut seen = std::collections::HashSet::new();
        queue
            .iter()
            .filter(|q| seen.insert(&q.file))
            .map(|q| {
                let start = lib.get(&q.series).and_then(|s| s.item(&q.item)).and_then(|i| match i.state {
                    crate::model::WatchState::Started { pos, .. } => Some(pos),
                    _ => None,
                });
                crate::mpv::PlayFile { path: q.file.clone(), start }
            })
            .collect()
    }

    /// Fresh IPC socket path for a new mpv instance (its directory is created
    /// by [`Paths::ensure`]). When that path is too long for a Unix socket, it
    /// is in a private `anipv-<uid>` directory in the temporary directory instead.
    pub fn socket_path(&self) -> std::path::PathBuf {
        let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
        let name = format!("anipv-{}-{n}.sock", std::process::id());
        let path = self.paths.runtime_dir.join(&name);
        if fits_socket_path(&path) {
            return path;
        }
        short_socket_dir(&self.paths.runtime_dir).map_or(path, |dir| dir.join(name))
    }

    /// Record the result of a finished file; returns what was recorded
    /// (series, items, outcome, and the events).
    pub fn record_playback(
        &self,
        lib: &Library,
        path: &std::path::Path,
        pos: f64,
        dur: Option<f64>,
        eof: bool,
    ) -> Result<(String, Vec<ItemKey>, crate::mpv::Outcome, Recorded)> {
        use crate::mpv::{Outcome, outcome};
        let (series, items) = lib.identify(&self.cfg, path);
        let file = Some(crate::library::file_name_lossy(path)).filter(|f| !f.is_empty());
        let out = outcome(pos, dur, eof, self.cfg.watched_threshold);
        let bodies: Vec<EventBody> = match out {
            Outcome::Watched => items
                .iter()
                .map(|i| EventBody::Watched { series: series.clone(), item: i.clone(), file: file.clone() })
                .collect(),
            Outcome::Partial { pos, dur } => items
                .iter()
                .map(|i| EventBody::Progress { series: series.clone(), item: i.clone(), pos, dur, file: file.clone() })
                .collect(),
            Outcome::Nothing => Vec::new(),
        };
        let recorded = self.record(bodies)?;
        Ok((series, items, out, recorded))
    }
}

/// Queue entries for up to `n` new episodes of `s`, skipping items for which `skip` is true.
pub fn queue_new(s: &Series, n: usize, skip: impl Fn(&ItemKey) -> bool) -> Vec<QueueEntry> {
    s.queue_candidates(n, skip)
        .into_iter()
        .map(|(item, file)| QueueEntry { series: s.key.clone(), item, file })
        .collect()
}

/// Find a series by key, exact title, or fuzzy match on title/key/aliases.
pub fn find_series<'a>(lib: &'a Library, query: &str) -> Result<&'a Series> {
    if let Some(s) = lib.get(query) {
        return Ok(s);
    }
    let key = crate::identity::series_key(query);
    if let Some(s) = lib.get(&key) {
        return Ok(s);
    }
    let ranked = fuzzy_rank(lib.series.iter(), query, |s| s.search_text().into());
    let Some(&(_, best_score)) = ranked.first() else { bail!("no series matches {query:?}") };
    // Equally good matches: prefer a series you follow or have watched over,
    // say, a new season sitting untouched in the inbox.
    let tied: Vec<&Series> = ranked.iter().take_while(|(_, s)| *s == best_score).map(|(s, _)| *s).collect();
    let rank = |s: &Series| (s.status == crate::model::SeriesStatus::Following, s.engaged());
    let top = tied.iter().map(|s| rank(s)).max().unwrap_or_default();
    let best: Vec<&Series> = tied.into_iter().filter(|s| rank(s) == top).collect();
    if let [one] = best[..] {
        Ok(one)
    } else {
        // Name the candidates that actually tied, not just the top of the ranking.
        let names: Vec<_> = best.iter().take(5).map(|s| s.title.as_str()).collect();
        bail!("{query:?} is ambiguous: {}", names.join(", "))
    }
}

/// Rank items by fuzzy match score (best first); non-matches are dropped.
///
/// The matcher (and its scratch memory) is kept per thread between calls, so
/// ranking on every keystroke does not set one up each time.
pub fn fuzzy_rank<'a, T>(
    items: impl Iterator<Item = &'a T>,
    query: &str,
    text: impl Fn(&T) -> Cow<'_, str>,
) -> Vec<(&'a T, u32)>
where
    T: 'a,
{
    use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
    use nucleo_matcher::{Config as MConfig, Matcher, Utf32Str};
    thread_local! {
        // Taken out while in use, so a `text` that ranks something itself
        // just gets a fresh matcher instead of a borrow panic.
        static MATCHER: std::cell::Cell<Option<Matcher>> = const { std::cell::Cell::new(None) };
    }
    let mut matcher = MATCHER.take().unwrap_or_else(|| Matcher::new(MConfig::DEFAULT));
    let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
    let mut buf = Vec::new();
    let mut out: Vec<(&T, u32)> = items
        .filter_map(|it| {
            let t = text(it);
            pattern.score(Utf32Str::new(&t, &mut buf), &mut matcher).map(|s| (it, s))
        })
        .collect();
    MATCHER.set(Some(matcher));
    out.sort_by_key(|x| std::cmp::Reverse(x.1));
    out
}

/// Parse `"7"`, `"1-3"`, `"5,7"` into episode keys.
pub fn parse_episode_list(spec: &str) -> Result<Vec<ItemKey>> {
    let mut out = Vec::new();
    for part in spec.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        match part.split_once('-') {
            Some((a, b)) => {
                let a: u32 = a.trim().parse().context("bad range start")?;
                let b: u32 = b.trim().parse().context("bad range end")?;
                if b < a || b - a > 5000 || b > crate::model::EpNo::MAX {
                    bail!("bad range {part}");
                }
                out.extend((a..=b).map(|n| ItemKey::episode(crate::model::EpNo::new(n))));
            }
            None => out.push(ItemKey::episode(part.parse().map_err(anyhow::Error::msg)?)),
        }
    }
    // `1-3,2` names episode 2 once: no duplicate events, and counts stay right.
    let mut seen = HashSet::new();
    out.retain(|k| seen.insert(k.clone()));
    Ok(out)
}

/// Longest IPC socket path used as is: a Unix socket address holds 104 to 108
/// bytes depending on the platform.
const MAX_SOCKET_PATH: usize = 100;

/// Whether `path` is short enough for a Unix socket address.
fn fits_socket_path(path: &std::path::Path) -> bool {
    path.as_os_str().len() <= MAX_SOCKET_PATH
}

/// A short private directory for IPC sockets: `anipv-<uid>` in the temporary
/// directory, created with mode 0700. `None` if it can't be created or isn't
/// private (a symlink, another user's, not mode 0700).
fn short_socket_dir(runtime_dir: &std::path::Path) -> Option<std::path::PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt};
        let uid = std::fs::metadata(runtime_dir).ok()?.uid();
        let dir = std::env::temp_dir().join(format!("anipv-{uid}"));
        // Already existing is fine if it passes the checks below.
        let _ = std::fs::DirBuilder::new().mode(0o700).create(&dir);
        let m = std::fs::symlink_metadata(&dir).ok()?;
        (m.is_dir() && m.uid() == uid && m.mode() & 0o777 == 0o700).then_some(dir)
    }
    #[cfg(not(unix))]
    {
        let _ = runtime_dir;
        let dir = std::env::temp_dir().join(format!("anipv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok().map(|()| dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::EpNo;

    #[test]
    fn episode_lists() {
        let e = |n| ItemKey::episode(EpNo::new(n));
        assert_eq!(parse_episode_list("7").unwrap(), vec![e(7)]);
        assert_eq!(parse_episode_list("1-3, 5").unwrap(), vec![e(1), e(2), e(3), e(5)]);
        assert_eq!(parse_episode_list("1-3,2,1").unwrap(), vec![e(1), e(2), e(3)], "each episode once");
        assert_eq!(parse_episode_list("12.5").unwrap(), vec![ItemKey::episode("12.5".parse().unwrap())]);
        assert!(parse_episode_list("3-1").is_err());
        assert!(parse_episode_list("x").is_err());
        assert!(parse_episode_list("500000000").is_err());
        assert!(parse_episode_list("400000000-400000001").is_err());
    }

    #[test]
    fn opening_creates_all_directories() {
        let dir = tempfile::tempdir().unwrap();
        let mut paths = crate::demo::paths(dir.path());
        paths.runtime_dir = dir.path().join("not/yet/there");
        let ctx = Ctx::open(Config::default(), paths).unwrap();
        assert!(ctx.socket_path().parent().unwrap().is_dir());
        assert!(ctx.paths.cache_dir.is_dir());
    }

    /// Regression: a socket path too long for a Unix socket address (a deep
    /// `XDG_RUNTIME_DIR` or cache directory) moves to a short directory.
    #[test]
    fn long_socket_paths_are_not_used() {
        let p = |n: usize| std::path::PathBuf::from(format!("/{}", "x".repeat(n - 1)));
        assert!(fits_socket_path(&p(30)));
        assert!(fits_socket_path(&p(MAX_SOCKET_PATH)));
        assert!(!fits_socket_path(&p(MAX_SOCKET_PATH + 1)));
    }

    /// A scan's new index has the metadata written since the old one was
    /// read, by this process or another.
    #[test]
    fn index_from_reads_the_metadata_again() {
        use crate::index::db::MatchAttempt;
        let (_dir, ctx) = crate::demo::ctx();
        let old = ctx.index().unwrap();
        assert!(old.meta.attempts.is_empty());
        let attempt =
            MatchAttempt { series: "yuru camp".into(), db_version: "v1".into(), at: 1, result: "none".into() };
        ctx.db.put_match_results(&[], &[attempt]).unwrap();
        ctx.db.set_kv(OFFLINE_DB_VERSION, "v1").unwrap();
        let new = ctx.index_from(&old).unwrap();
        assert_eq!(new.meta.attempts.len(), 1);
        assert_eq!(new.files().len(), old.files().len());
    }

    #[test]
    fn attempts_from_an_older_offline_db_are_ignored() {
        use crate::index::db::MatchAttempt;
        let (_dir, ctx) = crate::demo::ctx();
        let attempt =
            MatchAttempt { series: "yuru camp".into(), db_version: "2026-07".into(), at: 1, result: "none".into() };
        ctx.db.put_match_results(&[], &[attempt]).unwrap();
        ctx.db.set_kv(OFFLINE_DB_VERSION, "2026-07").unwrap();
        assert_eq!(ctx.index().unwrap().meta.attempts.len(), 1);
        assert!(ctx.library().unwrap().get("yuru camp").unwrap().match_checked);
        ctx.db.set_kv(OFFLINE_DB_VERSION, "2026-10").unwrap();
        assert!(ctx.index().unwrap().meta.attempts.is_empty(), "a newer database re-matches");
    }

    #[test]
    fn finished_and_fully_watched_series_complete_once() {
        let (_dir, ctx) = crate::demo::ctx();
        // Yuru Camp: 12 episodes, finished airing (demo metadata).
        let lib = ctx.library().unwrap();
        let yc = lib.get("yuru camp").unwrap();
        ctx.set_status(&ctx.library().unwrap(), "yuru camp", SeriesStatus::Following, None).unwrap();
        ctx.mark(yc, &parse_episode_list("1-11").unwrap(), true).unwrap();
        assert!(ctx.auto_complete(&ctx.library().unwrap()).unwrap().1.is_empty(), "one episode left");

        let lib = ctx.library().unwrap();
        ctx.mark(lib.get("yuru camp").unwrap(), &parse_episode_list("12").unwrap(), true).unwrap();
        assert_eq!(ctx.auto_complete(&ctx.library().unwrap()).unwrap().1, vec!["Yuru Camp"]);
        let lib = ctx.library().unwrap();
        let yc = lib.get("yuru camp").unwrap();
        assert_eq!(yc.status, SeriesStatus::Completed);
        assert_eq!(yc.note.as_deref(), Some("all 12 episodes watched"));

        // The user puts it back to following: anipv doesn't override that.
        ctx.set_status(&ctx.library().unwrap(), "yuru camp", SeriesStatus::Following, None).unwrap();
        assert!(ctx.auto_complete(&ctx.library().unwrap()).unwrap().1.is_empty());
        assert_eq!(ctx.library().unwrap().get("yuru camp").unwrap().status, SeriesStatus::Following);
    }

    /// A status chosen once the series is finished and fully watched stays:
    /// it is not completed again, so it can be followed for a rewatch.
    #[test]
    fn status_chosen_on_a_finished_series_is_not_overridden() {
        let (_dir, ctx) = crate::demo::ctx();
        let lib = ctx.library().unwrap();
        ctx.mark(lib.get("yuru camp").unwrap(), &parse_episode_list("1-12").unwrap(), true).unwrap();
        for status in [SeriesStatus::Following, SeriesStatus::Paused, SeriesStatus::Following] {
            ctx.set_status(&ctx.library().unwrap(), "yuru camp", status, None).unwrap();
            assert!(ctx.auto_complete(&ctx.library().unwrap()).unwrap().1.is_empty());
            assert_eq!(ctx.library().unwrap().get("yuru camp").unwrap().status, status);
        }
        let events = ctx.log.load_all().unwrap().events;
        let kept = |e: &crate::events::Event| matches!(e.body, EventBody::SeriesStatus { keep: true, .. });
        assert_eq!(events.iter().filter(|e| kept(e)).count(), 3);
        // Choosing a status again while it is not finished is not "keeping": it
        // completes when it is finished (the series is back to following).
        let lib = ctx.library().unwrap();
        ctx.mark(lib.get("yuru camp").unwrap(), &parse_episode_list("12").unwrap(), false).unwrap();
        ctx.set_status(&ctx.library().unwrap(), "yuru camp", SeriesStatus::Following, None).unwrap();
        let lib = ctx.library().unwrap();
        ctx.mark(lib.get("yuru camp").unwrap(), &parse_episode_list("12").unwrap(), true).unwrap();
        assert_eq!(ctx.auto_complete(&ctx.library().unwrap()).unwrap().1, vec!["Yuru Camp"]);
        // Completing is not "keeping".
        ctx.set_status(&ctx.library().unwrap(), "yuru camp", SeriesStatus::Completed, None).unwrap();
        assert_eq!(ctx.log.load_all().unwrap().events.iter().filter(|e| kept(e)).count(), 3);
    }

    #[test]
    fn marking_an_item_twice_records_it_once() {
        let (_dir, ctx) = crate::demo::ctx();
        let ep = ItemKey::episode(crate::model::EpNo::new(11));
        ctx.mark(ctx.library().unwrap().get("yuru camp").unwrap(), std::slice::from_ref(&ep), false).unwrap();
        let before = ctx.log.load_all().unwrap().events.len();
        let lib = ctx.library().unwrap();
        assert_eq!(ctx.mark(lib.get("yuru camp").unwrap(), &[ep.clone(), ep], true).unwrap().events.len(), 1);
        assert_eq!(ctx.log.load_all().unwrap().events.len(), before + 1);
    }

    /// The TUI's reload: completing rebuilds from the events already loaded
    /// plus the recorded ones, the same library a fresh build gives.
    #[test]
    fn library_completed_rebuilds_with_the_completions() {
        let (_dir, ctx) = crate::demo::ctx();
        let lib = ctx.library().unwrap();
        ctx.set_status(&ctx.library().unwrap(), "yuru camp", SeriesStatus::Following, None).unwrap();
        ctx.mark(lib.get("yuru camp").unwrap(), &parse_episode_list("1-12").unwrap(), true).unwrap();
        let mut index = ctx.index().unwrap();
        let mut log = ctx.log.load_cached().unwrap();
        let (lib, done) = ctx.library_completed(&mut index, &mut log).unwrap();
        assert_eq!(done, vec!["Yuru Camp"]);
        assert_eq!(lib.get("yuru camp").unwrap().status, SeriesStatus::Completed);
        let fresh = ctx.migrated_library(&mut index, &mut ctx.log.load_cached().unwrap()).unwrap();
        let summary =
            |l: &Library| -> Vec<_> { l.series.iter().map(|s| (s.key.clone(), s.status, s.note.clone())).collect() };
        assert_eq!(summary(&lib), summary(&fresh));
        let (_, done) = ctx.library_completed(&mut index, &mut log).unwrap();
        assert!(done.is_empty(), "once");
    }

    /// The note counts episodes the way completing does: a watched `12.5`
    /// recap does not make it "all 13 episodes".
    #[test]
    fn auto_complete_note_counts_whole_episodes() {
        let (_dir, ctx) = crate::demo::ctx();
        let lib = ctx.library().unwrap();
        ctx.set_status(&ctx.library().unwrap(), "yuru camp", SeriesStatus::Following, None).unwrap();
        let mut eps = parse_episode_list("1-12").unwrap();
        eps.push(ItemKey::episode("12.5".parse().unwrap()));
        ctx.mark(lib.get("yuru camp").unwrap(), &eps, true).unwrap();
        let lib = ctx.library().unwrap();
        assert_eq!(lib.get("yuru camp").unwrap().watched_count(), 13);
        assert_eq!(ctx.auto_complete(&lib).unwrap().1, vec!["Yuru Camp"]);
        let note = ctx.library().unwrap().get("yuru camp").unwrap().note.clone();
        assert_eq!(note.as_deref(), Some("all 12 episodes watched"));
    }

    #[test]
    fn airing_or_unknown_length_series_are_not_completed() {
        let (_dir, ctx) = crate::demo::ctx();
        let lib = ctx.library().unwrap();
        // Frieren is still airing in the demo; One Piece has no known total.
        for key in ["sousou no frieren", "one piece"] {
            let s = lib.get(key).unwrap();
            let all: Vec<ItemKey> = s.episodes().map(|i| i.key.clone()).collect();
            ctx.mark(s, &all, true).unwrap();
        }
        assert!(ctx.auto_complete(&ctx.library().unwrap()).unwrap().1.is_empty());
    }

    #[test]
    fn a_file_is_played_once_even_if_queued_for_several_episodes() {
        let (_dir, ctx) = crate::demo::ctx();
        let lib = ctx.library().unwrap();
        let file = std::path::PathBuf::from("/m/Show - 001&002.mkv");
        let entry = |n| QueueEntry {
            series: "show".into(),
            item: ItemKey::episode(crate::model::EpNo::new(n)),
            file: file.clone(),
        };
        assert_eq!(ctx.play_files(&lib, &[entry(1), entry(2)]).len(), 1);
    }

    // ---- keys from before kana voiced marks were kept (`バカ` was `ハカ`) ----

    /// A demo context with `Anime/<name>/<name> - 01.mkv` on disk for each name.
    fn with_shows(dir: &std::path::Path, names: &[&str]) -> Ctx {
        let mut ctx = crate::demo::setup(dir, 0).unwrap();
        add_shows(&mut ctx, names);
        ctx
    }

    fn add_shows(ctx: &mut Ctx, names: &[&str]) {
        let anime = ctx.cfg.roots.iter().find(|r| r.name == "Anime").unwrap().resolved();
        for name in names {
            std::fs::create_dir_all(anime.join(name)).unwrap();
            std::fs::write(anime.join(format!("{name}/{name} - 01.mkv")), b"x").unwrap();
        }
        ctx.scan(None, &|_, _| {}).unwrap();
    }

    fn meta(series: &str, anilist: u64) -> crate::index::db::SeriesMeta {
        crate::index::db::SeriesMeta { series: series.into(), anilist: Some(anilist), ..Default::default() }
    }

    /// `バカ` on disk, with an episode watched under its legacy key `ハカ`.
    fn legacy_ctx() -> (tempfile::TempDir, Ctx) {
        let dir = tempfile::tempdir().unwrap();
        let ctx = with_shows(dir.path(), &["バカ"]);
        ctx.record([EventBody::watched("ハカ", ItemKey::episode(EpNo::new(1)))]).unwrap();
        (dir, ctx)
    }

    /// Cached metadata rows as `(series, anilist)`.
    fn meta_rows(ctx: &Ctx) -> Vec<(String, Option<u64>)> {
        ctx.db.all_meta().unwrap().into_iter().map(|m| (m.series, m.anilist)).collect()
    }

    /// True if no metadata row is cached under `ハカ`.
    fn no_legacy_row(ctx: &Ctx) -> bool {
        meta_rows(ctx).iter().all(|(s, _)| s != "ハカ")
    }

    fn legacy_aliases(ctx: &Ctx) -> Vec<EventBody> {
        let events = ctx.log.load_all().unwrap().events;
        events
            .into_iter()
            .map(|e| e.body)
            .filter(|b| matches!(b, EventBody::Alias { from, .. } if from == "ハカ"))
            .collect()
    }

    #[test]
    fn legacy_keys_are_made_permanent_once() {
        use crate::index::db::MatchAttempt;
        let (_dir, ctx) = legacy_ctx();
        let ep1 = ItemKey::episode(EpNo::new(1));
        let attempt = |s: &str| MatchAttempt { series: s.into(), db_version: "v".into(), at: 1, result: "none".into() };
        ctx.db.put_match_results(&[meta("ハカ", 1), meta("バカ", 2)], &[]).unwrap();
        ctx.db.put_match_results(&[], &[attempt("ハカ")]).unwrap();
        ctx.db.set_kv(OFFLINE_DB_VERSION, "v").unwrap();

        let lib = ctx.library().unwrap();
        assert!(lib.get("バカ").unwrap().item(&ep1).unwrap().state.is_watched());
        assert_eq!(legacy_aliases(&ctx), vec![EventBody::Alias { from: "ハカ".into(), to: "バカ".into() }]);
        // Cache rows moved: the row already under the new key wins, the old one is gone.
        assert!(meta_rows(&ctx).contains(&("バカ".into(), Some(2))));
        assert!(no_legacy_row(&ctx), "{:?}", meta_rows(&ctx));
        assert!(ctx.db.match_attempts().unwrap().iter().all(|a| a.series != "ハカ"), "dropped, not moved");
        assert!(!lib.get("バカ").unwrap().match_checked, "so the new key gets matched");

        // Idempotent across rebuilds, and a second device sharing the log
        // sees the alias instead of writing its own.
        for _ in 0..3 {
            ctx.library().unwrap();
        }
        let laptop = Ctx::open(Config { device: Some("laptop".into()), ..ctx.cfg.clone() }, ctx.paths.clone()).unwrap();
        laptop.library().unwrap();
        assert_eq!(legacy_aliases(&ctx).len(), 1);
        assert!(ctx.library().unwrap().get("バカ").unwrap().item(&ep1).unwrap().state.is_watched());

        // A row under the old key alone is moved to the new key.
        ctx.db.delete_meta("バカ").unwrap();
        ctx.db.put_metas(&[meta("ハカ", 3)]).unwrap();
        ctx.library().unwrap();
        assert!(meta_rows(&ctx).contains(&("バカ".into(), Some(3))) && no_legacy_row(&ctx), "{:?}", meta_rows(&ctx));
    }

    /// The rows moved in the database are moved in the index in memory too,
    /// so later rebuilds have nothing left to look up.
    #[test]
    fn migrating_moves_the_rows_in_memory_too() {
        let (_dir, ctx) = legacy_ctx();
        let sorted = |mut v: Vec<(String, Option<u64>)>| {
            v.sort();
            v
        };
        let rows = |index: &Index| sorted(index.meta.rows.iter().map(|m| (m.series.clone(), m.anilist)).collect());
        let baka = ("バカ".to_string(), Some(1));
        ctx.db.put_metas(&[meta("ハカ", 1)]).unwrap();
        let mut index = ctx.index().unwrap();
        ctx.migrated_library(&mut index, &mut ctx.log.load_cached().unwrap()).unwrap();
        assert_eq!(rows(&index), sorted(meta_rows(&ctx)));
        assert!(rows(&index).contains(&baka) && no_legacy_row(&ctx));

        // A row already under the new key wins.
        ctx.db.put_metas(&[meta("ハカ", 2)]).unwrap();
        let mut index = ctx.index().unwrap();
        ctx.migrated_library(&mut index, &mut ctx.log.load_cached().unwrap()).unwrap();
        assert_eq!(rows(&index), sorted(meta_rows(&ctx)));
        assert!(rows(&index).contains(&baka) && no_legacy_row(&ctx));
    }

    /// Only a root that cannot be scanned is offline: one that scans but whose
    /// result is not recorded is not.
    #[test]
    fn only_unreachable_roots_are_offline() {
        use crate::config::{Root, RootKind};
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("here")).unwrap();
        let cfg = Config {
            roots: vec![
                Root::test("here", dir.path().join("here"), RootKind::Archive),
                Root::test("gone", dir.path().join("gone"), RootKind::Archive),
            ],
            ..Config::default()
        };
        let mut db = Db::open_in_memory().unwrap();
        let scans = scan_all(&cfg, &mut db, None, &|_, _| {}).unwrap();
        let offline: Vec<(&str, bool, bool)> =
            scans.iter().map(|r| (r.root.as_str(), r.offline, r.stats.is_err())).collect();
        assert_eq!(offline, [("here", false, false), ("gone", true, true)]);
    }

    /// A rescan classifies only the files it did not know: the others keep
    /// their classification (here one made under other settings, to tell).
    #[test]
    fn rescans_reuse_known_classifications() {
        use crate::config::{Root, RootKind};
        use crate::model::ItemKind;
        let cfg = Config { roots: vec![Root::test("a", "/a", RootKind::Archive)], ..Config::default() };
        let extras = Config { extras_dirs: vec!["Show".into()], ..cfg.clone() };
        let f = |rel: &str| FileRow::test("a", rel);
        let old = Index::new(&extras, vec![f("Show/Show - 01.mkv")], MetaCache::default());
        let files = vec![f("Show/Show - 01.mkv"), f("Show/Show - 02.mkv"), f("Other/Other - 01.mkv")];
        let new = Index::with_known(&cfg, files, MetaCache::default(), &old);
        let kinds: Vec<ItemKind> = new.classified().map(|(_, _, c)| c.parsed.kind).collect();
        assert_eq!(kinds, [ItemKind::Extra, ItemKind::Episode, ItemKind::Episode]);
    }

    /// Once the cache rows are moved, rebuilding the library does not write to
    /// the database: it works while another connection holds the write lock.
    #[test]
    fn rebuilding_after_the_legacy_migration_does_not_write() {
        let (_dir, ctx) = legacy_ctx();
        ctx.db.put_metas(&[meta("ハカ", 1)]).unwrap();
        ctx.library().unwrap();
        assert!(no_legacy_row(&ctx), "migrated");

        let other = rusqlite::Connection::open(&ctx.paths.db_file).unwrap();
        other.execute_batch("BEGIN IMMEDIATE").unwrap();
        let start = std::time::Instant::now();
        let lib = ctx.library().unwrap();
        assert!(lib.warnings.is_empty(), "{:?}", lib.warnings);
        assert!(start.elapsed() < std::time::Duration::from_secs(2), "waited for the write lock");
        other.execute_batch("ROLLBACK").unwrap();
        assert_eq!(ctx.db.rekey_legacy([("ハカ", "バカ")]).unwrap(), 0, "nothing left to move");
    }

    /// A failing cache migration does not fail the library build: the library
    /// reads the old row as the new key's anyway, and the next build retries.
    #[test]
    fn a_failing_legacy_migration_still_builds_the_library() {
        let (_dir, ctx) = legacy_ctx();
        ctx.db.put_metas(&[meta("ハカ", 7)]).unwrap();
        let other = rusqlite::Connection::open(&ctx.paths.db_file).unwrap();
        other
            .execute_batch(
                "CREATE TRIGGER no_rekey BEFORE UPDATE ON series_meta BEGIN SELECT RAISE(ABORT, 'read-only'); END",
            )
            .unwrap();
        let lib = ctx.library().unwrap();
        assert_eq!(lib.warnings.len(), 1, "{:?}", lib.warnings);
        assert!(lib.warnings[0].contains("read-only"), "{:?}", lib.warnings);
        assert_eq!(lib.get("バカ").unwrap().anilist_id(), Some(7));
        assert!(!no_legacy_row(&ctx), "left for the next build");

        other.execute_batch("DROP TRIGGER no_rekey").unwrap();
        let lib = ctx.library().unwrap();
        assert!(lib.warnings.is_empty(), "{:?}", lib.warnings);
        assert!(no_legacy_row(&ctx));
    }

    #[test]
    fn a_later_lookalike_on_disk_does_not_steal_the_history() {
        let (_dir, mut ctx) = legacy_ctx();
        let ep1 = ItemKey::episode(EpNo::new(1));
        ctx.library().unwrap();
        add_shows(&mut ctx, &["ハカ"]);
        let lib = ctx.library().unwrap();
        let s = lib.get("バカ").unwrap();
        assert_eq!(s.key, "バカ");
        assert!(s.item(&ep1).unwrap().state.is_watched());
        assert_eq!(lib.get("ハカ").unwrap().key, "バカ", "merged in (the accepted trade-off)");
        assert_eq!(legacy_aliases(&ctx).len(), 1);

        // Unmerging it gives the lookalike its own series (with the old history,
        // which nothing tells apart any more), and no alias is written again.
        ctx.unmerge(&lib, ["ハカ"]).unwrap();
        let lib = ctx.library().unwrap();
        assert_eq!(lib.get("ハカ").unwrap().key, "ハカ");
        assert_eq!(legacy_aliases(&ctx).len(), 1);
    }

    #[test]
    fn unmerging_a_name_merged_under_its_legacy_key() {
        let (_dir, ctx) = legacy_ctx();
        ctx.record([EventBody::Alias { from: "ハカ".into(), to: "yuru camp".into() }]).unwrap();
        let lib = ctx.library().unwrap();
        assert_eq!(lib.get("バカ").unwrap().key, "yuru camp");
        assert_eq!(legacy_aliases(&ctx).len(), 1, "the user's merge is kept, nothing added");
        ctx.unmerge(&lib, ["バカ"]).unwrap();
        let lib = ctx.library().unwrap();
        let s = lib.get("バカ").unwrap();
        assert_eq!((s.key.as_str(), s.watched_count()), ("バカ", 1));
        assert!(lib.get("yuru camp").unwrap().aliases.is_empty());
    }

    /// Unmerging works from the library already loaded: the log is not read
    /// again (here another device's log has become unreadable since).
    #[cfg(unix)]
    #[test]
    fn unmerging_does_not_read_the_log_again() {
        use std::os::unix::fs::PermissionsExt as _;
        let (_dir, ctx) = legacy_ctx();
        ctx.record([EventBody::Alias { from: "ハカ".into(), to: "yuru camp".into() }]).unwrap();
        let lib = ctx.library().unwrap();
        let other = ctx.paths.events_dir.join("other.jsonl");
        std::fs::write(&other, "").unwrap();
        std::fs::set_permissions(&other, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::File::open(&other).is_ok() {
            return; // Running as root: nothing is unreadable.
        }
        ctx.unmerge(&lib, ["バカ"]).unwrap();
        std::fs::remove_file(&other).unwrap();
        let s = ctx.library().unwrap();
        assert_eq!(s.get("バカ").unwrap().key, "バカ", "the legacy spelling was unmerged too");
    }

    #[test]
    fn linking_leaves_other_series_rows_alone() {
        let dir = tempfile::tempdir().unwrap();
        // `ハカ` is a real series here: its row is its own.
        let ctx = with_shows(dir.path(), &["バカ", "ハカ"]);
        let lib = ctx.library().unwrap();
        assert!(lib.legacy_keys().is_empty());
        ctx.db.put_metas(&[meta("ハカ", 1), meta("バカ", 2)]).unwrap();
        ctx.link(&lib, "バカ", Some(5)).unwrap();
        let left: Vec<String> = ctx.db.all_meta().unwrap().into_iter().map(|m| m.series).collect();
        assert!(left.contains(&"ハカ".to_string()) && !left.contains(&"バカ".to_string()), "{left:?}");

        // Where `ハカ` stands for `バカ`, its row goes with the link.
        let dir = tempfile::tempdir().unwrap();
        let ctx = with_shows(dir.path(), &["バカ"]);
        let lib = ctx.library().unwrap();
        ctx.db.put_metas(&[meta("ハカ", 1)]).unwrap();
        ctx.link(&lib, "バカ", Some(5)).unwrap();
        assert!(no_legacy_row(&ctx));
    }

    #[test]
    fn fuzzy() {
        let names = ["One Piece", "One Punch Man", "Grand Blue S3"];
        let r = fuzzy_rank(names.iter(), "grnd blu", |n| (*n).into());
        assert_eq!(*r[0].0, "Grand Blue S3");
        assert!(fuzzy_rank(names.iter(), "zzzz", |n| (*n).into()).is_empty());
    }
}
