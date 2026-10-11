//! Title search on `AniList`: for series the offline database can't match
//! (shows newer than the dataset, say), when `anilist_search` is on or asked
//! for with `meta match --search`. Only the title text is sent.
//!
//! A series links only when one answer goes by its name and can hold the
//! episodes on disk; answers that link nothing are recorded and stand for a
//! while ([`SEARCH_RETRY`]), and at most [`SEARCH_CAP`] series are searched per
//! sync, least recently searched first.

use super::{DAY, MatchReport, Unlinked, anilist, episode_shape};
use crate::identity::series_key;
use crate::index::db::{MatchAttempt, MatchResult, SeriesMeta};
use crate::library::{Library, Series};

/// How many series one sync looks up by title on `AniList`, one request each.
/// `AniList` allows 90 requests a minute (30 while its API is degraded, as it
/// has often been), and the same sync may need a few for airing info: the
/// rest wait for the next run.
pub const SEARCH_CAP: usize = 10;

/// How long a title search that linked nothing stands before the series is
/// searched again. `AniList`'s answer doesn't depend on the offline database,
/// so (unlike "no match" in the database) it outlives database versions.
pub const SEARCH_RETRY: i64 = 30 * DAY;

/// How long after a failed title search the series is searched again. Until
/// then it waits behind the others, so a title whose search keeps failing
/// doesn't hold up the rest.
pub const SEARCH_FAILED_RETRY: i64 = DAY;

/// A title search is due for `s` at `ts` (when it has no link): nothing is on
/// record yet, the offline database had no match and no search has answered
/// since, or the last search's answer is old (see [`SEARCH_RETRY`],
/// [`SEARCH_FAILED_RETRY`]). The database's own ambiguity isn't searched.
pub fn search_due(s: &Series, ts: i64) -> bool {
    let Some(m) = s.last_match else { return true };
    // An answer from the future (the clock was wrong) is old.
    let older_than = |wait: i64| ts - m.at >= wait || m.at > ts;
    match m.result {
        MatchResult::None => true,
        MatchResult::Ambiguous => false,
        MatchResult::SearchFailed => older_than(SEARCH_FAILED_RETRY),
        MatchResult::SearchNone | MatchResult::SearchAmbiguous => older_than(SEARCH_RETRY),
    }
}

/// When `s` was last searched by title (`None`: never, as far as is known).
fn last_searched(s: &Series) -> Option<i64> {
    s.last_match.filter(|m| m.result.is_search()).map(|m| m.at)
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
/// Of those, like [`super::offline::OfflineDb::match_key`] but stricter, a finished anime with
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
    /// Series searched with no link: [`MatchResult::SearchNone`],
    /// [`MatchResult::SearchAmbiguous`] (with its candidates) or
    /// [`MatchResult::SearchFailed`].
    pub unlinked: Vec<Unlinked>,
    /// What else to tell the user.
    pub outcome: SearchOutcome,
}

/// What a title search has to say besides its answers.
#[derive(Debug, Default)]
pub struct SearchOutcome {
    /// Why the search stopped early: a request that failed (a rate limit, no
    /// network, an error `AniList` sent). Not a failure of the sync: the
    /// search is an extra, and the series is searched again later.
    pub stopped: Option<String>,
    /// Series left for the next run (over [`SEARCH_CAP`], or after a failure).
    pub skipped: usize,
}

/// Look `series` up on `AniList` by title, one request each (at most
/// [`SEARCH_CAP`]). Only the title text is sent. A series links only when
/// exactly one answer goes by its key or an alias (its title or a synonym,
/// normalized like a series key) and can hold the episodes on disk.
///
/// Stops at the first failed request: the rest would most likely fail too.
/// The series it failed for is answered [`MatchResult::SearchFailed`], so it
/// waits behind the others for a while instead of failing first every time.
pub fn search_unmatched(series: &[&Series], http: &dyn anilist::Http, ts: i64) -> SearchReport {
    let mut rep = SearchReport::default();
    let mut asked = 0;
    for s in series.iter().take(SEARCH_CAP) {
        asked += 1;
        let unlinked = |result, candidates| Unlinked { series: s.key.clone(), result, candidates };
        let found = match anilist::search(http, &s.title) {
            Ok(found) => found,
            Err(e) => {
                rep.unlinked.push(unlinked(MatchResult::SearchFailed, Vec::new()));
                rep.outcome.stopped = Some(format!("title search stopped at {:?}: {e:#}", s.title));
                break;
            }
        };
        let names: Vec<&str> = s.names().collect();
        let (episodes, max_ep) = episode_shape(s);
        match pick_search_match(&names, episodes, max_ep, &found) {
            SearchPick::Unique(m) => {
                let mut row = SeriesMeta { series: s.key.clone(), search_linked: true, ..SeriesMeta::default() };
                m.apply(&mut row, ts);
                rep.rows.push(row);
            }
            SearchPick::Ambiguous(v) => {
                rep.unlinked.push(unlinked(MatchResult::SearchAmbiguous, v.iter().map(|m| m.describe()).collect()));
            }
            SearchPick::None => rep.unlinked.push(unlinked(MatchResult::SearchNone, Vec::new())),
        }
    }
    rep.outcome.skipped = series.len() - asked;
    rep
}

/// Add a title search on `AniList` to a match against the offline database.
///
/// Searched are the unlinked series the database has no candidate for, or an
/// earlier search found several anime for, when worth matching (see
/// [`Series::worth_searching`]) and a search is due (see [`search_due`]; with
/// `force`, whenever). The least recently searched go first, so the cap
/// rotates through all of them.
///
/// A link joins `rep.rows` (and drops the series' attempt, which would unlink
/// it); any other answer replaces the series' attempt and entry, except a
/// failed search, which answers nothing: the series stays listed as it was,
/// and an earlier search answer stays on record. Without one the failure is
/// recorded, so the series waits behind the others for a while.
pub fn add_title_search(
    rep: &mut MatchReport,
    lib: &Library,
    http: &dyn anilist::Http,
    ts: i64,
    force: bool,
) -> SearchOutcome {
    let mut queue: Vec<&Series> = rep
        .unlinked
        .iter()
        .filter(|u| u.result != MatchResult::Ambiguous)
        .filter_map(|u| lib.get(&u.series))
        .filter(|s| s.worth_searching() && (force || search_due(s, ts)))
        .collect();
    if queue.is_empty() {
        return SearchOutcome::default();
    }
    queue.sort_by_key(|s| last_searched(s));
    let found = search_unmatched(&queue, http, ts);
    for row in found.rows {
        rep.unlinked.retain(|u| u.series != row.series);
        rep.attempts.retain(|a| a.series != row.series);
        rep.rows.push(row);
    }
    for answer in found.unlinked {
        // A failed search leaves an earlier answer on record, as matching
        // carried it forward (and its note with it).
        let failed = answer.result == MatchResult::SearchFailed;
        let entry = rep.unlinked.iter_mut().find(|u| u.series == answer.series);
        if failed
            && entry
                .as_ref()
                .is_some_and(|u| matches!(u.result, MatchResult::SearchNone | MatchResult::SearchAmbiguous))
        {
            continue;
        }
        rep.attempts.retain(|a| a.series != answer.series);
        let (series, db_version) = (answer.series.clone(), rep.db_version.clone());
        rep.attempts.push(MatchAttempt { series, db_version, at: ts, result: answer.result });
        if let Some(entry) = entry.filter(|_| !failed) {
            *entry = answer;
        }
    }
    found.outcome
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use super::*;
    use crate::app::{Index, MetaCache};
    use crate::config::Config;
    use crate::events::{Event, EventBody};
    use crate::index::db::FileRow;
    use crate::meta::offline::{self, OfflineDb};
    use crate::meta::tests::{cfg, lib, lib_at, lib_with};
    use crate::meta::{
        Checks, Request, STARTUP_MAX_AGE, SyncOptions, SyncResult, match_library, needs_match, plan, searches_due, sync,
    };
    use crate::model::SeriesStatus;

    /// An `AniList` that answers title searches from a table (title → anime),
    /// failing for titles it is told to, and counts the requests.
    #[derive(Default)]
    struct Search {
        answers: HashMap<String, Vec<serde_json::Value>>,
        fail: HashSet<String>,
        asked: std::cell::RefCell<Vec<String>>,
        /// Ids asked about by id queries (airing refreshes, prequels).
        ids: std::cell::RefCell<Vec<u64>>,
    }

    impl Search {
        fn new(answers: &[(&str, Vec<serde_json::Value>)]) -> Self {
            let answers = answers.iter().map(|(t, a)| ((*t).to_string(), a.clone())).collect();
            Self { answers, ..Self::default() }
        }

        fn failing(title: &str) -> Self {
            Self { fail: HashSet::from([title.to_string()]), ..Self::default() }
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
            if self.fail.contains(&q) {
                anyhow::bail!("AniList rate limit hit; try again in a minute");
            }
            let media = self.answers.get(&q).cloned().unwrap_or_default();
            Ok(serde_json::json!({"data": {"Page": {"media": media}}}))
        }
    }

    /// A finished 12-episode TV anime, as `AniList` returns it.
    fn found(id: u64, romaji: &str, synonyms: &[&str]) -> serde_json::Value {
        serde_json::json!({"id": id, "episodes": 12, "status": "FINISHED", "format": "TV", "seasonYear": 2026,
            "title": {"romaji": romaji, "english": null, "native": null}, "synonyms": synonyms,
            "nextAiringEpisode": null})
    }

    fn anime(id: u64, format: &str, episodes: Option<u32>, status: &str) -> anilist::Media {
        serde_json::from_value(serde_json::json!({"id": id, "episodes": episodes, "status": status, "format": format,
            "title": {"romaji": "Zzz"}, "nextAiringEpisode": null}))
        .unwrap()
    }

    fn attempt(series: &str, result: MatchResult, at: i64) -> MatchAttempt {
        MatchAttempt { series: series.into(), db_version: "2026-07-04".into(), at, result }
    }

    fn title(l: &Library, key: &str) -> String {
        l.get(key).unwrap().title.clone()
    }

    fn results(unlinked: &[Unlinked]) -> Vec<(&str, MatchResult)> {
        unlinked.iter().map(|u| (u.series.as_str(), u.result)).collect()
    }

    /// A sync against the sample offline database at `ts`, matching, with
    /// `opts`; in the fixture "zzz" is the series the database doesn't know.
    fn sync_at(l: &Library, http: &Search, opts: SyncOptions, ts: i64) -> SyncResult {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(OfflineDb::path(dir.path()), offline::tests::SAMPLE).unwrap();
        sync(l, dir.path(), Some(http), &SyncOptions { rematch: true, ..opts }, ts)
    }

    fn searching() -> SyncOptions {
        SyncOptions { network: true, search: true, ..SyncOptions::default() }
    }

    #[test]
    fn a_search_links_only_an_exact_name() {
        let l = lib(&[], &[]);
        let zzz = l.get("zzz").unwrap();
        let one = |answers: Vec<serde_json::Value>| {
            search_unmatched(&[zzz], &Search::new(&[(zzz.title.as_str(), answers)]), 7)
        };

        // The title, or a synonym, normalizes to the series key: linked, with AniList's data.
        for exact in [found(500, "Zzz", &[]), found(500, "Other", &["ZZZ"])] {
            let rep = one(vec![found(9, "Zzz Movie", &[]), exact]);
            assert_eq!(rep.rows.len(), 1, "{rep:?}");
            let row = &rep.rows[0];
            assert_eq!(
                (row.series.as_str(), row.anilist, row.episodes, row.refreshed_at),
                ("zzz", Some(500), Some(12), Some(7))
            );
            assert!(row.search_linked, "marked as a search's link");
            assert!(rep.unlinked.is_empty() && rep.outcome.stopped.is_none());
        }

        // Close but not the same name, e.g. another season: nothing linked.
        let rep = one(vec![found(1, "Zzz 2nd Season", &[]), found(2, "Zzz Zero", &[])]);
        assert!(rep.rows.is_empty());
        assert_eq!(results(&rep.unlinked), [("zzz", MatchResult::SearchNone)]);

        // Two anime by the same name: not linked, and listed with both.
        let rep = one(vec![found(1, "Zzz", &[]), found(2, "Zzz", &[])]);
        assert!(rep.rows.is_empty());
        assert_eq!(results(&rep.unlinked), [("zzz", MatchResult::SearchAmbiguous)]);
        let candidates = &rep.unlinked[0].candidates;
        assert!(candidates.len() == 2 && candidates[0].contains("anilist:1"), "{candidates:?}");
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
        assert_eq!(id(pick_search_match(&["zzz"], 1, 1, std::slice::from_ref(&movie))), Some(1));
        // No numbered episodes (a movie file, say): no format is preferred.
        let both = [movie, anime(6, "TV", Some(12), "FINISHED")];
        assert!(matches!(pick_search_match(&["zzz"], 0, 0, &both), SearchPick::Ambiguous(v) if v.len() == 2));
    }

    /// Titles are searched one request each, up to the cap; a failed request
    /// stops the rest, and its series is answered "failed".
    #[test]
    fn searches_stop_at_the_cap_and_at_a_failure() {
        let l = lib(&[], &[]);
        let series: Vec<&Series> =
            ["one piece", "grand blue s3", "shared", "zzz"].iter().map(|k| l.get(k).unwrap()).collect();
        let titles: Vec<String> = series.iter().map(|s| s.title.clone()).collect();

        let http = Search::default();
        let rep = search_unmatched(&series, &http, 1);
        assert_eq!(*http.asked.borrow(), titles);
        assert_eq!((rep.unlinked.len(), rep.outcome.skipped), (4, 0));

        let http = Search::failing(&titles[1]);
        let rep = search_unmatched(&series, &http, 1);
        assert_eq!(http.asked.borrow().len(), 2, "stopped after the failure");
        assert_eq!(
            results(&rep.unlinked),
            [("one piece", MatchResult::SearchNone), ("grand blue s3", MatchResult::SearchFailed)]
        );
        assert!(rep.outcome.stopped.as_deref().is_some_and(|n| n.contains("rate limit")));
        assert_eq!(rep.outcome.skipped, 2);

        // More series than the cap.
        let cfg = cfg();
        let files: Vec<FileRow> =
            (0..SEARCH_CAP + 5).map(|i| FileRow::test("dl", &format!("[A] Show {i:02} Name - 01.mkv"))).collect();
        let l = Library::build(&cfg, &[], &Index::new(&cfg, files, MetaCache::default()));
        let all: Vec<&Series> = l.series.iter().collect();
        let http = Search::default();
        let rep = search_unmatched(&all, &http, 1);
        assert_eq!(http.asked.borrow().len(), SEARCH_CAP);
        assert_eq!((rep.unlinked.len(), rep.outcome.skipped), (SEARCH_CAP, 5));
    }

    /// A search is due with nothing on record or a plain "no match"; an answer
    /// stands for a month (a failure for a day); the database's own ambiguity
    /// is never searched.
    #[test]
    fn when_searches_are_due() {
        let zzz = |m: Option<(MatchResult, i64)>| {
            let attempts: Vec<MatchAttempt> = m.map(|(r, at)| attempt("zzz", r, at)).into_iter().collect();
            lib_with(&[], &[], &attempts).get("zzz").cloned().unwrap()
        };
        assert!(search_due(&zzz(None), 1) && search_due(&zzz(Some((MatchResult::None, 1))), 1));
        assert!(!search_due(&zzz(Some((MatchResult::Ambiguous, 1))), 1 + SEARCH_RETRY));
        for (answer, wait) in [
            (MatchResult::SearchFailed, SEARCH_FAILED_RETRY),
            (MatchResult::SearchNone, SEARCH_RETRY),
            (MatchResult::SearchAmbiguous, SEARCH_RETRY),
        ] {
            let s = zzz(Some((answer, 1)));
            assert!(!search_due(&s, wait) && search_due(&s, 1 + wait), "{answer:?}");
        }
    }

    /// With title search on, a series that is due plans an automatic sync
    /// that searches without matching again; one whose answer stands doesn't.
    #[test]
    fn due_searches_are_planned_without_a_rematch() {
        // Every other series has been dealt with.
        let lib_after = |zzz: MatchResult| {
            let mut attempts: Vec<MatchAttempt> = ["one piece", "grand blue s3", "shared"]
                .iter()
                .map(|k| attempt(k, MatchResult::SearchNone, 1))
                .collect();
            attempts.push(attempt("zzz", zzz, 1));
            lib_with(&[], &[], &attempts)
        };
        let due = lib_after(MatchResult::None);
        assert!(!needs_match(&due), "matched already");
        assert!(searches_due(&due, 2));
        let on = Config { anilist: true, anilist_search: true, ..cfg() };
        let auto = Request::Auto { max_age: STARTUP_MAX_AGE };
        let opts = plan(&due, &on, &Checks::new(), &auto, 2).unwrap();
        assert!(opts.search && !opts.rematch && opts.asks_anilist());
        assert!(plan(&due, &Config { anilist_search: false, ..on.clone() }, &Checks::new(), &auto, 2).is_none());
        for done in
            [MatchResult::SearchNone, MatchResult::SearchAmbiguous, MatchResult::SearchFailed, MatchResult::Ambiguous]
        {
            assert!(!searches_due(&lib_after(done), 2), "{done:?}");
            assert!(plan(&lib_after(done), &on, &Checks::new(), &auto, 2).is_none(), "{done:?}");
        }
    }

    /// A search answer from an older offline database version still stands,
    /// but the newer database is asked again: the series is matched (not
    /// searched), and its answer re-recorded under the new version, keeping
    /// its time.
    #[test]
    fn answers_from_an_older_database_are_matched_again() {
        let old =
            MatchAttempt { series: "zzz".into(), db_version: "v1".into(), at: 1, result: MatchResult::SearchNone };
        let l = lib_at("2026-07-04", &[], &[], std::slice::from_ref(&old));
        let zzz = l.get("zzz").unwrap();
        assert!(!zzz.last_match.unwrap().current);
        assert!(needs_match(&l), "matched against the newer database");
        assert!(!searches_due(&l, 2), "not searched from the record");
        let db = OfflineDb::from_slice(offline::tests::SAMPLE.as_bytes()).unwrap();
        let rep = match_library(&l, &db, 9, None);
        let again = rep.attempts.iter().find(|a| a.series == "zzz").unwrap();
        assert_eq!((again.result, again.at, again.db_version.as_str()), (MatchResult::SearchNone, 1, "2026-07-04"));
        // Once re-recorded it is current, and nothing more is needed.
        let l = lib_at("2026-07-04", &[], &[], &rep.attempts);
        assert!(l.get("zzz").unwrap().last_match.unwrap().current);
    }

    /// An answer dated in the future (the clock was wrong) counts as old.
    #[test]
    fn future_answers_are_due() {
        let zzz = lib_with(&[], &[], &[attempt("zzz", MatchResult::SearchNone, 1_000)]).get("zzz").cloned().unwrap();
        assert!(search_due(&zzz, 999) && !search_due(&zzz, 1_000));
    }

    /// A show dropped or skipped and gone from disk isn't searched by title.
    #[test]
    fn rejected_shows_gone_from_disk_are_not_searched() {
        let ep = crate::model::ItemKey::episode(crate::model::EpNo::new(1));
        let events = [
            Event::new(1, "d", EventBody::watched("gone show", ep)),
            Event::new(2, "d", EventBody::status("gone show", SeriesStatus::Skipped, None)),
        ];
        let l = lib(&events, &[]);
        let gone = l.get("gone show").unwrap();
        assert!(gone.worth_matching() && !gone.worth_searching());
        let mut rep = MatchReport {
            unlinked: vec![Unlinked { series: "gone show".into(), result: MatchResult::None, candidates: Vec::new() }],
            ..MatchReport::default()
        };
        let http = Search::default();
        add_title_search(&mut rep, &l, &http, 1, true);
        assert!(http.asked.borrow().is_empty());
    }

    /// Searches that are due run from what is on record: no offline database
    /// is needed (or loaded).
    #[test]
    fn due_searches_run_without_the_offline_database() {
        let attempts = [attempt("zzz", MatchResult::None, 1), attempt("shared", MatchResult::Ambiguous, 1)];
        let l = lib_with(&[], &[], &attempts);
        let http = Search::new(&[(title(&l, "zzz").as_str(), vec![found(500, "Zzz", &[])])]);
        let empty = tempfile::tempdir().unwrap();
        let res = sync(&l, empty.path(), Some(&http), &searching(), 9);
        assert!(res.errors.is_empty(), "{:?}", res.errors);
        assert_eq!(*http.asked.borrow(), [title(&l, "zzz")], "not the database's ambiguous one");
        assert!(res.rows.iter().any(|r| r.series == "zzz" && r.anilist == Some(500) && r.search_linked));
        assert!(res.db_version.is_none(), "the database's version is left as it was");
    }

    /// In a sync, title search runs only when asked for (and with the
    /// network on), and only for series the database has no match for.
    #[test]
    fn a_sync_searches_unmatched_series_only_when_asked() {
        let l = lib(&[], &[]);
        let answers = || Search::new(&[(title(&l, "zzz").as_str(), vec![found(500, "Zzz", &[])])]);

        let http = answers();
        let res = sync_at(&l, &http, searching(), 9);
        assert_eq!(*http.asked.borrow(), [title(&l, "zzz")], "not the matched or the ambiguous ones");
        assert!(res.errors.is_empty() && res.notes.is_empty(), "{res:?}");
        assert!(res.rows.iter().any(|r| r.series == "zzz" && r.anilist == Some(500) && r.search_linked));
        assert!(res.attempts.iter().all(|a| a.series != "zzz"), "no \"none\" that would unlink it");

        for opts in [
            SyncOptions { network: true, ..SyncOptions::default() },
            SyncOptions { search: true, ..SyncOptions::default() },
        ] {
            let http = answers();
            let res = sync_at(&l, &http, opts, 9);
            assert!(http.asked.borrow().is_empty());
            assert!(res.attempts.iter().any(|a| a.series == "zzz" && a.result == MatchResult::None));
        }
    }

    /// An answer that linked nothing stands until it is old, whatever the
    /// offline database does meanwhile; then the series is searched again.
    #[test]
    fn a_series_is_searched_again_only_when_its_answer_is_old() {
        let l = lib(&[], &[]);
        let res = sync_at(&l, &Search::default(), searching(), 9);
        let answer = res.attempts.iter().find(|a| a.series == "zzz").unwrap().clone();
        assert_eq!((answer.result, answer.at), (MatchResult::SearchNone, 9), "replaces the plain \"none\"");
        assert_eq!(res.attempts.iter().filter(|a| a.series == "zzz").count(), 1);

        let l = lib_with(&[], &[], &[answer]);
        let http = Search::new(&[(title(&l, "zzz").as_str(), vec![found(500, "Zzz", &[])])]);
        let res = sync_at(&l, &http, searching(), 10);
        assert!(http.asked.borrow().is_empty());
        assert!(res.attempts.iter().all(|a| a.series != "zzz"), "the answer on record stands, unchanged");
        let res = sync_at(&l, &http, searching(), 9 + SEARCH_RETRY);
        assert_eq!(http.asked.borrow().len(), 1);
        assert!(res.rows.iter().any(|r| r.series == "zzz" && r.anilist == Some(500)));
    }

    /// A failed search is a note, not a failure of the sync (the search is an
    /// extra), and the series is searched again a day later.
    #[test]
    fn a_failed_search_is_a_note_and_asked_again_later() {
        let l = lib(&[], &[]);
        let res = sync_at(&l, &Search::failing(&title(&l, "zzz")), searching(), 9);
        assert!(res.errors.is_empty(), "{:?}", res.errors);
        assert!(res.notes.iter().any(|n| n.contains("rate limit")), "{:?}", res.notes);
        let failed = res.attempts.iter().find(|a| a.series == "zzz").unwrap().clone();
        assert_eq!(failed.result, MatchResult::SearchFailed);
        let zzz = lib_with(&[], &[], &[failed]).get("zzz").cloned().unwrap();
        assert!(!search_due(&zzz, 9 + SEARCH_FAILED_RETRY - 1) && search_due(&zzz, 9 + SEARCH_FAILED_RETRY));
    }

    /// Without a downloaded offline database a title search still runs:
    /// every series without a link is unmatched then.
    #[test]
    fn searches_need_no_offline_database() {
        let l = lib(&[], &[]);
        let http = Search::new(&[(title(&l, "zzz").as_str(), vec![found(500, "Zzz", &[])])]);
        let empty = tempfile::tempdir().unwrap();
        let res = sync(&l, empty.path(), Some(&http), &SyncOptions { rematch: true, ..searching() }, 9);
        assert!(res.errors.is_empty(), "{:?}", res.errors);
        assert!(http.asked.borrow().contains(&title(&l, "zzz")));
        assert!(res.rows.iter().any(|r| r.series == "zzz" && r.anilist == Some(500) && r.search_linked));
        // An update of one series searches only that one.
        let http = Search::default();
        sync(&l, empty.path(), Some(&http), &SyncOptions { only: Some("zzz".into()), rematch: true, ..searching() }, 9);
        assert_eq!(*http.asked.borrow(), [title(&l, "zzz")]);
        // Without searching, there is nothing to do.
        let http = Search::default();
        let res = sync(
            &l,
            empty.path(),
            Some(&http),
            &SyncOptions { rematch: true, network: true, ..SyncOptions::default() },
            9,
        );
        assert!(http.asked.borrow().is_empty() && res.rows.is_empty() && res.attempts.is_empty());
    }

    /// A failed search doesn't replace an earlier answer (or its note).
    #[test]
    fn a_failed_search_keeps_the_earlier_answer() {
        for earlier in [MatchResult::SearchAmbiguous, MatchResult::SearchNone] {
            let l = lib_with(&[], &[], &[attempt("zzz", earlier, 9)]);
            let res = sync_at(&l, &Search::failing(&title(&l, "zzz")), searching(), 9 + SEARCH_RETRY);
            assert!(res.notes.iter().any(|n| n.contains("rate limit")), "{:?}", res.notes);
            assert!(res.attempts.iter().all(|a| a.series != "zzz" || a.result == earlier), "{:?}", res.attempts);
        }
    }

    /// A failed request stops the search; the series it failed for waits
    /// (and then goes behind those never searched), so it can't hold them up.
    #[test]
    fn a_failing_title_does_not_hold_up_the_others() {
        let l = lib(&[], &[]);
        let queue = || MatchReport {
            unlinked: ["grand blue s3", "zzz"]
                .map(|k| Unlinked { series: k.into(), result: MatchResult::None, candidates: Vec::new() })
                .to_vec(),
            ..MatchReport::default()
        };
        let mut rep = queue();
        let http = Search::failing(&title(&l, "grand blue s3"));
        add_title_search(&mut rep, &l, &http, 1, false);
        assert_eq!(http.asked.borrow().len(), 1, "stopped at the failure");
        let recorded: Vec<(&str, MatchResult)> = rep.attempts.iter().map(|a| (a.series.as_str(), a.result)).collect();
        assert_eq!(recorded, [("grand blue s3", MatchResult::SearchFailed)]);

        // Next time: the failed one waits, the other is searched.
        let l = lib_with(&[], &[], &rep.attempts);
        let http = Search::default();
        add_title_search(&mut queue(), &l, &http, 2, false);
        assert_eq!(*http.asked.borrow(), [title(&l, "zzz")]);
        // A day later both are due, the one never searched first.
        let http = Search::default();
        add_title_search(&mut queue(), &l, &http, 1 + SEARCH_FAILED_RETRY, false);
        assert_eq!(*http.asked.borrow(), [title(&l, "zzz"), title(&l, "grand blue s3")]);
    }

    /// `force` asks about series searched before too, but those not yet
    /// searched (then the least recently searched) go first, so repeating it
    /// gets past the cap.
    #[test]
    fn forced_searches_get_past_the_cap() {
        let cfg = cfg();
        let files: Vec<FileRow> =
            (0..SEARCH_CAP + 5).map(|i| FileRow::test("dl", &format!("[A] Show {i:02} Name - 01.mkv"))).collect();
        let none = |l: &Library| MatchReport {
            unlinked: l
                .series
                .iter()
                .map(|s| Unlinked { series: s.key.clone(), result: MatchResult::None, candidates: Vec::new() })
                .collect(),
            ..MatchReport::default()
        };
        let l = Library::build(&cfg, &[], &Index::new(&cfg, files.clone(), MetaCache::default()));
        let (mut rep, http) = (none(&l), Search::default());
        add_title_search(&mut rep, &l, &http, 1, true);
        assert_eq!(http.asked.borrow().len(), SEARCH_CAP);

        let meta = MetaCache { attempts: rep.attempts.clone(), ..MetaCache::default() };
        let l = Library::build(&cfg, &[], &Index::new(&cfg, files, meta));
        let (mut rep, http) = (none(&l), Search::default());
        add_title_search(&mut rep, &l, &http, 2, true);
        let unsearched: Vec<String> =
            l.series.iter().filter(|s| last_searched(s).is_none()).map(|s| s.title.clone()).collect();
        assert_eq!(unsearched.len(), 5);
        assert_eq!(http.asked.borrow()[..5], unsearched[..]);
    }

    /// A series an earlier search found ambiguous stays listed as such; a
    /// forced (or due) search asks again and the new answer replaces it,
    /// except a failed one.
    #[test]
    fn earlier_ambiguous_searches_stay_listed() {
        let db = OfflineDb::from_slice(offline::tests::SAMPLE.as_bytes()).unwrap();
        let l = lib_with(&[], &[], &[attempt("zzz", MatchResult::SearchAmbiguous, 1)]);
        let zzz_entry = |rep: &MatchReport| rep.unlinked.iter().find(|u| u.series == "zzz").cloned().unwrap();

        let rep = match_library(&l, &db, 9, None);
        assert_eq!(zzz_entry(&rep).result, MatchResult::SearchAmbiguous);
        assert_eq!(zzz_entry(&rep).candidates, [crate::meta::SEARCHED_AMBIGUOUS_NOTE]);
        assert!(rep.attempts.iter().all(|a| a.series != "zzz"), "nothing new to record");

        // Not again until due; forced, the new answer replaces the listing.
        let http = Search::default();
        add_title_search(&mut match_library(&l, &db, 9, None), &l, &http, 9, false);
        assert!(http.asked.borrow().is_empty());
        let mut forced = match_library(&l, &db, 9, None);
        add_title_search(&mut forced, &l, &http, 9, true);
        assert_eq!(http.asked.borrow().len(), 1);
        assert_eq!(zzz_entry(&forced).result, MatchResult::SearchNone);
        assert!(forced.attempts.iter().any(|a| a.series == "zzz" && a.result == MatchResult::SearchNone));

        // A search that fails leaves the listing as it was.
        let mut failed = match_library(&l, &db, 9, None);
        add_title_search(&mut failed, &l, &Search::failing(&title(&l, "zzz")), 9, true);
        assert_eq!(zzz_entry(&failed).result, MatchResult::SearchAmbiguous);
    }

    /// Series that aren't on disk and aren't tracked aren't searched (their
    /// titles aren't sent, and they don't use up the cap).
    #[test]
    fn absent_untracked_series_are_not_searched() {
        let ep = crate::model::ItemKey::episode(crate::model::EpNo::new(1));
        let l = lib(&[Event::new(1, "d", EventBody::watched("gone show", ep))], &[]);
        let gone = l.get("gone show").unwrap();
        assert!(!gone.present() && gone.status == SeriesStatus::Untracked && !gone.worth_searching());
        let mut rep = MatchReport {
            unlinked: ["gone show", "zzz"]
                .map(|k| Unlinked { series: k.into(), result: MatchResult::None, candidates: Vec::new() })
                .to_vec(),
            ..MatchReport::default()
        };
        let http = Search::default();
        add_title_search(&mut rep, &l, &http, 1, true);
        assert_eq!(*http.asked.borrow(), [title(&l, "zzz")]);
    }

    /// A series a sync just linked by title is refreshed in the same sync,
    /// not taken for one being unlinked.
    #[test]
    fn search_links_are_refreshed_in_the_same_sync() {
        let follow = Event::new(1, "d", EventBody::status("zzz", SeriesStatus::Following, None));
        let l = lib(&[follow], &[]);
        let http = Search::new(&[(title(&l, "zzz").as_str(), vec![found(500, "Zzz", &[])])]);
        let res = sync_at(&l, &http, SyncOptions { network: true, search: true, ..SyncOptions::requested(None) }, 9);
        assert!(res.rows.iter().any(|r| r.series == "zzz" && r.anilist == Some(500)));
        assert!(http.ids.borrow().contains(&500), "asked about by id too: {:?}", http.ids.borrow());
    }
}
