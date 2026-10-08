//! Metadata: offline anime database matching and `AniList` airing info.
//!
//! * `offline` — manami *anime-offline-database*, downloaded on demand; used
//!   to match series to `AniList` ids and learn episode totals.
//! * `anilist` — anonymous GraphQL queries for followed series' next airing
//!   episode.
//!
//! Matches are cached locally in SQLite. Manual links are recorded as events
//! so they sync between devices.

pub mod anilist;
pub mod offline;

use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::events::Link;
use crate::identity::series_key;
use crate::index::db::{AnilistCheck, MatchAttempt, SeriesMeta};
use crate::library::{Library, Series};
use crate::model::SeriesStatus;
use offline::{Match, OfflineDb};

/// Result of matching the library against the offline database.
#[derive(Debug, Default)]
pub struct MatchReport {
    /// New or changed links.
    pub rows: Vec<SeriesMeta>,
    /// Series that were checked and have nothing to link.
    pub attempts: Vec<MatchAttempt>,
    /// Series with several candidates: (key, candidate titles).
    pub ambiguous: Vec<(String, Vec<String>)>,
    /// Series with no candidate.
    pub unmatched: Vec<String>,
    /// Of `ambiguous`, those a title search found several anime for earlier
    /// (their candidates weren't kept: a forced search asks again).
    pub searched_ambiguous: Vec<String>,
}

/// Listed for a series a title search found several anime for earlier.
pub const SEARCHED_AMBIGUOUS_NOTE: &str =
    "several anime on AniList go by this name; `anipv meta match --search` lists them";

/// Match every series (or just `only`) against the database, returning links
/// that are new or changed and "nothing to link" attempts. Manually linked
/// series use their link; unlinked ones are skipped.
pub fn match_library(lib: &Library, db: &OfflineDb, ts: i64, only: Option<&str>) -> MatchReport {
    let mut rep = MatchReport::default();
    let version = db.version.clone().unwrap_or_default();
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
        for k in std::iter::once(&s.key).chain(&s.aliases) {
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
            // Same id as cached: keep the row (it may hold fresher AniList data).
            Some(e) if e.anilist_id() == current => {}
            Some(e) => rep.rows.push(e.to_meta(&s.key)),
            // A link a title search made (see `SeriesMeta::search_linked`)
            // isn't undone by the database matching nothing, or several
            // entries: the database never could link it.
            None if s.search_linked() => {}
            // A title search found several anime for it earlier: still ambiguous.
            None if amb.is_none() && s.match_result.as_deref() == Some(SEARCH_AMBIGUOUS) => {
                rep.ambiguous.push((s.key.clone(), vec![SEARCHED_AMBIGUOUS_NOTE.to_string()]));
                rep.searched_ambiguous.push(s.key.clone());
            }
            None => {
                let result = if amb.is_some() { "ambiguous" } else { "none" };
                // Record unless this version already said so (and no stale link is cached).
                if s.meta.is_some() || !s.match_checked {
                    rep.attempts.push(MatchAttempt {
                        series: s.key.clone(),
                        db_version: version.clone(),
                        at: ts,
                        result: result.into(),
                    });
                }
                match amb {
                    Some(v) => rep.ambiguous.push((s.key.clone(), v.iter().map(|e| e.describe()).collect())),
                    None => rep.unmatched.push(s.key.clone()),
                }
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

/// How many series one sync looks up by title on `AniList` (one request each;
/// `AniList` allows 90 requests a minute). The rest wait for the next run.
pub const SEARCH_CAP: usize = 30;

/// `MatchAttempt::result` of a title search that found nothing.
pub const SEARCH_NONE: &str = "search-none";
/// `MatchAttempt::result` of a title search with several equally good answers.
pub const SEARCH_AMBIGUOUS: &str = "search-ambiguous";

/// True for the `MatchAttempt::result` of a title search (one that linked
/// nothing: a link is marked on its row, [`SeriesMeta::search_linked`]).
pub fn is_search_attempt(result: &str) -> bool {
    [SEARCH_NONE, SEARCH_AMBIGUOUS].contains(&result)
}

/// What `AniList` answered a title search with.
#[derive(Debug, PartialEq, Eq)]
enum SearchPick<'a> {
    /// One anime goes by one of the series' names.
    Unique(&'a anilist::Media),
    /// Several do.
    Ambiguous(Vec<&'a anilist::Media>),
    /// None does.
    None,
}

/// The anime among `found` that goes by one of `keys` (series keys, see
/// [`series_key`]): its title or a synonym, normalized the same way. Never the
/// "closest" one: a wrong link would put a series' history under another anime,
/// and a season is an entry of its own ("… 2nd Season" is key `… s2`).
///
/// Of those, like [`OfflineDb::match_key`] but stricter, a finished anime with
/// fewer episodes than the highest number on disk (`max_ep`) is never linked
/// (a same-named movie for a TV series), and with more than one numbered
/// episode (`episodes`) a TV or ONA entry ranks first, with exactly one a TV
/// entry (with none, say a movie file, no format is preferred). Several
/// equally good ones are ambiguous.
fn pick_search_match<'a>(keys: &[&str], episodes: usize, max_ep: u32, found: &'a [anilist::Media]) -> SearchPick<'a> {
    let fits = |m: &anilist::Media| {
        let airing = matches!(m.status.as_deref(), Some("RELEASING" | "HIATUS" | "NOT_YET_RELEASED"));
        airing || m.episodes.is_none_or(|n| n == 0 || max_ep <= n)
    };
    let rank = |m: &anilist::Media| {
        let format = m.format.as_deref();
        (episodes > 1 && matches!(format, Some("TV" | "ONA")), episodes == 1 && format == Some("TV"))
    };
    let mut hits: Vec<&anilist::Media> = found
        .iter()
        .filter(|m| m.names().any(|n| keys.contains(&series_key(n).as_str())))
        .filter(|m| fits(m))
        .collect();
    if let Some(best) = hits.iter().map(|m| rank(m)).max() {
        hits.retain(|m| rank(m) == best);
    }
    match hits.len() {
        0 => SearchPick::None,
        1 => SearchPick::Unique(hits.remove(0)),
        _ => SearchPick::Ambiguous(hits),
    }
}

/// Outcome of [`search_unmatched`].
#[derive(Debug, Default)]
pub struct SearchReport {
    /// Links found (with `AniList`'s data applied).
    pub rows: Vec<SeriesMeta>,
    /// Series searched with no link: [`SEARCH_NONE`] or [`SEARCH_AMBIGUOUS`].
    pub attempts: Vec<MatchAttempt>,
    /// Series with several candidates: (key, candidate descriptions).
    pub ambiguous: Vec<(String, Vec<String>)>,
    /// Series that were not searched this time (over [`SEARCH_CAP`], or after a failure).
    pub skipped: usize,
    /// Why the search stopped early: a failed request (the series it failed
    /// for gets no attempt, so it is searched again next time).
    pub errors: Vec<anyhow::Error>,
    /// Titles `AniList` answered with an error about the query (those series
    /// count as searched).
    pub notes: Vec<String>,
}

/// Look the series `keys` up on `AniList` by title, one request each (at most
/// [`SEARCH_CAP`]), for series the offline database has no match for. Only the
/// title text is sent. A series links only when exactly one answer goes by its
/// key or an alias (its title or a synonym, normalized like a series key).
/// Stops at the first failed request (a rate limit, no network): the rest
/// would most likely fail too. An error `AniList` answers one title with
/// doesn't stop it.
pub fn search_unmatched(
    lib: &Library,
    http: &dyn anilist::Http,
    keys: &[String],
    db_version: &str,
    ts: i64,
) -> SearchReport {
    let mut rep = SearchReport::default();
    let attempt = |key: &str, result: &str| MatchAttempt {
        series: key.to_string(),
        db_version: db_version.to_string(),
        at: ts,
        result: result.to_string(),
    };
    let series = keys.iter().filter_map(|k| lib.get(k));
    let mut stopped = false;
    for (i, s) in series.enumerate() {
        if i >= SEARCH_CAP || stopped {
            rep.skipped += 1;
            continue;
        }
        let found = match anilist::search(http, &s.title) {
            Ok(found) => found,
            // An error about this title: it won't search any better next
            // time, so it counts as searched (and the queue moves on).
            Err(e) if e.is::<anilist::AnswerError>() => {
                rep.attempts.push(attempt(&s.key, SEARCH_NONE));
                rep.notes.push(format!("searching {:?}: {e:#}", s.title));
                continue;
            }
            Err(e) => {
                rep.errors.push(e);
                rep.skipped += 1;
                stopped = true;
                continue;
            }
        };
        let names: Vec<&str> = std::iter::once(&s.key).chain(&s.aliases).map(String::as_str).collect();
        let (episodes, max_ep) = episode_shape(s);
        match pick_search_match(&names, episodes, max_ep, &found) {
            SearchPick::Unique(m) => {
                let mut row = SeriesMeta { series: s.key.clone(), search_linked: true, ..SeriesMeta::default() };
                m.apply(&mut row, ts);
                rep.rows.push(row);
            }
            SearchPick::Ambiguous(v) => {
                rep.attempts.push(attempt(&s.key, SEARCH_AMBIGUOUS));
                rep.ambiguous.push((s.key.clone(), v.iter().map(|m| m.describe()).collect()));
            }
            SearchPick::None => rep.attempts.push(attempt(&s.key, SEARCH_NONE)),
        }
    }
    rep
}

/// Add a title search on `AniList` to a match against the offline database:
/// the series it found nothing for (`rep.unmatched`) are looked up with
/// [`search_unmatched`], except those already searched for this database
/// (unless `force`). Links join `rep.rows`; series searched in vain get their
/// plain "no match" attempt replaced by a search one (so they aren't asked
/// again), and ones linked lose it (an attempt would unlink them). With
/// `force`, series a search found ambiguous earlier are searched again too.
pub fn add_title_search(
    rep: &mut MatchReport,
    lib: &Library,
    http: &dyn anilist::Http,
    db_version: &str,
    ts: i64,
    force: bool,
) -> SearchSummary {
    if force {
        let earlier: HashSet<String> = rep.searched_ambiguous.drain(..).collect();
        rep.ambiguous.retain(|(k, _)| !earlier.contains(k));
        rep.unmatched.extend(earlier);
    }
    let searched = |k: &String| lib.get(k).is_some_and(Series::searched);
    // Only series that matter, as for `needs_match`: on disk or tracked.
    let wanted = |k: &String| lib.get(k).is_some_and(|s| s.present() || s.status != SeriesStatus::Untracked);
    let mut keys: Vec<String> =
        rep.unmatched.iter().filter(|k| wanted(k) && (force || !searched(k))).cloned().collect();
    // With `force`, those not searched yet come first, so repeated runs get
    // past the cap instead of asking about the same series again.
    keys.sort_by_key(searched);
    if keys.is_empty() {
        return SearchSummary::default();
    }
    let found = search_unmatched(lib, http, &keys, db_version, ts);
    let answered: HashSet<&str> =
        found.rows.iter().map(|r| r.series.as_str()).chain(found.attempts.iter().map(|a| a.series.as_str())).collect();
    rep.attempts.retain(|a| !answered.contains(a.series.as_str()));
    // Still unmatched unless linked, or found ambiguous (listed with those).
    let listed: HashSet<&str> =
        found.rows.iter().map(|r| r.series.as_str()).chain(found.ambiguous.iter().map(|(k, _)| k.as_str())).collect();
    rep.unmatched.retain(|k| !listed.contains(k.as_str()));
    rep.attempts.extend(found.attempts);
    rep.ambiguous.extend(found.ambiguous);
    rep.rows.extend(found.rows);
    SearchSummary { errors: found.errors, notes: found.notes, skipped: found.skipped }
}

/// What [`add_title_search`] has to say besides its links.
#[derive(Debug, Default)]
pub struct SearchSummary {
    /// Why the search stopped early (a failed request).
    pub errors: Vec<anyhow::Error>,
    /// Titles `AniList` answered with an error (they count as searched).
    pub notes: Vec<String>,
    /// Series left for the next run.
    pub skipped: usize,
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

/// `AniList` ids to refresh because the user asked for it: every tracked series
/// (or just `only`, whatever its status), regardless of age or "finished".
pub fn requested_ids(lib: &Library, fresh: &HashMap<String, SeriesMeta>, only: Option<&str>) -> Vec<(String, u64)> {
    lib.series
        .iter()
        .filter(|s| match only {
            Some(k) => s.key == k,
            None => s.status.is_tracked(),
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

/// True if some series lacks metadata or has a manual link not yet applied.
///
/// With `search` (title search on, see [`SyncOptions::search`]), also if a
/// series the offline database had no match for hasn't been searched yet:
/// one from before search was turned on, past [`SEARCH_CAP`], or whose search
/// failed.
pub fn needs_match(lib: &Library, search: bool) -> bool {
    lib.series.iter().any(|s| {
        let auto = s.meta.is_none() && s.link == Link::Auto;
        let unchecked = auto && (!s.match_checked || (search && s.search_due()));
        (unchecked && (s.present() || s.status != SeriesStatus::Untracked))
            || (s.link.manual().is_some() && s.usable_meta(s.meta.as_ref()).is_none())
    })
}

/// Outcome of [`sync`].
#[derive(Debug, Default)]
pub struct SyncResult {
    /// Rows to write.
    pub rows: Vec<SeriesMeta>,
    /// "Nothing to link" results to record.
    pub attempts: Vec<MatchAttempt>,
    /// Version of the offline database used, if it was loaded.
    pub db_version: Option<String>,
    /// Things to tell the user that aren't failures: a fallback source served
    /// the offline database, it is old, or `AniList` rejected a title search.
    pub notes: Vec<String>,
    /// `AniList` prequel ids fetched for inbox series (ids `AniList` knows).
    pub prequels: Vec<(u64, Vec<u64>)>,
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
#[derive(Debug, Default)]
#[expect(clippy::struct_excessive_bools, reason = "independent switches of one update")]
pub struct SyncOptions {
    /// Inbox series to fetch airing info and prequels for (see [`inbox_lookups`]).
    pub inbox: Vec<(String, u64)>,
    /// What to refresh from `AniList`.
    pub refresh: Refresh,
    /// Match against the offline database (it always is after a download).
    pub rematch: bool,
    /// Limit matching and refreshing to this series key.
    pub only: Option<String>,
    /// Download the offline database first if it is missing or older than
    /// [`OFFLINE_DB_MAX_AGE`]. A failed download is reported in
    /// [`SyncResult::errors`] and the cached copy (if any) is used.
    pub update_offline: bool,
    /// The offline database as loaded by earlier syncs (and updated by this one).
    pub offline: std::sync::Arc<offline::OfflineCache>,
    /// Where an update downloads the offline database from, in order (see
    /// [`offline::source_urls`]; [`plan`] fills it from the config).
    pub offline_urls: Vec<String>,
    /// Network metadata is on (`anilist` in the config): without it, [`sync`]
    /// asks `AniList` nothing, whatever transport it is given.
    pub network: bool,
    /// Look series the offline database can't match up on `AniList` by title
    /// (`anilist_search` in the config, with `network`): their titles are sent.
    pub search: bool,
}

impl SyncOptions {
    /// A user-requested re-match and refresh of everything (`None`) or one
    /// series, without inbox lookups or an offline database download (nor,
    /// until [`SyncOptions::network`] is set, any `AniList` query).
    pub fn requested(only: Option<String>) -> Self {
        Self { refresh: Refresh::Requested, rematch: true, only, ..Self::default() }
    }

    /// True if the sync would ask `AniList` anything (given a connection).
    pub fn asks_anilist(&self) -> bool {
        !self.inbox.is_empty() || !matches!(&self.refresh, Refresh::Stale { ids, .. } if ids.is_empty())
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
    let only = match request {
        Request::Auto { max_age } => {
            let ids = if network { stale_ids(lib, checks, ts, *max_age) } else { Vec::new() };
            let rematch = needs_match(lib, search);
            if ids.is_empty() && inbox.is_empty() && !rematch {
                return None;
            }
            let refresh = Refresh::Stale { ids, max_age: *max_age, checks: checks.clone() };
            return Some(SyncOptions {
                inbox,
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
    Some(SyncOptions { inbox, update_offline: network, network, search, offline_urls, ..SyncOptions::requested(only) })
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
    let downloaded = if opts.update_offline && OfflineDb::needs_update(cache_dir) {
        progress(SyncStep::Downloading);
        let db = opts.offline.download(cache_dir, &opts.offline_urls);
        progress(SyncStep::Syncing);
        db.map_err(|e| out.errors.push(format!("offline db download: {e:#}"))).ok()
    } else {
        None
    };
    if let Some(db) = &downloaded {
        let first = opts.offline_urls.first().map_or("", String::as_str);
        out.notes.extend(offline::download_notes(first, db.source.as_deref(), db.version.as_deref(), ts));
    }
    if downloaded.is_some() || opts.rematch {
        let db = match downloaded {
            Some(db) => Ok(Some(db)),
            None => opts.offline.load(cache_dir),
        };
        match db {
            Ok(Some(db)) => {
                let mut rep = match_library(lib, &db, ts, only);
                if let (true, Some(http)) = (opts.search && opts.network, http) {
                    let version = db.version.clone().unwrap_or_default();
                    let found = add_title_search(&mut rep, lib, http, &version, ts, false);
                    out.errors.extend(found.errors.iter().map(|e| format!("AniList: {e:#}")));
                    out.notes.extend(found.notes);
                }
                fresh.extend(rep.rows.into_iter().map(|r| (r.series.clone(), r)));
                out.attempts = rep.attempts;
                out.db_version.clone_from(&db.version);
            }
            Ok(None) => {}
            Err(e) => out.errors.push(format!("offline db: {e:#}")),
        }
    }
    if let Some(http) = http.filter(|_| opts.network) {
        // A series whose cached link no longer matches is being unlinked: don't
        // refresh the old anime's data, which would write the link back.
        let unlinked: HashSet<&str> = out.attempts.iter().map(|a| a.series.as_str()).collect();
        let wanted = match &opts.refresh {
            Refresh::Requested => requested_ids(lib, &fresh, only),
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
            out.errors.extend(errors.iter().map(|e| format!("AniList: {e:#}")));
        };
        if !pairs.is_empty() {
            let ids: Vec<u64> = pairs.iter().map(|p| p.1).collect();
            let res = anilist::fetch(http, &ids);
            record(&ids, &res.answered, res.items.iter().map(|m| m.id).collect(), &res.errors);
            for r in apply_media(lib, &fresh, &pairs, &res.items, ts) {
                fresh.insert(r.series.clone(), r);
            }
        }
        if !inbox.is_empty() {
            let ids: Vec<u64> = inbox.iter().map(|p| p.1).collect();
            let res = anilist::fetch_prequels(http, &ids);
            record(&ids, &res.answered, res.items.iter().map(|(id, _)| *id).collect(), &res.errors);
            out.prequels = res.items;
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

    fn lib(events: &[Event], meta: &[SeriesMeta]) -> Library {
        lib_with(events, meta, &[])
    }

    fn lib_with(events: &[Event], meta: &[SeriesMeta], attempts: &[MatchAttempt]) -> Library {
        let cfg = cfg();
        let row = |rel: &str| FileRow::test("dl", rel);
        let files = vec![
            row("[A] One Piece - 1181.mkv"),
            row("[A] Grand Blue S3 - 01.mkv"),
            row("[A] Shared - 01.mkv"),
            row("[A] Zzz - 01.mkv"),
        ];
        let meta = MetaCache { rows: meta.to_vec(), attempts: attempts.to_vec(), ..MetaCache::default() };
        Library::build(&cfg, events, &Index::new(&cfg, files, meta))
    }

    #[test]
    fn match_report() {
        let db = OfflineDb::from_slice(offline::tests::SAMPLE.as_bytes()).unwrap();
        let rep = match_library(&lib(&[], &[]), &db, 5, None);
        let keys: Vec<_> = rep.rows.iter().map(|r| (r.series.as_str(), r.anilist)).collect();
        assert!(keys.contains(&("one piece", Some(21))));
        assert!(keys.contains(&("grand blue s3", Some(199_111))));
        assert_eq!(rep.ambiguous.len(), 1);
        assert_eq!(rep.unmatched, vec!["zzz"]);
        assert_eq!(rep.rows.len(), 2, "only real links become rows");
        // Unmatched/ambiguous series are recorded so they aren't retried every start.
        let tried: Vec<(&str, &str, &str)> =
            rep.attempts.iter().map(|a| (a.series.as_str(), a.result.as_str(), a.db_version.as_str())).collect();
        assert!(tried.contains(&("zzz", "none", "2026-07-04")));
        assert!(tried.contains(&("shared", "ambiguous", "2026-07-04")));
        assert!(needs_match(&lib(&[], &rep.rows), false));
        let after = lib_with(&[], &rep.rows, &rep.attempts);
        assert!(!needs_match(&after, false));
        assert!(match_library(&after, &db, 6, None).attempts.is_empty(), "checked series aren't re-recorded");
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
        assert!(requested_ids(&l, &HashMap::new(), None).is_empty());
        assert_eq!(requested_ids(&l, &HashMap::new(), Some("one piece")), vec![("one piece".to_string(), 21)]);
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
            rematch: needs_match(l, false),
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
            Index::new(&cfg, files, MetaCache { rows, ..MetaCache::default() })
        };
        let l = Library::build(&cfg, std::slice::from_ref(&follow), &index(vec![fresh]));
        let auto = Request::Auto { max_age: 3600 };
        assert!(plan(&l, &cfg, &Checks::new(), &auto, 100).is_none());
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

    /// An `AniList` that answers title searches from a table (title → anime),
    /// failing for titles it is told to, and counts the requests.
    struct Search {
        answers: HashMap<String, Vec<serde_json::Value>>,
        fail: HashSet<String>,
        /// Titles `AniList` answers with an error about the query.
        refuse: HashSet<String>,
        asked: std::cell::RefCell<Vec<String>>,
        /// Ids asked about by id queries (airing refreshes, prequels).
        ids: std::cell::RefCell<Vec<u64>>,
    }

    impl Search {
        fn new(answers: &[(&str, Vec<serde_json::Value>)]) -> Self {
            let answers = answers.iter().map(|(t, a)| ((*t).to_string(), a.clone())).collect();
            Self {
                answers,
                fail: HashSet::new(),
                refuse: HashSet::new(),
                asked: std::cell::RefCell::default(),
                ids: std::cell::RefCell::default(),
            }
        }
    }

    impl anilist::Http for Search {
        fn post_json(&self, _url: &str, body: &serde_json::Value) -> anyhow::Result<serde_json::Value> {
            if let Some(ids) = body["variables"].get("ids") {
                self.ids.borrow_mut().extend(serde_json::from_value::<Vec<u64>>(ids.clone()).unwrap_or_default());
                return Ok(serde_json::json!({"data": {"Page": {"media": []}}}));
            }
            let q = body["variables"]["q"].as_str().expect("a title search").to_string();
            self.asked.borrow_mut().push(q.clone());
            if self.refuse.contains(&q) {
                return Ok(serde_json::json!({"errors": [{"message": "Internal Server Error"}], "data": null}));
            }
            if self.fail.contains(&q) {
                anyhow::bail!("AniList rate limit hit; try again in a minute");
            }
            let media = self.answers.get(&q).cloned().unwrap_or_default();
            Ok(serde_json::json!({"data": {"Page": {"media": media}}}))
        }
    }

    fn found(id: u64, romaji: &str, synonyms: &[&str]) -> serde_json::Value {
        serde_json::json!({"id": id, "episodes": 12, "status": "FINISHED", "format": "TV", "seasonYear": 2026,
            "title": {"romaji": romaji, "english": null, "native": null}, "synonyms": synonyms,
            "nextAiringEpisode": null})
    }

    #[test]
    fn a_search_links_only_an_exact_name() {
        let l = lib(&[], &[]);
        let keys = vec!["zzz".to_string()];
        let title = l.get("zzz").unwrap().title.clone();
        let one = |answers: Vec<serde_json::Value>| {
            let http = Search::new(&[(title.as_str(), answers)]);
            search_unmatched(&l, &http, &keys, "2026-10-04", 7)
        };

        // The title, or a synonym, normalizes to the series key: linked, with AniList's data.
        for exact in [found(500, "Zzz", &[]), found(500, "Other", &["ZZZ"])] {
            let rep = one(vec![found(9, "Zzz Movie", &[]), exact]);
            assert_eq!(rep.rows.len(), 1, "{rep:?}");
            assert_eq!((rep.rows[0].series.as_str(), rep.rows[0].anilist), ("zzz", Some(500)));
            assert_eq!((rep.rows[0].episodes, rep.rows[0].refreshed_at), (Some(12), Some(7)));
            assert!(rep.errors.is_empty() && rep.attempts.is_empty());
            assert!(rep.rows[0].search_linked, "marked as a search's link");
        }

        // Close but not the same name, e.g. another season: nothing linked.
        let rep = one(vec![found(1, "Zzz 2nd Season", &[]), found(2, "Zzz Zero", &[])]);
        assert!(rep.rows.is_empty(), "{rep:?}");
        assert_eq!(rep.attempts.len(), 1);
        assert_eq!((rep.attempts[0].result.as_str(), rep.attempts[0].db_version.as_str()), (SEARCH_NONE, "2026-10-04"));

        // Two anime by the same name: not linked, and listed.
        let rep = one(vec![found(1, "Zzz", &[]), found(2, "Zzz", &[])]);
        assert!(rep.rows.is_empty());
        assert_eq!(rep.attempts[0].result, SEARCH_AMBIGUOUS);
        assert_eq!(rep.ambiguous.len(), 1);
        assert!(rep.ambiguous[0].1.iter().any(|d| d.contains("anilist:1")) && rep.ambiguous[0].1.len() == 2);
    }

    /// Runs a sync against the sample offline database with `opts`, `zzz`
    /// being the series the database has no match for.
    fn sync_with_search(l: &Library, http: &Search, opts: SyncOptions) -> SyncResult {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(OfflineDb::path(dir.path()), offline::tests::SAMPLE).unwrap();
        sync(l, dir.path(), Some(http), &SyncOptions { rematch: true, ..opts }, 9)
    }

    #[test]
    fn a_sync_searches_unmatched_series_only_when_asked() {
        let l = lib(&[], &[]);
        let title = l.get("zzz").unwrap().title.clone();
        let http = Search::new(&[(title.as_str(), vec![found(500, "Zzz", &[])])]);

        // On: zzz is looked up by title and linked; the others (matched, or
        // ambiguous in the database) are not asked about.
        let res = sync_with_search(&l, &http, SyncOptions { network: true, search: true, ..SyncOptions::default() });
        assert_eq!(*http.asked.borrow(), std::slice::from_ref(&title));
        assert!(res.errors.is_empty(), "{:?}", res.errors);
        assert!(res.rows.iter().any(|r| r.series == "zzz" && r.anilist == Some(500)));
        assert!(res.attempts.iter().all(|a| a.series != "zzz"), "no plain \"none\" that would unlink it");

        // Off (the default), or without network: not a single request, and zzz is just unmatched.
        for opts in [
            SyncOptions { network: true, ..SyncOptions::default() },
            SyncOptions { search: true, ..SyncOptions::default() },
        ] {
            let http = Search::new(&[(title.as_str(), vec![found(500, "Zzz", &[])])]);
            let res = sync_with_search(&l, &http, opts);
            assert!(http.asked.borrow().is_empty());
            assert!(res.rows.iter().all(|r| r.series != "zzz"));
            assert!(res.attempts.iter().any(|a| a.series == "zzz" && a.result == "none"));
        }
    }

    /// A series searched in vain is not searched again for the same database.
    #[test]
    fn a_series_is_searched_once_per_database() {
        let l = lib(&[], &[]);
        let title = l.get("zzz").unwrap().title.clone();
        let http = Search::new(&[]);
        let opts = || SyncOptions { network: true, search: true, ..SyncOptions::default() };
        let res = sync_with_search(&l, &http, opts());
        assert_eq!(http.asked.borrow().len(), 1);
        let attempt = res.attempts.iter().find(|a| a.series == "zzz").unwrap().clone();
        assert_eq!(attempt.result, SEARCH_NONE, "replaces the plain attempt");
        assert_eq!(res.attempts.iter().filter(|a| a.series == "zzz").count(), 1);

        // Searched in vain, it still counts as unmatched in the report.
        let db = OfflineDb::from_slice(offline::tests::SAMPLE.as_bytes()).unwrap();
        let mut rep = match_library(&l, &db, 9, None);
        assert!(rep.unmatched.contains(&"zzz".to_string()));
        add_title_search(&mut rep, &l, &Search::new(&[]), "v", 9, false);
        assert!(rep.unmatched.contains(&"zzz".to_string()), "{:?}", rep.unmatched);
        let linked = Search::new(&[(title.as_str(), vec![found(500, "Zzz", &[])])]);
        let mut rep = match_library(&l, &db, 9, None);
        add_title_search(&mut rep, &l, &linked, "v", 9, false);
        assert!(!rep.unmatched.contains(&"zzz".to_string()), "linked ones are matched");

        // The attempt is in the cache now: the library knows, and nothing is asked.
        let l = lib_with(&[], &[], &[attempt]);
        assert!(l.get("zzz").unwrap().searched() && l.get("zzz").unwrap().match_checked);
        let http = Search::new(&[(title.as_str(), vec![found(500, "Zzz", &[])])]);
        let res = sync_with_search(&l, &http, opts());
        assert!(http.asked.borrow().is_empty());
        assert!(res.rows.iter().all(|r| r.series != "zzz"));
        // A plain "no match" is not a search.
        let plain =
            MatchAttempt { series: "zzz".into(), db_version: "2026-07-04".into(), at: 1, result: "none".into() };
        assert!(!lib_with(&[], &[], &[plain]).get("zzz").unwrap().searched());
    }

    /// A failed search is reported, leaves no attempt, and doesn't fail the sync.
    #[test]
    fn a_failed_search_is_reported_and_asked_again() {
        let l = lib(&[], &[]);
        let title = l.get("zzz").unwrap().title.clone();
        let mut http = Search::new(&[]);
        http.fail.insert(title);
        let res = sync_with_search(&l, &http, SyncOptions { network: true, search: true, ..SyncOptions::default() });
        assert!(res.errors.iter().any(|e| e.starts_with("AniList: ") && e.contains("rate limit")), "{:?}", res.errors);
        assert!(res.attempts.iter().all(|a| a.result != SEARCH_NONE), "{:?}", res.attempts);
        assert!(res.rows.iter().all(|r| r.series != "zzz"));
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
        assert!(rep.attempts.iter().all(|a| a.series != "zzz"), "{:?}", rep.attempts);
        assert!(!rep.unmatched.contains(&"zzz".to_string()) && rep.rows.iter().all(|r| r.series != "zzz"));

        // The same row without that mark is undone, as before.
        let rep = match_library(&lib(&[], &[linked(false)]), &db, 9, None);
        assert!(rep.attempts.iter().any(|a| a.series == "zzz" && a.result == "none"));
    }

    /// `force` asks about series searched before again, but those not yet
    /// searched go first, so repeating it gets past the cap.
    #[test]
    fn forced_searches_get_past_the_cap() {
        let cfg = cfg();
        let files: Vec<FileRow> =
            (0..SEARCH_CAP + 5).map(|i| FileRow::test("dl", &format!("[A] Show {i:02} Name - 01.mkv"))).collect();
        let none = |l: &Library| MatchReport {
            unmatched: l.series.iter().map(|s| s.key.clone()).collect(),
            ..MatchReport::default()
        };
        let l = Library::build(&cfg, &[], &Index::new(&cfg, files.clone(), MetaCache::default()));
        let (mut rep, http) = (none(&l), Search::new(&[]));
        add_title_search(&mut rep, &l, &http, "v", 1, true);
        let first = http.asked.borrow().clone();
        assert_eq!(first.len(), SEARCH_CAP);

        // What the first run recorded is in the cache now.
        let attempts = rep.attempts.clone();
        let meta = MetaCache { attempts, ..MetaCache::default() };
        let l = Library::build(&cfg, &[], &Index::new(&cfg, files, meta));
        let (mut rep, http) = (none(&l), Search::new(&[]));
        add_title_search(&mut rep, &l, &http, "v", 2, true);
        let second = http.asked.borrow().clone();
        let unsearched: Vec<_> = l.series.iter().filter(|s| !s.searched()).map(|s| s.title.clone()).collect();
        assert_eq!(unsearched.len(), 5);
        assert!(second[..5].iter().all(|t| unsearched.contains(t)), "{second:?}");
    }

    fn anime(id: u64, format: &str, episodes: Option<u32>, status: &str) -> anilist::Media {
        serde_json::from_value(serde_json::json!({"id": id, "episodes": episodes, "status": status, "format": format,
            "title": {"romaji": "Zzz"}, "nextAiringEpisode": null}))
        .unwrap()
    }

    /// A same-named anime that can't hold the episodes on disk (a movie for a
    /// TV series) is never linked; among those that can, a TV entry ranks first.
    #[test]
    fn searches_check_episodes_and_format() {
        let id = |p: SearchPick<'_>| match p {
            SearchPick::Unique(m) => Some(m.id),
            SearchPick::None => None,
            SearchPick::Ambiguous(v) => panic!("ambiguous: {v:?}"),
        };
        let movie = anime(1, "MOVIE", Some(1), "FINISHED");
        let tv24 = anime(2, "TV", Some(24), "FINISHED");
        let tv12 = anime(3, "TV", Some(12), "FINISHED");
        let airing = anime(4, "TV", Some(12), "RELEASING");
        let unknown = anime(5, "ONA", None, "FINISHED");
        // Episodes 1 to 24 on disk.
        let pick = |found: &[anilist::Media]| id(pick_search_match(&["zzz"], 24, 24, found));
        assert_eq!(pick(std::slice::from_ref(&movie)), None, "a movie can't hold 24 episodes");
        assert_eq!(pick(&[movie.clone(), tv24.clone()]), Some(2));
        assert_eq!(pick(std::slice::from_ref(&tv12)), None, "finished with 12");
        assert_eq!(pick(&[tv12, airing]), Some(4), "still airing: its count may grow");
        assert_eq!(pick(std::slice::from_ref(&unknown)), Some(5), "unknown length fits");
        // One episode on disk: the TV entry beats a same-named movie.
        assert_eq!(id(pick_search_match(&["zzz"], 1, 1, &[movie.clone(), tv24])), Some(2));
        assert_eq!(id(pick_search_match(&["zzz"], 1, 1, &[movie])), Some(1));
    }

    /// With title search on, a series the database had no match for and
    /// that hasn't been searched makes an automatic sync match (and search).
    #[test]
    fn searches_due_trigger_a_match() {
        let attempt = |series: &str, result: &str| MatchAttempt {
            series: series.into(),
            db_version: "v".into(),
            at: 1,
            result: result.into(),
        };
        // Every other series has been dealt with.
        let lib_after = |zzz: &str| {
            let mut attempts: Vec<MatchAttempt> =
                ["one piece", "grand blue s3", "shared"].iter().map(|k| attempt(k, SEARCH_NONE)).collect();
            attempts.push(attempt("zzz", zzz));
            lib_with(&[], &[], &attempts)
        };
        let due = lib_after("none");
        assert!(due.get("zzz").unwrap().search_due());
        assert!(!needs_match(&due, false), "without search, a checked series is done");
        assert!(needs_match(&due, true));
        for done in [SEARCH_NONE, SEARCH_AMBIGUOUS, "ambiguous"] {
            assert!(!needs_match(&lib_after(done), true), "{done}");
        }
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
        assert!(rep.attempts.iter().all(|a| a.series != "shared"), "{:?}", rep.attempts);
        assert!(rep.ambiguous.iter().all(|(k, _)| k != "shared"));
    }

    /// A series a sync just linked by title is refreshed in the same sync,
    /// not taken for one being unlinked.
    #[test]
    fn search_links_are_refreshed_in_the_same_sync() {
        let follow = Event::new(1, "d", EventBody::status("zzz", SeriesStatus::Following, None));
        let l = lib(&[follow], &[]);
        let title = l.get("zzz").unwrap().title.clone();
        let http = Search::new(&[(title.as_str(), vec![found(500, "Zzz", &[])])]);
        let opts = SyncOptions { network: true, search: true, ..SyncOptions::requested(None) };
        let res = sync_with_search(&l, &http, opts);
        assert!(res.rows.iter().any(|r| r.series == "zzz" && r.anilist == Some(500)));
        assert!(http.ids.borrow().contains(&500), "asked about by id too: {:?}", http.ids.borrow());
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

    /// A series a title search found ambiguous stays listed as ambiguous on
    /// later matches, and a forced search asks about it again.
    #[test]
    fn earlier_ambiguous_searches_stay_listed() {
        let db = OfflineDb::from_slice(offline::tests::SAMPLE.as_bytes()).unwrap();
        let earlier =
            MatchAttempt { series: "zzz".into(), db_version: "v".into(), at: 1, result: SEARCH_AMBIGUOUS.into() };
        let l = lib_with(&[], &[], &[earlier]);
        let rep = match_library(&l, &db, 9, None);
        assert!(!rep.unmatched.contains(&"zzz".to_string()));
        assert!(rep.ambiguous.iter().any(|(k, c)| k == "zzz" && c == &[SEARCHED_AMBIGUOUS_NOTE]));
        assert_eq!(rep.searched_ambiguous, ["zzz"]);

        // Not again without `force`; with it, the new answer replaces the listing.
        let http = Search::new(&[]);
        let mut again = match_library(&l, &db, 9, None);
        add_title_search(&mut again, &l, &http, "v", 9, false);
        assert!(http.asked.borrow().is_empty());
        let mut forced = match_library(&l, &db, 9, None);
        add_title_search(&mut forced, &l, &http, "v", 9, true);
        assert_eq!(http.asked.borrow().len(), 1);
        assert!(forced.ambiguous.iter().all(|(k, _)| k != "zzz"), "found nothing this time");
        assert!(forced.unmatched.contains(&"zzz".to_string()));
        assert!(forced.attempts.iter().any(|a| a.series == "zzz" && a.result == SEARCH_NONE));
    }

    /// `AniList` answering one title with an error marks just that series as
    /// searched; the others are still searched.
    #[test]
    fn an_error_for_one_title_does_not_stop_the_search() {
        let l = lib(&[], &[]);
        let keys: Vec<String> = ["grand blue s3", "zzz"].map(String::from).to_vec();
        let mut http = Search::new(&[]);
        http.refuse.insert(l.get("grand blue s3").unwrap().title.clone());
        let rep = search_unmatched(&l, &http, &keys, "v", 1);
        assert_eq!(http.asked.borrow().len(), 2, "went on to the next title");
        let results: Vec<(&str, &str)> = rep.attempts.iter().map(|a| (a.series.as_str(), a.result.as_str())).collect();
        assert_eq!(results, [("grand blue s3", SEARCH_NONE), ("zzz", SEARCH_NONE)]);
        assert_eq!((rep.errors.len(), rep.notes.len(), rep.skipped), (0, 1, 0));
        assert!(rep.notes[0].contains("Internal Server Error"), "{:?}", rep.notes);

        // In a sync it is a note, not a failure (which would fail `meta refresh`).
        let mut http = Search::new(&[]);
        http.refuse.insert(l.get("zzz").unwrap().title.clone());
        let res = sync_with_search(&l, &http, SyncOptions { network: true, search: true, ..SyncOptions::default() });
        assert!(res.errors.is_empty(), "{:?}", res.errors);
        assert_eq!(res.notes.len(), 1, "{:?}", res.notes);
    }

    /// Series that aren't on disk and aren't tracked aren't searched (their
    /// titles aren't sent, and they don't use up the cap).
    #[test]
    fn absent_untracked_series_are_not_searched() {
        let watched = Event::new(
            1,
            "d",
            EventBody::watched("gone show", crate::model::ItemKey::episode(crate::model::EpNo::new(1))),
        );
        let l = lib(&[watched], &[]);
        let gone = l.get("gone show").unwrap();
        assert!(!gone.present() && gone.status == SeriesStatus::Untracked);
        let mut rep = MatchReport { unmatched: vec!["gone show".into(), "zzz".into()], ..MatchReport::default() };
        let http = Search::new(&[]);
        add_title_search(&mut rep, &l, &http, "v", 1, true);
        assert_eq!(*http.asked.borrow(), [l.get("zzz").unwrap().title.clone()]);
    }

    /// Without numbered episodes (a movie file, say) no format is preferred:
    /// a same-named movie and TV series are ambiguous.
    #[test]
    fn no_episodes_prefer_no_format() {
        let found = [anime(1, "MOVIE", Some(1), "FINISHED"), anime(2, "TV", Some(12), "FINISHED")];
        assert!(matches!(pick_search_match(&["zzz"], 0, 0, &found), SearchPick::Ambiguous(v) if v.len() == 2));
    }

    /// A season is an anime of its own: the season 1 key doesn't take season 2's entry.
    #[test]
    fn seasons_are_told_apart() {
        let s2 = found(7, "Frieren 2nd Season", &[]);
        let keys = [series_key("Frieren"), series_key("Frieren 2nd Season")];
        let media: Vec<anilist::Media> =
            serde_json::from_value(serde_json::json!([found(1, "Frieren", &[]), s2])).unwrap();
        let pick = |key: &str| match pick_search_match(&[key], 0, 0, &media) {
            SearchPick::Unique(m) => m.id,
            other => panic!("{other:?}"),
        };
        assert_eq!((pick(&keys[0]), pick(&keys[1])), (1, 7));
        assert_eq!(pick_search_match(&["frieren s3"], 0, 0, &media), SearchPick::None);
    }

    /// Titles are searched one request each, only up to the cap; the failure
    /// of a request stops the rest and leaves no attempt for what wasn't answered.
    #[test]
    fn searches_stop_at_the_cap_and_at_a_failure() {
        let l = lib(&[], &[]);
        let keys: Vec<String> = ["one piece", "grand blue s3", "shared", "zzz"].map(String::from).to_vec();
        let titles: Vec<String> = keys.iter().map(|k| l.get(k).unwrap().title.clone()).collect();

        // Nothing found anywhere: one attempt each, one request each.
        let http = Search::new(&[]);
        let rep = search_unmatched(&l, &http, &keys, "v", 1);
        assert_eq!(*http.asked.borrow(), titles);
        assert_eq!(rep.attempts.len(), 4);
        assert_eq!(rep.skipped, 0);

        // The second request fails: the first is answered, the others wait.
        let mut http = Search::new(&[]);
        http.fail.insert(titles[1].clone());
        let rep = search_unmatched(&l, &http, &keys, "v", 1);
        assert_eq!(http.asked.borrow().len(), 2, "stopped after the failure");
        assert_eq!(rep.attempts.iter().map(|a| a.series.as_str()).collect::<Vec<_>>(), [keys[0].as_str()]);
        assert_eq!((rep.errors.len(), rep.skipped), (1, 3));

        // Unknown keys are ignored.
        let http = Search::new(&[]);
        let rep = search_unmatched(&l, &http, &["no such series".to_string()], "v", 1);
        assert!(http.asked.borrow().is_empty() && rep.attempts.is_empty());
    }

    #[test]
    fn the_search_cap_is_respected() {
        // More series than the cap, built from files.
        let cfg = cfg();
        let files: Vec<FileRow> =
            (0..SEARCH_CAP + 5).map(|i| FileRow::test("dl", &format!("[A] Show {i:02} Name - 01.mkv"))).collect();
        let l = Library::build(&cfg, &[], &Index::new(&cfg, files, MetaCache::default()));
        let keys: Vec<String> = l.series.iter().map(|s| s.key.clone()).collect();
        assert!(keys.len() > SEARCH_CAP);
        let http = Search::new(&[]);
        let rep = search_unmatched(&l, &http, &keys, "v", 1);
        assert_eq!(http.asked.borrow().len(), SEARCH_CAP);
        assert_eq!((rep.attempts.len(), rep.skipped), (SEARCH_CAP, keys.len() - SEARCH_CAP));
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
    fn cfg() -> Config {
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
        assert!(res.errors.is_empty(), "{:?}", res.errors);
        assert!(res.prequels.is_empty(), "nothing is padded in");
        assert_eq!(res.checked, vec![(5, false)]);

        let (l, checked) = inbox_lib(checks(&res, 100), &[]);
        assert!(inbox_lookups(&l, &cfg(), &checked, 100 + NOT_FOUND_RETRY - 1).is_empty());
        assert_eq!(inbox_lookups(&l, &cfg(), &checked, 100 + NOT_FOUND_RETRY).len(), 1);
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
        assert_eq!(res.errors.len(), 1, "{:?}", res.errors);
        assert!(res.errors[0].contains("connection reset"), "{:?}", res.errors);
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
        assert!(res.errors.is_empty(), "{:?}", res.errors);
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
        file.set_modified(SystemTime::now() - Duration::from_secs(8 * 86_400)).unwrap();
        assert!(OfflineDb::needs_update(dir.path()), "older than a week");
    }
}
