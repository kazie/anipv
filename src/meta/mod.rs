//! Metadata: offline anime database matching and `AniList` airing info.
//!
//! * `offline` — the *anime-offline-database* (a community continuation of
//!   manami's), downloaded on demand; used to match series to `AniList` ids
//!   and learn episode totals.
//! * `anilist` — anonymous GraphQL queries: airing info and prequels by id,
//!   and (only with `anilist_search`) title searches.
//! * `search` — title search for series the offline database can't match;
//!   the one place where text from the user's library (series titles) is
//!   sent anywhere.
//!
//! Matches are cached locally in SQLite. Manual links are recorded as events
//! so they sync between devices.

pub mod anilist;
pub mod offline;
pub mod ratelimit;
pub mod search;

pub use search::{SearchOutcome, add_title_search, search_due, search_unmatched};

use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::events::Link;
use crate::index::db::{AnilistCheck, MatchAttempt, MatchResult, SeriesMeta};
use crate::library::{Library, Series};
use crate::model::SeriesStatus;
use offline::{Match, OfflineDb};

/// Result of matching the library against the offline database (and maybe by
/// title, see [`add_title_search`]).
#[derive(Debug, Default)]
pub struct MatchReport {
    /// New or changed links, and rows of unchanged links given a score.
    pub rows: Vec<SeriesMeta>,
    /// How many of `rows` are unchanged links only given a score.
    pub scored: usize,
    /// Results to record: new ones, and "nothing to link" for a cached link
    /// that no longer matches (which drops it).
    pub attempts: Vec<MatchAttempt>,
    /// Every series matched with nothing to link, once each.
    pub unlinked: Vec<Unlinked>,
    /// Version of the offline database matched against (`""` without one).
    pub db_version: String,
}

impl MatchReport {
    /// The unlinked series with several candidates.
    pub fn ambiguous(&self) -> impl Iterator<Item = &Unlinked> {
        self.unlinked.iter().filter(|u| u.result.is_ambiguous())
    }

    /// The unlinked series with no candidate at all.
    pub fn unmatched(&self) -> impl Iterator<Item = &Unlinked> {
        self.unlinked.iter().filter(|u| !u.result.is_ambiguous())
    }
}

/// A series matched with nothing to link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unlinked {
    /// Series key.
    pub series: String,
    /// Why: no candidate, several, or a title search's answer.
    pub result: MatchResult,
    /// When ambiguous: the candidates, one line each (see [`describe_anime`]).
    pub candidates: Vec<String>,
}

/// One line about an anime, for lists of candidates:
/// `Yuru Camp (TV 2018, 12 eps) anilist:98444`, with `?` for what isn't known.
pub fn describe_anime(title: &str, format: Option<&str>, year: Option<i32>, episodes: Option<u32>, id: u64) -> String {
    let year = year.map(|y| format!(" {y}")).unwrap_or_default();
    let episodes = episodes.map_or_else(|| "?".to_string(), |n| n.to_string());
    format!("{title} ({format}{year}, {episodes} eps) anilist:{id}", format = format.unwrap_or("?"))
}

/// Listed for a series a title search found several anime for earlier (their
/// candidates weren't kept).
pub const SEARCHED_AMBIGUOUS_NOTE: &str =
    "several anime on AniList go by this name; `anipv meta match --search` lists them";

/// Match every series (or just `only`) against the database, returning links
/// that are new or changed and "nothing to link" attempts. Manually linked
/// series use their link; unlinked ones are skipped.
pub fn match_library(lib: &Library, db: &OfflineDb, ts: i64, only: Option<&str>) -> MatchReport {
    let mut rep = MatchReport { db_version: db.version_key().to_string(), ..MatchReport::default() };
    for s in lib.series.iter().filter(|s| only.is_none_or(|k| s.key == k)) {
        let current = s.meta.as_ref().and_then(|m| m.anilist);
        match s.link {
            Link::Manual(id) => {
                if current != Some(id) {
                    rep.rows.push(db.by_anilist(id).map_or_else(
                        || SeriesMeta { series: s.key.clone(), anilist: Some(id), ..SeriesMeta::default() },
                        |e| e.to_meta(&s.key),
                    ));
                }
                continue;
            }
            Link::Off => continue,
            Link::Auto => {}
        }
        let (episodes, max_ep) = episode_shape(s);
        let mut found = None;
        let mut amb = None;
        for k in s.names() {
            match db.match_key(k, &s.title, episodes, max_ep) {
                Match::Unique(e) => {
                    found = Some(e);
                    break;
                }
                Match::Ambiguous(v) if amb.is_none() => amb = Some(v),
                _ => {}
            }
        }
        match found {
            // Same id as cached: keep the row (it may hold fresher AniList
            // data), only filling in a score it lacks (rows from before scores
            // were kept).
            Some(e) if e.anilist_id() == current => {
                if let Some(m) = s.meta.as_ref().filter(|m| m.score.is_none() && e.score.is_some()) {
                    rep.rows.push(SeriesMeta { score: e.score, ..m.clone() });
                    rep.scored += 1;
                }
            }
            Some(e) => rep.rows.push(e.to_meta(&s.key)),
            // A link a title search made (see `SeriesMeta::search_linked`)
            // isn't undone by the database matching nothing, or several
            // entries: the database never could link it.
            None if s.search_linked() => {}
            None => {
                // While the database finds nothing, a title search's answer
                // stands (it searched because of that); several candidates in
                // the database replace it.
                let prior = s.last_match;
                let result = match (prior, &amb) {
                    (_, Some(_)) => MatchResult::Ambiguous,
                    (Some(p), None) if p.result.is_search() => p.result,
                    (_, None) => MatchResult::None,
                };
                // Record what is new: a stale link to drop, another result than
                // the one on record, or one now made with the current database
                // (the time stays when the result is the same).
                if s.meta.is_some() || prior.is_none_or(|p| p.result != result || !p.current) {
                    let at = prior.filter(|p| p.result == result).map_or(ts, |p| p.at);
                    let (series, db_version) = (s.key.clone(), rep.db_version.clone());
                    rep.attempts.push(MatchAttempt { series, db_version, at, result });
                }
                let candidates = match (result, amb) {
                    (MatchResult::Ambiguous, Some(v)) => v.iter().map(|e| e.describe()).collect(),
                    (MatchResult::SearchAmbiguous, _) => vec![SEARCHED_AMBIGUOUS_NOTE.to_string()],
                    _ => Vec::new(),
                };
                rep.unlinked.push(Unlinked { series: s.key.clone(), result, candidates });
            }
        }
    }
    rep
}

/// How many numbered episodes `s` has on record, and the highest number: what
/// a candidate anime must be able to hold.
fn episode_shape(s: &Series) -> (usize, u32) {
    let episodes = s.episodes().count();
    let max_ep = s.episodes().filter_map(|i| i.key.ep).max().map_or(0, super::model::EpNo::whole);
    (episodes, max_ep)
}

/// Metadata for `s`, preferring rows not yet written to the library (when
/// its link admits them, see [`Series::usable_meta`]).
fn meta_of<'a>(s: &'a Series, fresh: &'a HashMap<String, SeriesMeta>) -> Option<&'a SeriesMeta> {
    s.usable_meta(fresh.get(&s.key)).or(s.meta.as_ref())
}

// How long cached metadata is trusted, in seconds.

const HOUR: i64 = 3600;
const DAY: i64 = 24 * HOUR;

/// The automatic update (on TUI start) refreshes airing info older than this.
pub const STARTUP_MAX_AGE: i64 = 12 * HOUR;

/// How long after a refresh a series whose next episode has already aired is
/// refreshed again: `AniList` can take a while to move on to the next
/// episode, and until then every start would ask again.
pub const AIRED_RETRY: i64 = HOUR;

/// How long an id `AniList` returned nothing for is left alone before it is
/// asked about again.
pub const NOT_FOUND_RETRY: i64 = DAY;

/// How long an inbox series' prequels are trusted before they are asked for again.
pub const INBOX_RECHECK: i64 = 30 * DAY;

/// The offline database comes out weekly: a download older than this is replaced.
pub const OFFLINE_DB_MAX_AGE: i64 = 7 * DAY;

/// How long a background update waits before trying an offline database
/// download again (the last try may have failed: a source that is down).
pub const OFFLINE_DB_RETRY: i64 = DAY;

/// With `refresh_untracked`, how old the data of a series that isn't tracked
/// may get before a background update refreshes it.
pub const UNTRACKED_MAX_AGE: i64 = 30 * DAY;

/// With `refresh_untracked`, how many untracked series one background update
/// refreshes at most (the oldest first), so a large library is spread out.
pub const UNTRACKED_PER_RUN: usize = 50;

/// When `AniList` was last asked about each id (see [`AnilistCheck`]).
pub type Checks = HashMap<u64, AnilistCheck>;

/// True if `AniList` returned nothing for `id` less than [`NOT_FOUND_RETRY`] ago.
fn recently_not_found(checks: &Checks, id: u64, ts: i64) -> bool {
    checks.get(&id).is_some_and(|c| !c.found && ts - c.checked_at < NOT_FOUND_RETRY)
}

/// `AniList` ids of followed/paused series whose airing info is older than
/// `max_age`, or whose next episode has aired (at most every [`AIRED_RETRY`]).
/// Ids `AniList` recently didn't know (see `checks`) are left out.
pub fn stale_ids(lib: &Library, checks: &Checks, ts: i64, max_age: i64) -> Vec<(String, u64)> {
    lib.series
        .iter()
        .filter_map(|s| Some((s.key.clone(), stale_id(s, s.meta.as_ref(), checks, ts, max_age)?)))
        .collect()
}

/// The `AniList` id of `s` if its metadata `m` is stale (see [`stale_ids`]).
fn stale_id(s: &Series, m: Option<&SeriesMeta>, checks: &Checks, ts: i64, max_age: i64) -> Option<u64> {
    if !s.status.is_tracked() {
        return None;
    }
    // Airing info is kept fresh for series being watched; others (e.g.
    // completed) only get the one-time refresh below.
    let watching = matches!(s.status, SeriesStatus::Following | SeriesStatus::Paused);
    let id = s.anilist_id_with(m)?;
    if recently_not_found(checks, id, ts) {
        return None;
    }
    // No cached data for this id yet (e.g. a manual link without the
    // offline database): fetch it.
    let Some(m) = m.filter(|m| m.is_for(id)) else { return Some(id) };
    // Offline-database data can predate the show airing (or finishing),
    // so a row that never came from AniList is always refreshed once.
    let Some(refreshed) = m.refreshed_at else { return Some(id) };
    if !watching {
        return None;
    }
    let finished = m.is_finished();
    let age = ts - refreshed;
    let aired = m.next_airing.is_some_and(|t| t < ts) && age >= AIRED_RETRY;
    (!finished && (age >= max_age || aired)).then_some(id)
}

/// `AniList` ids of linked series that aren't tracked (untracked, dropped,
/// skipped) whose data is missing or older than [`UNTRACKED_MAX_AGE`]: at
/// most [`UNTRACKED_PER_RUN`], least recently refreshed first. Ids `AniList`
/// recently didn't know (see `checks`) are left out.
pub fn untracked_stale_ids(lib: &Library, checks: &Checks, ts: i64) -> Vec<(String, u64)> {
    let mut due: Vec<(Option<i64>, &str, u64)> = lib
        .series
        .iter()
        .filter(|s| !s.status.is_tracked())
        .filter_map(|s| {
            let id = s.anilist_id_with(s.meta.as_ref())?;
            let refreshed = s.meta.as_ref().filter(|m| m.is_for(id)).and_then(|m| m.refreshed_at);
            let stale = refreshed.is_none_or(|at| ts.saturating_sub(at) >= UNTRACKED_MAX_AGE);
            (stale && !recently_not_found(checks, id, ts)).then_some((refreshed, s.key.as_str(), id))
        })
        .collect();
    // `None` (never refreshed) sorts first.
    due.sort_unstable();
    due.into_iter().take(UNTRACKED_PER_RUN).map(|(_, key, id)| (key.to_string(), id)).collect()
}

/// `AniList` ids to refresh because the user asked for it: every tracked series
/// (every linked one with `untracked`, or just `only`, whatever its status),
/// regardless of age or "finished".
pub fn requested_ids(
    lib: &Library,
    fresh: &HashMap<String, SeriesMeta>,
    only: Option<&str>,
    untracked: bool,
) -> Vec<(String, u64)> {
    lib.series
        .iter()
        .filter(|s| match only {
            Some(k) => s.key == k,
            None => untracked || s.status.is_tracked(),
        })
        .filter_map(|s| Some((s.key.clone(), s.anilist_id_with(meta_of(s, fresh))?)))
        .collect()
}

/// Apply `AniList` results to cache rows for the given (series, id) pairs.
pub fn apply_media(
    lib: &Library,
    fresh: &HashMap<String, SeriesMeta>,
    pairs: &[(String, u64)],
    media: &[anilist::Media],
    ts: i64,
) -> Vec<SeriesMeta> {
    let by_id: HashMap<u64, &anilist::Media> = media.iter().map(|m| (m.id, m)).collect();
    pairs
        .iter()
        .filter_map(|(key, id)| {
            let m = by_id.get(id)?;
            let mut row = lib
                .get(key)
                .and_then(|s| meta_of(s, fresh).cloned())
                // Don't mix in facts cached for a different anime.
                .filter(|row| row.is_for(*id))
                .unwrap_or_else(|| SeriesMeta { series: key.clone(), ..SeriesMeta::default() });
            m.apply(&mut row, ts);
            Some(row)
        })
        .collect()
}

/// True if some series needs matching against the offline database: it has
/// no metadata and was never matched, or has a manual link not yet applied.
pub fn needs_match(lib: &Library) -> bool {
    lib.series.iter().any(|s| {
        let unchecked = s.meta.is_none() && s.link == Link::Auto && s.last_match.is_none_or(|m| !m.current);
        (unchecked && s.worth_matching()) || (s.link.manual().is_some() && s.usable_meta(s.meta.as_ref()).is_none())
    })
}

/// True if a title search is due at `ts` for some series that was matched
/// with nothing to link (see [`search_due`]): one from before search was
/// turned on, past [`search::SEARCH_CAP`], whose search failed, or whose last
/// answer is old. Such searches don't need the offline database: the series
/// are taken as matched last time.
pub fn searches_due(lib: &Library, ts: i64) -> bool {
    unlinked_series(lib).any(|s| search_due(s, ts))
}

/// Series with an automatic link and no metadata, worth searching.
fn searchable(lib: &Library) -> impl Iterator<Item = &Series> {
    lib.series.iter().filter(|s| s.meta.is_none() && s.link == Link::Auto && s.worth_searching())
}

/// [`searchable`] series matched against the current offline database with
/// nothing to link.
fn unlinked_series(lib: &Library) -> impl Iterator<Item = &Series> {
    searchable(lib).filter(|s| s.last_match.is_some_and(|m| m.current))
}

/// A report of `series` for a title search without matching against the
/// offline database (again): each as on record (a search's answer even from
/// an older database, as [`match_library`] keeps it), else unmatched.
fn report_of<'a>(lib: &Library, series: impl Iterator<Item = &'a Series>) -> MatchReport {
    let unlinked = series
        .map(|s| {
            let on_record = s.last_match.filter(|m| m.current || m.result.is_search());
            let result = on_record.map_or(MatchResult::None, |m| m.result);
            let candidates = if result == MatchResult::SearchAmbiguous {
                vec![SEARCHED_AMBIGUOUS_NOTE.to_string()]
            } else {
                Vec::new()
            };
            Unlinked { series: s.key.clone(), result, candidates }
        })
        .collect();
    MatchReport { unlinked, db_version: lib.db_version.clone(), ..MatchReport::default() }
}

/// A report for a title search when there is no offline database to match
/// with: every series without a link (or just `only`) is unmatched.
pub fn unmatched_without_database(lib: &Library, only: Option<&str>) -> MatchReport {
    report_of(lib, searchable(lib).filter(|s| only.is_none_or(|o| o == s.key)))
}

/// Outcome of [`sync`].
#[derive(Debug, Default)]
pub struct SyncResult {
    /// Rows to write.
    pub rows: Vec<SeriesMeta>,
    /// "Nothing to link" results to record.
    pub attempts: Vec<MatchAttempt>,
    /// Version of the offline database used, if it was loaded (see
    /// [`OfflineDb::version_key`]).
    pub db_version: Option<String>,
    /// Things to tell the user that aren't failures: a fallback source served
    /// the offline database, it is old, or a title search stopped at a failed
    /// request.
    pub notes: Vec<String>,
    /// `AniList` prequel ids fetched for inbox and engaged series (ids
    /// `AniList` knows).
    pub prequels: Vec<(u64, Vec<u64>)>,
    /// `AniList` sequel ids fetched with them.
    pub sequels: Vec<(u64, Vec<u64>)>,
    /// Every id `AniList` was asked about, and whether it returned it.
    pub checked: Vec<(u64, bool)>,
    /// Non-fatal problems (offline DB unreadable, `AniList` unreachable…).
    pub errors: Vec<String>,
}

/// Inbox series to ask `AniList` about: their prequels (to recognise new
/// seasons of shows you watch) and current airing state (the offline database
/// often still says "upcoming" for a show whose first episode is on disk).
///
/// Each id is asked once, then again after [`INBOX_RECHECK`]; one `AniList`
/// didn't know after [`NOT_FOUND_RETRY`] (see `checks`).
pub fn inbox_lookups(lib: &Library, cfg: &crate::config::Config, checks: &Checks, ts: i64) -> Vec<(String, u64)> {
    let ongoing = cfg.ongoing_roots();
    lib.series
        .iter()
        .filter(|s| Library::in_inbox(s, &ongoing))
        .filter_map(|s| {
            let id = s.anilist_id()?;
            let due = match checks.get(&id) {
                None => true,
                Some(c) if !c.found => ts - c.checked_at >= NOT_FOUND_RETRY,
                // Known from an airing refresh alone: its prequels were never asked for.
                Some(c) => s.prequels.is_none() || ts - c.checked_at >= INBOX_RECHECK,
            };
            due.then(|| (s.key.clone(), id))
        })
        .collect()
}

/// Series you have a relationship with (see [`Series::engaged`]) whose
/// `AniList` relations (prequels and sequels, to group them with their other
/// seasons and entries) were never fetched, or not for [`INBOX_RECHECK`].
/// Only ids of series you engage with are sent, as for airing refreshes.
pub fn relation_lookups(lib: &Library, ts: i64) -> Vec<(String, u64)> {
    lib.series
        .iter()
        .filter(|s| s.engaged())
        .filter(|s| s.relations_at.is_none_or(|at| ts - at >= INBOX_RECHECK))
        .filter_map(|s| Some((s.key.clone(), s.anilist_id()?)))
        .collect()
}

/// What a metadata update is asked to cover (see [`plan`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Only what is missing, or stale after `max_age` seconds.
    Auto {
        /// Refresh airing info older than this, for series being watched.
        max_age: i64,
    },
    /// Everything, now (and the offline database if it is old).
    All,
    /// One series (by key), now, whatever its status.
    Series(String),
}

/// Which series a [`sync`] refreshes from `AniList`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refresh {
    /// Series whose airing info is stale: `ids` as [`stale_ids`] found them
    /// when planning. Series matched by the sync itself are looked at again
    /// with their new rows.
    Stale {
        /// The stale (series, id) pairs.
        ids: Vec<(String, u64)>,
        /// The age `ids` were picked by.
        max_age: i64,
        /// When `AniList` was last asked about each id (see [`stale_ids`]).
        checks: Checks,
    },
    /// Every tracked series (or the [`SyncOptions::only`] one, whatever its
    /// status), regardless of age or "finished".
    Requested,
}

impl Default for Refresh {
    fn default() -> Self {
        Self::Stale { ids: Vec::new(), max_age: 0, checks: Checks::new() }
    }
}

/// What a [`sync`] should do: made by [`plan`], or [`SyncOptions::requested`]
/// for matching alone.
#[derive(Debug)]
#[expect(clippy::struct_excessive_bools, reason = "independent switches of one update")]
pub struct SyncOptions {
    /// Inbox series to fetch airing info and prequels for (see [`inbox_lookups`]).
    pub inbox: Vec<(String, u64)>,
    /// Engaged series to fetch relations for (see [`relation_lookups`]).
    pub relations: Vec<(String, u64)>,
    /// What to refresh from `AniList`.
    pub refresh: Refresh,
    /// Match against the offline database (it always is after a download).
    pub rematch: bool,
    /// Limit matching and refreshing to this series key.
    pub only: Option<String>,
    /// A requested refresh of everything also asks about series that aren't
    /// tracked (untracked, dropped, skipped): their ids go to `AniList`.
    pub untracked: bool,
    /// Download the offline database first if it is missing or older than
    /// [`OFFLINE_DB_MAX_AGE`]. A failed download is reported in
    /// [`SyncResult::errors`] and the cached copy (if any) is used.
    pub update_offline: bool,
    /// The offline database as loaded by earlier syncs (and updated by this one).
    pub offline: std::sync::Arc<offline::OfflineCache>,
    /// Where an update downloads the offline database from, in order: the
    /// built-in sources unless [`plan`] fills it from the config (see
    /// [`offline::source_urls`]).
    pub offline_urls: Vec<String>,
    /// Network metadata is on (`anilist` in the config): without it, [`sync`]
    /// asks `AniList` nothing, whatever transport it is given.
    pub network: bool,
    /// Look series the offline database can't match up on `AniList` by title
    /// (`anilist_search` in the config, with `network`): their titles are sent.
    /// Without `rematch`, the series are taken as matched last time (see
    /// [`searches_due`]).
    pub search: bool,
}

impl Default for SyncOptions {
    /// Nothing to do, from the built-in sources.
    fn default() -> Self {
        Self {
            inbox: Vec::new(),
            relations: Vec::new(),
            refresh: Refresh::default(),
            rematch: false,
            only: None,
            untracked: false,
            update_offline: false,
            offline: std::sync::Arc::default(),
            offline_urls: offline::source_urls(None),
            network: false,
            search: false,
        }
    }
}

impl SyncOptions {
    /// A user-requested re-match and refresh of everything (`None`) or one
    /// series, without inbox lookups or an offline database download (nor,
    /// until [`SyncOptions::network`] is set, any `AniList` query).
    pub fn requested(only: Option<String>) -> Self {
        Self { refresh: Refresh::Requested, rematch: true, only, ..Self::default() }
    }

    /// True if the sync would ask `AniList` anything (given a connection):
    /// inbox lookups, a refresh, or title searches.
    pub fn asks_anilist(&self) -> bool {
        !self.inbox.is_empty()
            || !self.relations.is_empty()
            || !matches!(&self.refresh, Refresh::Stale { ids, .. } if ids.is_empty())
            || self.search
    }
}

/// Plan a metadata update for `request`: `None` when an automatic one has
/// nothing to do. With `anilist = false` in `cfg`, nothing is planned that
/// needs the network (inbox lookups, stale refreshes, the offline database
/// download) and the options say so ([`SyncOptions::network`]), so [`sync`]
/// asks `AniList` nothing; matching still uses a downloaded database.
pub fn plan(
    lib: &Library,
    cfg: &crate::config::Config,
    checks: &Checks,
    request: &Request,
    ts: i64,
) -> Option<SyncOptions> {
    let network = cfg.anilist;
    let search = network && cfg.anilist_search;
    let offline_urls = offline::source_urls(cfg.offline_db_url.as_deref());
    let inbox = if network { inbox_lookups(lib, cfg, checks, ts) } else { Vec::new() };
    let relations = if network { relation_lookups(lib, ts) } else { Vec::new() };
    let only = match request {
        Request::Auto { max_age } => {
            let ids = if network { stale_ids(lib, checks, ts, *max_age) } else { Vec::new() };
            let rematch = needs_match(lib);
            // Searches run after a rematch (which may find series to search),
            // or on their own when some are due.
            let search = search && (rematch || searches_due(lib, ts));
            if ids.is_empty() && inbox.is_empty() && relations.is_empty() && !rematch && !search {
                return None;
            }
            let refresh = Refresh::Stale { ids, max_age: *max_age, checks: checks.clone() };
            return Some(SyncOptions {
                inbox,
                relations,
                refresh,
                rematch,
                offline_urls,
                network,
                search,
                ..SyncOptions::default()
            });
        }
        Request::All => None,
        Request::Series(key) => Some(key.clone()),
    };
    // A requested update also refreshes an old offline database.
    Some(SyncOptions {
        inbox,
        relations,
        update_offline: network,
        network,
        search,
        offline_urls,
        ..SyncOptions::requested(only)
    })
}

/// Plan a background update (`anipv update`): what the TUI's automatic update
/// on start does ([`STARTUP_MAX_AGE`]), plus the offline database when
/// `offline_due` (see [`OfflineDb::needs_update`]) and, with
/// `refresh_untracked`, some [`untracked_stale_ids`]. `None` when nothing is
/// due. Like [`plan`], nothing needs the network with `anilist = false`.
pub fn plan_background(
    lib: &Library,
    cfg: &crate::config::Config,
    checks: &Checks,
    offline_due: bool,
    ts: i64,
) -> Option<SyncOptions> {
    let network = cfg.anilist;
    let update_offline = network && offline_due;
    let untracked = if network && cfg.refresh_untracked { untracked_stale_ids(lib, checks, ts) } else { Vec::new() };
    let auto = plan(lib, cfg, checks, &Request::Auto { max_age: STARTUP_MAX_AGE }, ts);
    if auto.is_none() && untracked.is_empty() && !update_offline {
        return None;
    }
    let mut opts = auto.unwrap_or_else(|| SyncOptions {
        refresh: Refresh::Stale { ids: Vec::new(), max_age: STARTUP_MAX_AGE, checks: checks.clone() },
        offline_urls: offline::source_urls(cfg.offline_db_url.as_deref()),
        network,
        ..SyncOptions::default()
    });
    // No overlap: `plan` only refreshes tracked series.
    if let Refresh::Stale { ids, .. } = &mut opts.refresh {
        ids.extend(untracked);
    }
    opts.update_offline = update_offline;
    Some(opts)
}

/// What a [`sync_with_progress`] is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncStep {
    /// Downloading the offline database.
    Downloading,
    /// Matching and refreshing (after a download).
    Syncing,
}

/// Match against the offline database (if downloaded), then (when `http` is
/// given and [`SyncOptions::network`] is on) refresh from `AniList`: stale
/// airing info, the inbox series' airing info and prequels, or, when forced,
/// everything requested.
pub fn sync(
    lib: &Library,
    cache_dir: &Path,
    http: Option<&dyn anilist::Http>,
    opts: &SyncOptions,
    ts: i64,
) -> SyncResult {
    sync_with_progress(lib, cache_dir, http, opts, ts, &mut |_| {})
}

/// The offline database freshly downloaded, when `opts` asks for an update
/// and the cached copy is missing or old. Notes and a failure go to `out`
/// (the cached copy, if any, is used then).
fn download_offline(
    opts: &SyncOptions,
    cache_dir: &Path,
    out: &mut SyncResult,
    progress: &mut dyn FnMut(SyncStep),
) -> Option<std::sync::Arc<OfflineDb>> {
    if !opts.update_offline || !OfflineDb::needs_update(cache_dir) {
        return None;
    }
    progress(SyncStep::Downloading);
    let res = opts.offline.download(cache_dir, &opts.offline_urls);
    progress(SyncStep::Syncing);
    match res {
        Ok((db, notes)) => {
            out.notes.extend(notes);
            Some(db)
        }
        Err(e) => {
            out.errors.push(format!("offline db download: {e:#}"));
            None
        }
    }
}

/// [`sync`], telling `progress` when an offline database download starts and ends.
pub fn sync_with_progress(
    lib: &Library,
    cache_dir: &Path,
    http: Option<&dyn anilist::Http>,
    opts: &SyncOptions,
    ts: i64,
    progress: &mut dyn FnMut(SyncStep),
) -> SyncResult {
    let mut out = SyncResult::default();
    let mut fresh: HashMap<String, SeriesMeta> = HashMap::new();
    let only = opts.only.as_deref();
    let inbox: Vec<(String, u64)> = opts.inbox.iter().filter(|(k, _)| only.is_none_or(|o| o == k)).cloned().collect();
    let downloaded = download_offline(opts, cache_dir, &mut out, progress);
    let searching = http.filter(|_| opts.search && opts.network);
    let db = match downloaded {
        Some(db) => Some(Ok(Some(db))),
        None => opts.rematch.then(|| opts.offline.load(cache_dir)),
    };
    let report = match db {
        Some(Ok(Some(db))) => {
            out.db_version = Some(db.version_key().to_string());
            Some(match_library(lib, &db, ts, only))
        }
        // Nothing to match with: a title search still can.
        Some(Ok(None)) => searching.map(|_| unmatched_without_database(lib, only)),
        Some(Err(e)) => {
            out.errors.push(format!("offline db: {e:#}"));
            None
        }
        // Only searches are due: the series are as matched last time, so the
        // offline database (tens of megabytes) needn't be loaded.
        None => searching.map(|_| report_of(lib, unlinked_series(lib))),
    };
    if let Some(mut rep) = report {
        if let Some(http) = searching {
            http.optional(&mut || out.notes.extend(add_title_search(&mut rep, lib, http, ts, false).stopped));
        }
        fresh.extend(rep.rows.into_iter().map(|r| (r.series.clone(), r)));
        out.attempts = rep.attempts;
    }
    if let Some(http) = http.filter(|_| opts.network) {
        // A series whose cached link no longer matches is being unlinked: don't
        // refresh the old anime's data, which would write the link back.
        let unlinked: HashSet<&str> = out.attempts.iter().map(|a| a.series.as_str()).collect();
        let wanted = match &opts.refresh {
            Refresh::Requested => requested_ids(lib, &fresh, only, opts.untracked),
            Refresh::Stale { ids, max_age, checks } => {
                let rechecked = lib.series.iter().filter_map(|s| {
                    let m = fresh.get(&s.key)?;
                    Some((s.key.clone(), stale_id(s, Some(m), checks, ts, *max_age)?))
                });
                ids.iter().filter(|(k, _)| !fresh.contains_key(k)).cloned().chain(rechecked).collect()
            }
        };
        let mut seen = HashSet::new();
        let pairs: Vec<(String, u64)> = wanted
            .into_iter()
            .chain(inbox.iter().cloned())
            .filter(|(key, _)| !unlinked.contains(key.as_str()))
            .filter(|p| seen.insert(p.clone()))
            .collect();
        // Every id answered, and whether any query returned it. An id in a
        // failed batch of either query gets no record, so it is asked again.
        let mut checked: HashMap<u64, bool> = HashMap::new();
        let mut failed: HashSet<u64> = HashSet::new();
        let mut record = |asked: &[u64], answered: &[u64], got: HashSet<u64>, errors: &[anyhow::Error]| {
            let answered: HashSet<u64> = answered.iter().copied().collect();
            failed.extend(asked.iter().filter(|id| !answered.contains(id)));
            for id in answered {
                *checked.entry(id).or_default() |= got.contains(&id);
            }
            out.errors.extend(errors.iter().map(|e| format!("{e:#}")));
        };
        if !pairs.is_empty() {
            let ids: Vec<u64> = pairs.iter().map(|p| p.1).collect();
            let res = anilist::fetch(http, &ids);
            record(&ids, &res.answered, res.items.iter().map(|m| m.id).collect(), &res.errors);
            for r in apply_media(lib, &fresh, &pairs, &res.items, ts) {
                fresh.insert(r.series.clone(), r);
            }
        }
        // Relations for the inbox (recorded as asked about, with their airing
        // info) and for engaged series (not: their airing info is refreshed
        // on its own schedule).
        let inbox_ids: Vec<u64> = inbox.iter().map(|p| p.1).collect();
        let mut ids = inbox_ids.clone();
        ids.extend(opts.relations.iter().filter(|(k, _)| only.is_none_or(|o| o == k)).map(|p| p.1));
        let mut seen_ids = HashSet::new();
        ids.retain(|id| seen_ids.insert(*id));
        if !ids.is_empty() {
            let res = anilist::fetch_relations(http, &ids);
            let asked: HashSet<u64> = inbox_ids.iter().copied().collect();
            let answered: Vec<u64> = res.answered.iter().copied().filter(|id| asked.contains(id)).collect();
            let got = res.items.iter().map(|r| r.id).filter(|id| asked.contains(id)).collect();
            record(&inbox_ids, &answered, got, &res.errors);
            out.prequels = res.items.iter().map(|r| (r.id, r.prequels.clone())).collect();
            // An id AniList didn't return gets an empty sequel list too: it
            // is asked again after `INBOX_RECHECK`, not on every start.
            let returned: HashSet<u64> = res.items.iter().map(|r| r.id).collect();
            let unknown = res.answered.iter().filter(|id| !returned.contains(id)).map(|&id| (id, Vec::new()));
            out.sequels = res.items.into_iter().map(|r| (r.id, r.sequels)).chain(unknown).collect();
        }
        out.checked = checked.into_iter().filter(|(id, _)| !failed.contains(id)).collect();
    }
    out.rows = fresh.into_values().collect();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{Index, MetaCache};
    use crate::config::{Config, Root, RootKind};
    use crate::events::{Event, EventBody};
    use crate::index::db::FileRow;
    use anilist::Echo;

    pub(super) fn lib(events: &[Event], meta: &[SeriesMeta]) -> Library {
        lib_with(events, meta, &[])
    }

    pub(super) fn lib_with(events: &[Event], meta: &[SeriesMeta], attempts: &[MatchAttempt]) -> Library {
        lib_at("2026-07-04", events, meta, attempts)
    }

    /// [`lib_with`], with `version` the current offline database version
    /// ([`lib_with`] has the sample database's).
    pub(super) fn lib_at(version: &str, events: &[Event], meta: &[SeriesMeta], attempts: &[MatchAttempt]) -> Library {
        let cfg = cfg();
        let row = |rel: &str| FileRow::test("dl", rel);
        let files = vec![
            row("[A] One Piece - 1181.mkv"),
            row("[A] Grand Blue S3 - 01.mkv"),
            row("[A] Shared - 01.mkv"),
            row("[A] Zzz - 01.mkv"),
        ];
        let meta = MetaCache {
            rows: meta.to_vec(),
            attempts: attempts.to_vec(),
            db_version: version.into(),
            ..MetaCache::default()
        };
        Library::build(&cfg, events, &Index::new(&cfg, files, meta))
    }

    #[test]
    fn match_report() {
        let db = OfflineDb::from_slice(offline::tests::SAMPLE.as_bytes()).unwrap();
        let rep = match_library(&lib(&[], &[]), &db, 5, None);
        let keys: Vec<_> = rep.rows.iter().map(|r| (r.series.as_str(), r.anilist)).collect();
        assert!(keys.contains(&("one piece", Some(21))));
        assert!(keys.contains(&("grand blue s3", Some(199_111))));
        let unlinked: Vec<(&str, MatchResult)> = rep.unlinked.iter().map(|u| (u.series.as_str(), u.result)).collect();
        assert_eq!(unlinked, [("shared", MatchResult::Ambiguous), ("zzz", MatchResult::None)]);
        assert_eq!(rep.ambiguous().next().unwrap().candidates.len(), 2, "both twins listed");
        assert_eq!(rep.rows.len(), 2, "only real links become rows");
        // Unmatched/ambiguous series are recorded so they aren't retried every start.
        let tried: Vec<(&str, MatchResult, &str)> =
            rep.attempts.iter().map(|a| (a.series.as_str(), a.result, a.db_version.as_str())).collect();
        assert!(tried.contains(&("zzz", MatchResult::None, "2026-07-04")));
        assert!(tried.contains(&("shared", MatchResult::Ambiguous, "2026-07-04")));
        assert!(needs_match(&lib(&[], &rep.rows)));
        let after = lib_with(&[], &rep.rows, &rep.attempts);
        assert!(!needs_match(&after));
        assert!(match_library(&after, &db, 6, None).attempts.is_empty(), "checked series aren't re-recorded");
    }

    /// A cached row for the matched id is kept, AniList data and all; only a
    /// missing score is filled in from the database.
    #[test]
    fn cached_rows_get_missing_scores() {
        let db = OfflineDb::from_slice(offline::tests::SAMPLE.as_bytes()).unwrap();
        let cached = SeriesMeta {
            series: "one piece".into(),
            anilist: Some(21),
            next_ep: Some(1181),
            popularity: Some(612_000),
            refreshed_at: Some(3),
            ..SeriesMeta::default()
        };
        let rep = match_library(&lib(&[], std::slice::from_ref(&cached)), &db, 5, None);
        let row = rep.rows.iter().find(|r| r.series == "one piece").unwrap();
        assert_eq!(*row, SeriesMeta { score: Some(87), ..cached.clone() });
        assert_eq!(rep.scored, 1);
        let scored = SeriesMeta { score: Some(90), ..cached };
        let rep = match_library(&lib(&[], &[scored]), &db, 5, None);
        assert!(rep.rows.iter().all(|r| r.series != "one piece"), "a scored row is left alone");
        assert_eq!(rep.scored, 0);
    }

    #[test]
    fn unlinked_series_are_not_rematched() {
        let db = OfflineDb::from_slice(offline::tests::SAMPLE.as_bytes()).unwrap();
        let unlink = Event::new(1, "d", EventBody::Unlink { series: "one piece".into() });
        // A link cached before the unlink arrived (e.g. from another device) is ignored.
        let cached = SeriesMeta {
            series: "one piece".into(),
            anilist: Some(21),
            refreshed_at: Some(1),
            ..SeriesMeta::default()
        };
        let l = lib(&[unlink], &[cached]);
        let op = l.get("one piece").unwrap();
        assert_eq!(op.link, Link::Off);
        assert!(op.meta.is_none());
        let rep = match_library(&l, &db, 5, None);
        assert!(!rep.rows.iter().any(|r| r.series == "one piece"), "the wrong match is not restored");
        assert!(!rep.attempts.iter().any(|a| a.series == "one piece"));
    }

    #[test]
    fn manual_link_wins() {
        let db = OfflineDb::from_slice(offline::tests::SAMPLE.as_bytes()).unwrap();
        let ev = Event::new(1, "d", EventBody::Meta { series: "zzz".into(), anilist: Some(100_922), episodes: None });
        let rep = match_library(&lib(&[ev], &[]), &db, 5, None);
        let z = rep.rows.iter().find(|r| r.series == "zzz").unwrap();
        assert_eq!(z.title.as_deref(), Some("Grand Blue"));
    }

    #[test]
    fn stale_selection() {
        let follow = |k: &str| Event::new(1, "d", EventBody::status(k, SeriesStatus::Following, None));
        let meta = vec![
            SeriesMeta {
                series: "one piece".into(),
                anilist: Some(21),
                status: Some("RELEASING".into()),
                refreshed_at: Some(0),
                ..SeriesMeta::default()
            },
            SeriesMeta {
                series: "grand blue s3".into(),
                anilist: Some(199_111),
                status: Some("FINISHED".into()),
                refreshed_at: Some(0),
                ..SeriesMeta::default()
            },
        ];
        let l = lib(&[follow("one piece"), follow("grand blue s3")], &meta);
        assert_eq!(stale_ids(&l, &Checks::new(), 100_000, 43_200), vec![("one piece".to_string(), 21)]);
        assert!(stale_ids(&l, &Checks::new(), 100, 43_200).is_empty());
        // A recent "found" check doesn't hold back a stale refresh.
        let found = Checks::from([(21, AnilistCheck { checked_at: 99_999, found: true })]);
        assert_eq!(stale_ids(&l, &found, 100_000, 43_200).len(), 1);
    }

    /// A followed series whose next episode has aired is refreshed, but not
    /// again within [`AIRED_RETRY`] while `AniList` still lists that episode.
    #[test]
    fn aired_episodes_are_rechecked_at_most_hourly() {
        let follow = Event::new(1, "d", EventBody::status("one piece", SeriesStatus::Following, None));
        let meta = SeriesMeta {
            series: "one piece".into(),
            anilist: Some(21),
            status: Some("RELEASING".into()),
            next_airing: Some(50),
            refreshed_at: Some(1000),
            ..SeriesMeta::default()
        };
        let l = lib(&[follow], &[meta]);
        let day = 86_400;
        assert!(stale_ids(&l, &Checks::new(), 1000 + AIRED_RETRY - 1, day).is_empty());
        assert_eq!(stale_ids(&l, &Checks::new(), 1000 + AIRED_RETRY, day), vec![("one piece".to_string(), 21)]);
    }

    /// A completed series whose data only came from the offline database (say,
    /// after the cache was rebuilt) still gets one `AniList` refresh, so it
    /// doesn't show stale "upcoming, 12 episodes" forever.
    #[test]
    fn completed_series_get_a_one_time_refresh() {
        let done = Event::new(1, "d", EventBody::status("one piece", SeriesStatus::Completed, None));
        let db = OfflineDb::from_slice(offline::tests::SAMPLE.as_bytes()).unwrap();
        let offline = db.by_anilist(21).unwrap().to_meta("one piece");
        let l = lib(std::slice::from_ref(&done), std::slice::from_ref(&offline));
        assert_eq!(stale_ids(&l, &Checks::new(), 5, 43_200).len(), 1);
        let refreshed = SeriesMeta { refreshed_at: Some(1), status: Some("RELEASING".into()), ..offline };
        assert!(stale_ids(&lib(&[done], &[refreshed]), &Checks::new(), 999_999, 43_200).is_empty());
    }

    /// Skipped series are left alone like dropped ones (no automatic or
    /// "refresh all" lookups), but asking for one by name still works.
    #[test]
    fn skipped_series_are_not_refreshed_unless_asked_for() {
        let skip = Event::new(1, "d", EventBody::status("one piece", SeriesStatus::Skipped, None));
        let db = OfflineDb::from_slice(offline::tests::SAMPLE.as_bytes()).unwrap();
        let l = lib(&[skip], &[db.by_anilist(21).unwrap().to_meta("one piece")]);
        assert!(stale_ids(&l, &Checks::new(), 5, 43_200).is_empty());
        assert!(requested_ids(&l, &HashMap::new(), None, false).is_empty());
        assert_eq!(requested_ids(&l, &HashMap::new(), None, true), vec![("one piece".to_string(), 21)]);
        assert_eq!(requested_ids(&l, &HashMap::new(), Some("one piece"), false), vec![("one piece".to_string(), 21)]);
    }

    /// A manual link without any cached row (no offline database) still gets
    /// `AniList` data; so does one whose cached row is for a different id.
    #[test]
    fn manual_links_without_data_are_fetched() {
        let follow = Event::new(1, "d", EventBody::status("zzz", SeriesStatus::Following, None));
        let link = Event::new(2, "d", EventBody::Meta { series: "zzz".into(), anilist: Some(777), episodes: None });
        let l = lib(&[follow.clone(), link.clone()], &[]);
        assert_eq!(stale_ids(&l, &Checks::new(), 5, 43_200), vec![("zzz".to_string(), 777)]);
        let old = SeriesMeta { series: "zzz".into(), anilist: Some(1), refreshed_at: Some(5), ..SeriesMeta::default() };
        let l = lib(&[follow, link], &[old]);
        assert_eq!(stale_ids(&l, &Checks::new(), 5, 43_200), vec![("zzz".to_string(), 777)]);
    }

    /// Regression: a row just created from the offline database (here: a show
    /// listed as upcoming with 12 episodes that has since finished with 10)
    /// must be refreshed from `AniList` right away, not treated as fresh.
    #[test]
    fn offline_rows_are_refreshed_from_anilist_immediately() {
        let follow = Event::new(1, "d", EventBody::status("one piece", SeriesStatus::Following, None));
        let db = OfflineDb::from_slice(offline::tests::SAMPLE.as_bytes()).unwrap();
        let row = db.by_anilist(21).unwrap().to_meta("one piece");
        assert_eq!(row.refreshed_at, None);
        let l = lib(std::slice::from_ref(&follow), &[row]);
        assert_eq!(stale_ids(&l, &Checks::new(), 5, 43_200), vec![("one piece".to_string(), 21)]);
        // Even an offline row that claims FINISHED gets one AniList refresh.
        let finished =
            SeriesMeta { status: Some("FINISHED".into()), ..db.by_anilist(21).unwrap().to_meta("one piece") };
        assert_eq!(stale_ids(&lib(&[follow], &[finished]), &Checks::new(), 5, 43_200).len(), 1);
    }

    /// What [`plan`] makes of an automatic update, without inbox lookups.
    fn auto(l: &Library, checks: Checks, ts: i64, max_age: i64) -> SyncOptions {
        let ids = stale_ids(l, &checks, ts, max_age);
        SyncOptions {
            refresh: Refresh::Stale { ids, max_age, checks },
            rematch: needs_match(l),
            network: true,
            ..SyncOptions::default()
        }
    }

    /// [`SyncOptions::requested`] with network metadata on.
    fn requested(only: Option<&str>) -> SyncOptions {
        SyncOptions { network: true, ..SyncOptions::requested(only.map(str::to_string)) }
    }

    /// An automatic update with nothing stale, unmatched or in the inbox is
    /// not planned; one with is.
    #[test]
    fn automatic_updates_are_planned_only_when_needed() {
        let follow = Event::new(1, "d", EventBody::status("one piece", SeriesStatus::Following, None));
        let fresh = SeriesMeta { status: Some("RELEASING".into()), ..row("one piece", 21, Some(99)) };
        let cfg = Config { anilist: true, ..cfg() };
        let index = |rows: Vec<SeriesMeta>| {
            let files = vec![FileRow::test("dl", "[A] One Piece - 1181.mkv")];
            // Relations fetched already (see `relation_lookups`).
            let sequels = HashMap::from([(21, (vec![], 100))]);
            Index::new(&cfg, files, MetaCache { rows, sequels, ..MetaCache::default() })
        };
        let l = Library::build(&cfg, std::slice::from_ref(&follow), &index(vec![fresh]));
        let auto = Request::Auto { max_age: 3600 };
        assert!(plan(&l, &cfg, &Checks::new(), &auto, 100).is_none());
        let due = plan(&l, &cfg, &Checks::new(), &auto, 100 + INBOX_RECHECK).unwrap();
        assert_eq!(due.relations, vec![("one piece".into(), 21)]);
        let all = plan(&l, &cfg, &Checks::new(), &Request::All, 100).unwrap();
        assert!(all.asks_anilist() && all.update_offline && all.rematch);
        let stale = plan(&l, &cfg, &Checks::new(), &auto, 99 + 3600).unwrap();
        assert_eq!(
            stale.refresh,
            Refresh::Stale { ids: vec![("one piece".into(), 21)], max_age: 3600, checks: Checks::new() }
        );
        // Not matched yet: planned for matching, though nothing asks AniList.
        let unmatched = Library::build(&cfg, &[follow], &index(Vec::new()));
        let opts = plan(&unmatched, &cfg, &Checks::new(), &auto, 100).unwrap();
        assert!(opts.rematch && !opts.asks_anilist());
    }

    /// Untracked series are refreshed when never refreshed or a month old,
    /// the oldest first; tracked ones are left to [`stale_ids`].
    #[test]
    fn untracked_series_are_refreshed_oldest_first() {
        let ts = 100 * DAY;
        let drop = Event::new(1, "d", EventBody::status("one piece", SeriesStatus::Dropped, None));
        let follow = Event::new(2, "d", EventBody::status("shared", SeriesStatus::Following, None));
        let rows = [
            row("one piece", 21, Some(ts - UNTRACKED_MAX_AGE)),
            row("grand blue s3", 22, None),
            row("zzz", 23, Some(ts - UNTRACKED_MAX_AGE + 1)),
            row("shared", 24, None),
        ];
        let l = lib(&[drop, follow], &rows);
        assert_eq!(
            untracked_stale_ids(&l, &Checks::new(), ts),
            vec![("grand blue s3".to_string(), 22), ("one piece".to_string(), 21)]
        );
        // Not asked again right after AniList didn't know it.
        let checks = Checks::from([(22, AnilistCheck { checked_at: ts - 1, found: false })]);
        assert_eq!(untracked_stale_ids(&l, &checks, ts), vec![("one piece".to_string(), 21)]);
    }

    #[test]
    fn untracked_refreshes_are_capped_per_run() {
        let mut l = lib(&[], &[]);
        let mut many = l.series[0].clone();
        many.meta = Some(row(&many.key, 1, None));
        l.series = vec![many; UNTRACKED_PER_RUN + 5];
        assert_eq!(untracked_stale_ids(&l, &Checks::new(), 5).len(), UNTRACKED_PER_RUN);
    }

    /// A background update adds the offline database (when due) and, only
    /// when asked to, untracked series; nothing of it without the network.
    #[test]
    fn background_updates_add_the_offline_database_and_untracked_series() {
        let ts = 100 * DAY;
        let l = lib(&[], &[row("one piece", 21, None)]);
        let ids = |o: &SyncOptions| match &o.refresh {
            Refresh::Stale { ids, .. } => ids.clone(),
            Refresh::Requested => panic!("not a requested update"),
        };
        let untracked = ("one piece".to_string(), 21);
        let on = Config { anilist: true, refresh_untracked: true, ..cfg() };
        let opts = plan_background(&l, &on, &Checks::new(), true, ts).unwrap();
        assert!(opts.update_offline && opts.network);
        assert!(ids(&opts).contains(&untracked));
        let off = Config { refresh_untracked: false, ..on.clone() };
        let opts = plan_background(&l, &off, &Checks::new(), false, ts).unwrap();
        assert!(!opts.update_offline && !ids(&opts).contains(&untracked));
        let offline = Config { anilist: false, ..on };
        if let Some(opts) = plan_background(&l, &offline, &Checks::new(), true, ts) {
            assert!(!opts.update_offline && !opts.network && ids(&opts).is_empty());
        }
    }

    /// With nothing stale and the offline database recent, there is nothing
    /// to do; an old offline database alone is reason enough.
    #[test]
    fn background_updates_are_planned_only_when_needed() {
        let follow = Event::new(1, "d", EventBody::status("one piece", SeriesStatus::Following, None));
        let fresh = SeriesMeta { status: Some("RELEASING".into()), ..row("one piece", 21, Some(99)) };
        let cfg = Config { anilist: true, refresh_untracked: true, ..cfg() };
        let files = vec![FileRow::test("dl", "[A] One Piece - 1181.mkv")];
        let sequels = HashMap::from([(21, (vec![], 100))]);
        let index = Index::new(&cfg, files, MetaCache { rows: vec![fresh], sequels, ..MetaCache::default() });
        let l = Library::build(&cfg, &[follow], &index);
        assert!(plan_background(&l, &cfg, &Checks::new(), false, 100).is_none());
        let opts = plan_background(&l, &cfg, &Checks::new(), true, 100).unwrap();
        assert!(opts.update_offline && !opts.asks_anilist());
    }

    #[test]
    fn plan_searches_only_with_the_key_and_the_network() {
        let l = lib(&[], &[]);
        let plan_for = |anilist, search| {
            let cfg = Config { anilist, anilist_search: search, ..cfg() };
            plan(&l, &cfg, &Checks::new(), &Request::All, 100).unwrap().search
        };
        assert_eq!(
            [plan_for(true, false), plan_for(true, true), plan_for(false, true), plan_for(false, false)],
            [false, true, false, false]
        );
    }

    /// A link a title search made survives the next match (the database
    /// can't confirm it); an ordinary cached link for a series the database
    /// no longer matches is still undone.
    #[test]
    fn matching_keeps_links_a_search_made() {
        let db = OfflineDb::from_slice(offline::tests::SAMPLE.as_bytes()).unwrap();
        let linked = |search_linked| SeriesMeta {
            series: "zzz".into(),
            anilist: Some(500),
            refreshed_at: Some(1),
            search_linked,
            ..SeriesMeta::default()
        };

        let l = lib(&[], &[linked(true)]);
        assert!(l.get("zzz").unwrap().search_linked());
        let rep = match_library(&l, &db, 9, None);
        assert!(rep.attempts.iter().all(|a| a.series != "zzz"), "{attempts:?}", attempts = rep.attempts);
        assert!(rep.unlinked.iter().all(|u| u.series != "zzz") && rep.rows.iter().all(|r| r.series != "zzz"));

        // The same row without that mark is undone, as before.
        let rep = match_library(&lib(&[], &[linked(false)]), &db, 9, None);
        assert!(rep.attempts.iter().any(|a| a.series == "zzz" && a.result == MatchResult::None));
    }

    /// A newer database with several entries for a series' name doesn't
    /// undo a link a title search made either.
    #[test]
    fn search_links_survive_an_ambiguous_database() {
        let db = OfflineDb::from_slice(offline::tests::SAMPLE.as_bytes()).unwrap();
        let linked = SeriesMeta {
            series: "shared".into(),
            anilist: Some(500),
            refreshed_at: Some(1),
            search_linked: true,
            ..SeriesMeta::default()
        };
        let rep = match_library(&lib(&[], &[linked]), &db, 9, None);
        assert!(rep.attempts.iter().all(|a| a.series != "shared"), "{attempts:?}", attempts = rep.attempts);
        assert!(rep.unlinked.iter().all(|u| u.series != "shared"));
    }

    /// A search link is only one while it is an automatic link marked as
    /// such: a manual link to the same anime, or a database link, isn't.
    #[test]
    fn only_marked_automatic_links_are_search_links() {
        let db = OfflineDb::from_slice(offline::tests::SAMPLE.as_bytes()).unwrap();
        let row = |search_linked| SeriesMeta {
            series: "zzz".into(),
            anilist: Some(500),
            search_linked,
            ..SeriesMeta::default()
        };
        let manual = Event::new(1, "d", EventBody::Meta { series: "zzz".into(), anilist: Some(500), episodes: None });
        assert!(!lib(&[manual], &[row(true)]).get("zzz").unwrap().search_linked());
        assert!(!lib(&[], &[row(false)]).get("zzz").unwrap().search_linked());
        // A newer database that does know the series links it itself: the
        // row it writes carries no mark.
        let shared = lib(&[], &[SeriesMeta { series: "one piece".into(), ..row(true) }]);
        let rep = match_library(&shared, &db, 9, None);
        let op = rep.rows.iter().find(|r| r.series == "one piece").unwrap();
        assert_eq!((op.anilist, op.search_linked), (Some(21), false));
    }

    /// A sync with title searches to do asks `AniList`, even with nothing else.
    #[test]
    fn searches_ask_anilist() {
        assert!(SyncOptions { search: true, ..SyncOptions::default() }.asks_anilist());
        assert!(!SyncOptions { rematch: true, ..SyncOptions::default() }.asks_anilist());
    }

    /// A database result that differs from the one on record is recorded; a
    /// title search's answer stands for "none" (it searched because of one).
    #[test]
    fn changed_database_results_are_recorded() {
        let db = OfflineDb::from_slice(offline::tests::SAMPLE.as_bytes()).unwrap();
        let attempt = |series: &str, result| MatchAttempt {
            series: series.into(),
            db_version: "2026-07-04".into(),
            at: 1,
            result,
        };
        // "shared" is ambiguous in the database, "zzz" unknown to it.
        let l = lib_with(&[], &[], &[attempt("shared", MatchResult::None), attempt("zzz", MatchResult::SearchNone)]);
        let rep = match_library(&l, &db, 9, None);
        let recorded: Vec<(&str, MatchResult)> = rep.attempts.iter().map(|a| (a.series.as_str(), a.result)).collect();
        assert_eq!(recorded, [("shared", MatchResult::Ambiguous)]);
        // The search's answer stands for zzz, and is what it is listed as.
        let zzz = rep.unlinked.iter().find(|u| u.series == "zzz").unwrap();
        assert_eq!(zzz.result, MatchResult::SearchNone);
    }

    /// The planned update downloads from the configured source, or from the
    /// built-in ones.
    #[test]
    fn plan_carries_the_offline_database_sources() {
        let l = lib(&[], &[]);
        let built_in = plan(&l, &cfg(), &Checks::new(), &Request::All, 100).unwrap();
        assert_eq!(built_in.offline_urls, offline::source_urls(None));
        let own = Config { offline_db_url: Some("https://example.org/db.json".into()), ..cfg() };
        let planned = plan(&l, &own, &Checks::new(), &Request::All, 100).unwrap();
        assert_eq!(planned.offline_urls, ["https://example.org/db.json"]);
        // Options made without a plan download from the built-in sources.
        assert_eq!(SyncOptions::requested(None).offline_urls, offline::source_urls(None));
    }

    /// A series matched by the sync itself is refreshed from `AniList` in the
    /// same sync, although it was not stale (it had no data) when planned.
    #[test]
    fn series_matched_by_a_sync_are_refreshed_by_it() {
        let follow = Event::new(1, "d", EventBody::status("one piece", SeriesStatus::Following, None));
        let l = lib(&[follow], &[]);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(OfflineDb::path(dir.path()), offline::tests::SAMPLE).unwrap();
        let fake = Echo::default();
        let res = sync(&l, dir.path(), Some(&fake), &auto(&l, Checks::new(), 100, 3600), 100);
        assert_eq!(*fake.asked.borrow(), vec![21]);
        let op = res.rows.iter().find(|r| r.series == "one piece").unwrap();
        assert_eq!(op.refreshed_at, Some(100));
    }

    fn row(series: &str, id: u64, refreshed: Option<i64>) -> SeriesMeta {
        SeriesMeta {
            series: series.into(),
            anilist: Some(id),
            episodes: Some(12),
            status: Some("FINISHED".into()),
            refreshed_at: refreshed,
            ..SeriesMeta::default()
        }
    }

    #[test]
    fn requested_refresh_ignores_freshness_and_finished() {
        let follow = Event::new(1, "d", EventBody::status("one piece", SeriesStatus::Following, None));
        // Finished and refreshed a second ago: an automatic refresh skips it.
        let l = lib(&[follow], &[row("one piece", 21, Some(99)), row("zzz", 5, Some(99))]);
        let fake = Echo::default();
        let dir = tempfile::tempdir().unwrap();
        let auto = sync(&l, dir.path(), Some(&fake), &auto(&l, Checks::new(), 100, 3600), 100);
        assert!(auto.rows.is_empty() && fake.asked.borrow().is_empty());

        let all = sync(&l, dir.path(), Some(&fake), &requested(None), 100);
        assert_eq!(*fake.asked.borrow(), vec![21], "every tracked series, but not untracked ones");
        assert_eq!(all.rows[0].episodes, Some(10));
        assert_eq!(all.rows[0].refreshed_at, Some(100));
    }

    #[test]
    fn requested_refresh_of_one_series_includes_untracked() {
        let follow = Event::new(1, "d", EventBody::status("one piece", SeriesStatus::Following, None));
        let l = lib(&[follow], &[row("one piece", 21, Some(99)), row("zzz", 5, Some(99))]);
        let fake = Echo::default();
        let dir = tempfile::tempdir().unwrap();
        let res = sync(&l, dir.path(), Some(&fake), &requested(Some("zzz")), 100);
        assert_eq!(*fake.asked.borrow(), vec![5]);
        assert_eq!(res.rows.iter().map(|r| r.series.as_str()).collect::<Vec<_>>(), vec!["zzz"]);
    }

    /// With network metadata off, a sync asks `AniList` nothing, even when it
    /// is given a transport.
    #[test]
    fn network_off_asks_anilist_nothing() {
        let follow = Event::new(1, "d", EventBody::status("one piece", SeriesStatus::Following, None));
        let l = lib(&[follow], &[row("one piece", 21, Some(99))]);
        let opts = plan(&l, &Config { anilist: false, ..cfg() }, &Checks::new(), &Request::All, 100).unwrap();
        assert!(!opts.network);
        let fake = Echo::default();
        sync(&l, tempfile::tempdir().unwrap().path(), Some(&fake), &opts, 100);
        assert!(fake.asked.borrow().is_empty());
        let on = plan(&l, &Config { anilist: true, ..cfg() }, &Checks::new(), &Request::All, 100).unwrap();
        assert!(on.network);
    }

    /// The offline database no longer knows a series whose link was cached: the
    /// link is dropped, not refreshed from `AniList` and written straight back.
    #[test]
    fn dropped_links_are_not_refreshed() {
        let follow = Event::new(1, "d", EventBody::status("zzz", SeriesStatus::Following, None));
        let l = lib(&[follow], &[row("zzz", 5, None)]);
        let db_dir = tempfile::tempdir().unwrap();
        std::fs::write(OfflineDb::path(db_dir.path()), offline::tests::SAMPLE).unwrap();
        let fake = Echo::default();
        let res = sync(&l, db_dir.path(), Some(&fake), &auto(&l, Checks::new(), 100, 0), 100);
        assert!(res.attempts.iter().any(|a| a.series == "zzz"), "no match: attempt recorded");
        assert!(!res.rows.iter().any(|r| r.series == "zzz"), "the old link is not written back");
        assert!(!fake.asked.borrow().contains(&5));
    }

    /// One download root, `dl`.
    pub(super) fn cfg() -> Config {
        Config { roots: vec![Root::test("dl", "/dl", RootKind::Ongoing)], ..Config::default() }
    }

    /// An inbox series with an `AniList` link, plus the given check records.
    fn inbox_lib(checks: Checks, prequels: &[(u64, Vec<u64>)]) -> (Library, Checks) {
        let meta = MetaCache {
            rows: vec![row("zzz", 5, Some(1))],
            prequels: prequels.iter().cloned().collect(),
            anilist_checks: checks.clone(),
            ..MetaCache::default()
        };
        let index = Index::new(&cfg(), vec![FileRow::test("dl", "[A] Zzz - 01.mkv")], meta);
        (Library::build(&cfg(), &[], &index), checks)
    }

    fn checks(res: &SyncResult, ts: i64) -> Checks {
        res.checked.iter().map(|&(id, found)| (id, AnilistCheck { checked_at: ts, found })).collect()
    }

    /// Inbox ids `AniList` returns nothing for are recorded as checked and not
    /// found: not asked about again within [`NOT_FOUND_RETRY`], but after it.
    #[test]
    fn unknown_inbox_ids_are_retried_after_a_day() {
        let (l, none) = inbox_lib(Checks::new(), &[]);
        assert_eq!(inbox_lookups(&l, &cfg(), &none, 100), vec![("zzz".to_string(), 5)]);
        let opts = SyncOptions { inbox: vec![("zzz".into(), 5)], ..auto(&l, Checks::new(), 100, 0) };
        let res = sync(&l, tempfile::tempdir().unwrap().path(), Some(&Echo::unknown()), &opts, 100);
        assert!(res.errors.is_empty(), "{errors:?}", errors = res.errors);
        assert!(res.prequels.is_empty(), "nothing is padded in");
        assert_eq!(res.checked, vec![(5, false)]);

        let (l, checked) = inbox_lib(checks(&res, 100), &[]);
        assert!(inbox_lookups(&l, &cfg(), &checked, 100 + NOT_FOUND_RETRY - 1).is_empty());
        assert_eq!(inbox_lookups(&l, &cfg(), &checked, 100 + NOT_FOUND_RETRY).len(), 1);
    }

    /// docs/anilist.md R13: Once the client gives up during the airing refresh, the relations
    /// aren't asked for: the inbox id gets no check record (so it is asked
    /// again), and the outage is reported once.
    #[test]
    fn a_given_up_client_skips_the_relations() {
        let (l, _) = inbox_lib(Checks::new(), &[]);
        let opts = SyncOptions { inbox: vec![("zzz".into(), 5)], ..auto(&l, Checks::new(), 100, 0) };
        let fake = Echo::giving_up(1);
        let res = sync(&l, tempfile::tempdir().unwrap().path(), Some(&fake), &opts, 100);
        assert_eq!(res.errors, vec!["connection reset".to_string()]);
        assert!(res.checked.is_empty() && res.prequels.is_empty() && res.sequels.is_empty());
        assert!(fake.asked.borrow().is_empty());
    }

    /// A found inbox id (with its prequels stored) is asked again only after
    /// [`INBOX_RECHECK`].
    #[test]
    fn found_inbox_ids_are_rechecked_monthly() {
        let (l, _) = inbox_lib(Checks::new(), &[]);
        let fake = Echo::default();
        let opts = SyncOptions { inbox: vec![("zzz".into(), 5)], ..auto(&l, Checks::new(), 100, 0) };
        let res = sync(&l, tempfile::tempdir().unwrap().path(), Some(&fake), &opts, 100);
        assert_eq!(res.checked, vec![(5, true)]);
        assert_eq!(res.prequels, vec![(5, vec![])]);

        let (l, checked) = inbox_lib(checks(&res, 100), &res.prequels);
        assert!(inbox_lookups(&l, &cfg(), &checked, 100 + NOT_FOUND_RETRY).is_empty());
        assert!(inbox_lookups(&l, &cfg(), &checked, 100 + INBOX_RECHECK - 1).is_empty());
        assert_eq!(inbox_lookups(&l, &cfg(), &checked, 100 + INBOX_RECHECK).len(), 1);
        // Found by an airing refresh alone: the prequels are still to be asked for.
        let (l, checked) = inbox_lib(checks(&res, 100), &[]);
        assert_eq!(inbox_lookups(&l, &cfg(), &checked, 101).len(), 1);
    }

    /// Relations (prequels and sequels) of the series you engage with are
    /// fetched once, then monthly, without counting as an airing check.
    #[test]
    fn engaged_series_relations_are_fetched_monthly() {
        let follow = Event::new(1, "d", EventBody::status("zzz", SeriesStatus::Following, None));
        let l = lib(std::slice::from_ref(&follow), &[row("zzz", 5, Some(1))]);
        assert_eq!(relation_lookups(&l, 100), vec![("zzz".to_string(), 5)]);
        // Untracked series without a watched episode aren't sent.
        assert!(relation_lookups(&lib(&[], &[row("zzz", 5, Some(1))]), 100).is_empty());

        let opts = SyncOptions { relations: relation_lookups(&l, 100), ..auto(&l, Checks::new(), 100, i64::MAX) };
        let res = sync(&l, tempfile::tempdir().unwrap().path(), Some(&Echo::default()), &opts, 100);
        assert!(res.errors.is_empty(), "{errors:?}", errors = res.errors);
        assert_eq!((res.prequels, res.sequels), (vec![(5, vec![])], vec![(5, vec![])]));
        assert!(res.checked.is_empty(), "airing info wasn't asked for");

        // An id AniList doesn't know is recorded too, so it isn't asked on every start.
        let unknown = sync(&l, tempfile::tempdir().unwrap().path(), Some(&Echo::unknown()), &opts, 100);
        assert_eq!((unknown.prequels, unknown.sequels), (vec![], vec![(5, vec![])]));

        let meta = MetaCache {
            rows: vec![row("zzz", 5, Some(1))],
            sequels: HashMap::from([(5, (vec![], 100))]),
            ..MetaCache::default()
        };
        let index = Index::new(&cfg(), vec![FileRow::test("dl", "[A] Zzz - 01.mkv")], meta);
        let l = Library::build(&cfg(), &[follow], &index);
        assert!(relation_lookups(&l, 100 + INBOX_RECHECK - 1).is_empty());
        assert_eq!(relation_lookups(&l, 100 + INBOX_RECHECK).len(), 1);
    }

    /// A followed series whose id `AniList` doesn't know isn't asked about on
    /// every start (its row never gets `refreshed_at`), only once a day.
    #[test]
    fn unknown_ids_are_not_refreshed_on_every_start() {
        let follow = Event::new(1, "d", EventBody::status("zzz", SeriesStatus::Following, None));
        let l = lib(&[follow], &[row("zzz", 5, None)]);
        let none = Checks::new();
        let res = sync(
            &l,
            tempfile::tempdir().unwrap().path(),
            Some(&Echo::unknown()),
            &auto(&l, Checks::new(), 100, 0),
            100,
        );
        assert_eq!(res.checked, vec![(5, false)]);
        let checked = checks(&res, 100);
        assert_eq!(stale_ids(&l, &none, 101, 43_200).len(), 1);
        assert!(stale_ids(&l, &checked, 101, 43_200).is_empty());
        assert_eq!(stale_ids(&l, &checked, 100 + NOT_FOUND_RETRY, 43_200).len(), 1);
        // Automatic syncs skip it too.
        let fake = Echo::default();
        let opts = auto(&l, checked, 101, 0);
        sync(&l, tempfile::tempdir().unwrap().path(), Some(&fake), &opts, 101);
        assert!(fake.asked.borrow().is_empty());
    }

    /// Regression: a failed `AniList` batch no longer discards the batches that
    /// succeeded. Their rows and checks are kept; the failed ids get no check
    /// record (so they are asked again) and the error is reported.
    #[test]
    fn a_failed_anilist_batch_keeps_the_others() {
        let follows: Vec<Event> = (0..60)
            .map(|i| Event::new(1, "d", EventBody::status(format!("s{i}"), SeriesStatus::Following, None)))
            .collect();
        let rows: Vec<SeriesMeta> = (0..60).map(|i| row(&format!("s{i}"), 1000 + i, None)).collect();
        let l = lib(&follows, &rows);
        let res = sync(&l, tempfile::tempdir().unwrap().path(), Some(&Echo::failing(2)), &requested(None), 100);
        assert_eq!(res.errors.len(), 1, "{errors:?}", errors = res.errors);
        assert!(res.errors[0].contains("connection reset"), "{errors:?}", errors = res.errors);
        assert_eq!(res.rows.len(), 50, "the first batch's rows are kept");
        let mut refreshed: Vec<u64> = res.rows.iter().filter_map(|r| r.anilist).collect();
        let mut checked: Vec<u64> = res
            .checked
            .iter()
            .map(|&(id, found)| {
                assert!(found);
                id
            })
            .collect();
        refreshed.sort_unstable();
        checked.sort_unstable();
        assert_eq!(checked, refreshed, "only answered ids are recorded as checked");
    }

    /// A fresh cached offline database is used as is: nothing is downloaded.
    #[test]
    fn update_offline_keeps_a_fresh_database() {
        let l = lib(&[], &[]);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(OfflineDb::path(dir.path()), offline::tests::SAMPLE).unwrap();
        let opts = SyncOptions { update_offline: true, ..SyncOptions::requested(None) };
        let mut steps = Vec::new();
        let res = sync_with_progress(&l, dir.path(), None, &opts, 5, &mut |s| steps.push(s));
        assert!(steps.is_empty(), "no download");
        assert!(res.errors.is_empty(), "{errors:?}", errors = res.errors);
        assert_eq!(res.db_version.as_deref(), Some("2026-07-04"));
        assert!(res.rows.iter().any(|r| r.series == "one piece" && r.anilist == Some(21)));
    }

    #[test]
    fn offline_db_age_check() {
        use std::time::{Duration, SystemTime};
        let dir = tempfile::tempdir().unwrap();
        assert!(OfflineDb::needs_update(dir.path()), "missing");
        let file = std::fs::File::create(OfflineDb::path(dir.path())).unwrap();
        assert!(!OfflineDb::needs_update(dir.path()), "fresh");
        file.set_modified(SystemTime::now() - Duration::from_hours(8 * 24)).unwrap();
        assert!(OfflineDb::needs_update(dir.path()), "older than a week");
    }
}
