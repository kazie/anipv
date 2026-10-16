//! Anonymous `AniList` GraphQL queries: airing info and prequels by id (in
//! batches of 50), and title searches (see [`super::search`]).
//!
//! Only public anime data is requested. No account or token is used; nothing
//! about the user is sent, except the titles of series a title search looks
//! up (only with `anilist_search`, or `meta match --search`).
//!
//! What happens when `AniList` can't be asked, and why, is in
//! `docs/anilist.md`, each rule with the tests that hold it.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

#[cfg(test)]
use std::cell::RefCell;
use std::cell::{Cell, OnceCell};
use std::sync::Mutex;
use std::time::Duration;

use super::ratelimit::{Clock, Limiter, RateHeaders, RealClock, Reply, StillLimited, paced};
use crate::index::db::SeriesMeta;

/// GraphQL endpoint.
pub const ENDPOINT: &str = "https://graphql.anilist.co";

const QUERY: &str = "query ($ids: [Int]) {
  Page(perPage: 50) {
    media(id_in: $ids, type: ANIME) {
      id episodes status format seasonYear averageScore popularity
      title { romaji english }
      nextAiringEpisode { episode airingAt }
    }
  }
}";

/// An `AniList` id from `21` or an anime page URL: `https://anilist.co/anime/21/ONE-PIECE`,
/// with or without the scheme (`http` or `https`) and `www.`, and with any
/// trailing slug, slash, `?query` or `#fragment`. Other hosts and other kinds of
/// page (`/manga/…`) are not accepted.
pub fn parse_ref(s: &str) -> Option<u64> {
    let s = s.trim();
    let rest = s.strip_prefix("https://").or_else(|| s.strip_prefix("http://")).unwrap_or(s);
    let rest = rest.strip_prefix("www.").unwrap_or(rest);
    // A page URL gives its first path segment; anything else must be the id itself.
    let id = rest.strip_prefix("anilist.co/anime/").map_or(s, |path| path.split(['/', '?', '#']).next().unwrap_or(""));
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    id.parse().ok()
}

/// Minimal HTTP abstraction so tests can run offline.
pub trait Http {
    /// POST a JSON body and return the JSON response. Fails with
    /// [`Unavailable`] when `AniList` couldn't be asked.
    fn post_json(&self, url: &str, body: &Value) -> Result<Value>;

    /// Run `f`, whose requests are optional (title searches): they may use
    /// only half of what is left of the wait budget, so the requests that
    /// follow still have some.
    fn optional(&self, f: &mut dyn FnMut()) {
        f();
    }
}

/// `AniList` couldn't be asked: offline, timed out, failing (5xx), or still
/// refusing for its rate limit.
#[derive(Debug)]
pub struct Unavailable {
    cause: anyhow::Error,
    /// Whether the client gave up, and when.
    pub outage: Outage,
}

/// How far an [`Unavailable`] goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outage {
    /// This request failed; the next one may not.
    Once,
    /// The client gave up with this request: later ones fail at once.
    GaveUp,
    /// The client gave up before; this request wasn't sent, and its error
    /// was said already.
    GaveUpBefore,
}

impl Outage {
    /// The outage of `e`, if it is an [`Unavailable`].
    pub fn of(e: &anyhow::Error) -> Option<Self> {
        e.downcast_ref::<Unavailable>().map(|u| u.outage)
    }

    /// Whether later requests fail too: stop sending.
    pub fn stops(self) -> bool {
        self != Self::Once
    }
}

impl Unavailable {
    /// Whether `e` is `AniList` still refusing for its rate limit (rather than
    /// being down): it says nothing about what was asked.
    pub fn rate_limited(e: &anyhow::Error) -> bool {
        e.downcast_ref::<Self>().is_some_and(|u| u.cause.is::<StillLimited>())
    }
}

#[cfg(test)]
impl Unavailable {
    /// One for tests: `cause`, with `outage`.
    pub(crate) fn test(cause: &str, outage: Outage) -> Self {
        Self { cause: anyhow::anyhow!("{cause}"), outage }
    }

    /// One for tests: still rate limited, with `outage`.
    pub(crate) fn limited(outage: Outage) -> Self {
        Self { cause: StillLimited::test(Duration::from_secs(70)).into(), outage }
    }
}

/// It reads as its cause, whose own causes are its sources.
impl std::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{cause}", cause = self.cause)
    }
}

impl std::error::Error for Unavailable {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.cause.chain().nth(1)
    }
}

/// The real HTTP client, keeping to `AniList`'s rate limits (see
/// [`super::ratelimit`]): it waits out 429s for up to `max_wait` in all over
/// its requests, telling `on_wait` about each wait worth mentioning.
///
/// It gives up on `AniList` (and fails every later request at once) when a
/// 429 outlasts the budget, or when two requests in a row fail (each sent
/// twice): no answer, a server error, or one that isn't JSON (a proxy or
/// captive portal answering instead). One slow batch isn't an outage.
/// Optional requests never give up, nor count towards it. Make one per sync or command, so
/// each gets the whole budget and a fresh start.
pub struct Ureq<'a> {
    /// What its requests may spend waiting out 429s, together.
    max_wait: Duration,
    /// Inside [`Http::optional`]: the budget of its requests, which don't
    /// give up when it runs out.
    share: Cell<Option<Duration>>,
    /// Told how long a wait will be, before it starts.
    on_wait: &'a dyn Fn(Duration),
    /// What its requests have waited out so far.
    waited: Cell<Duration>,
    /// The last request failed.
    failed: Cell<bool>,
    /// Why it gave up, once it did.
    gave_up: OnceCell<String>,
    /// Sends a request: [`send_with_agent`], but for tests.
    send: &'a dyn Fn(&str, &Value) -> Result<Reply>,
    /// The process-wide limiter, but for tests.
    limiter: &'a Mutex<Limiter>,
    /// [`RealClock`], but for tests.
    clock: &'a dyn Clock,
}

impl<'a> Ureq<'a> {
    /// A client whose requests may wait out 429s for `max_wait` in all.
    pub fn new(max_wait: Duration, on_wait: &'a dyn Fn(Duration)) -> Self {
        Self {
            max_wait,
            share: Cell::new(None),
            on_wait,
            waited: Cell::default(),
            failed: Cell::new(false),
            gave_up: OnceCell::new(),
            send: &send_with_agent,
            limiter: &LIMITER,
            clock: &RealClock,
        }
    }

    /// `cause` as the [`Unavailable`] it is, giving up when it's time to.
    /// Optional requests never do, nor count towards it.
    fn unavailable(&self, cause: anyhow::Error) -> Unavailable {
        let given_up =
            if self.share.get().is_some() { false } else { cause.is::<StillLimited>() || self.failed.replace(true) };
        if given_up {
            let _ = self.gave_up.set(format!("{cause:#}"));
        }
        Unavailable { cause, outage: if given_up { Outage::GaveUp } else { Outage::Once } }
    }
}

/// How long a sync or command may wait out `AniList`'s rate limit while
/// someone watches (the TUI, a command), in all over its requests.
pub const INTERACTIVE_WAIT: Duration = Duration::from_secs(120);

/// How long `anipv update`, which runs unattended, may wait out `AniList`'s
/// rate limit, in all over its requests.
pub const BACKGROUND_WAIT: Duration = Duration::from_secs(600);

/// What this process knows about the rate limit, shared by every request.
static LIMITER: std::sync::LazyLock<Mutex<Limiter>> = std::sync::LazyLock::new(Default::default);

/// One agent for the whole process, so a sync's batches reuse the connection.
static AGENT: std::sync::LazyLock<ureq::Agent> = std::sync::LazyLock::new(|| {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(20)))
        .http_status_as_error(false)
        .build()
        .into()
});

/// POST `body` to `url` with [`AGENT`], once.
fn send_with_agent(url: &str, body: &Value) -> Result<Reply> {
    let mut resp = AGENT
        .post(url)
        .header("Accept", "application/json")
        .header("User-Agent", concat!("anipv/", env!("CARGO_PKG_VERSION")))
        .send_json(body)
        .context("contacting AniList")?;
    let headers = RateHeaders::parse(|name| resp.headers().get(name).and_then(|v| v.to_str().ok()));
    let status = resp.status().as_u16();
    let body = resp.body_mut().read_to_string().context("reading AniList response")?;
    Ok(Reply { status, headers, body })
}

impl Http for Ureq<'_> {
    fn post_json(&self, url: &str, body: &Value) -> Result<Value> {
        if let Some(why) = self.gave_up.get() {
            return Err(Unavailable { cause: anyhow::anyhow!("{why}"), outage: Outage::GaveUpBefore }.into());
        }
        let budget = self.share.get().unwrap_or(self.max_wait);
        let mut waited = self.waited.get();
        let reply = paced(self.limiter, self.clock, budget, &mut waited, self.on_wait, &mut || (self.send)(url, body));
        self.waited.set(waited);
        // A server error is as down as no answer (both were sent twice).
        let reply = match reply {
            Ok(reply) if reply.status < 500 => reply,
            Ok(reply) => return Err(self.unavailable(status_error(reply.status, &reply.body)).into()),
            Err(e) => return Err(self.unavailable(e).into()),
        };
        // Not JSON: something else answered (a proxy, a captive portal).
        let ok = (200..300).contains(&reply.status);
        let Ok(json) = serde_json::from_str::<Value>(&reply.body) else {
            let cause = if ok {
                anyhow::anyhow!("AniList sent a response that isn't JSON")
            } else {
                status_error(reply.status, &reply.body)
            };
            return Err(self.unavailable(cause).into());
        };
        self.failed.set(false);
        // An error AniList answered (a bad request) is about this request.
        if !ok {
            return Err(status_error(reply.status, &reply.body));
        }
        Ok(json)
    }

    fn optional(&self, f: &mut dyn FnMut()) {
        let waited = self.waited.get();
        self.share.set(Some(waited + self.max_wait.saturating_sub(waited) / 2));
        f();
        self.share.set(None);
    }
}

/// A non-success HTTP status as an error, with the start of the body.
/// (A 429 never gets here: [`paced`] waits it out or fails itself.)
fn status_error(status: u16, body: &str) -> anyhow::Error {
    let excerpt: String = body.trim().chars().take(200).collect();
    anyhow::anyhow!("AniList returned HTTP {status}: {excerpt}")
}

/// Fields fetched per anime.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Media {
    /// `AniList` id.
    pub id: u64,
    /// Total episodes (null while airing with unknown length).
    pub episodes: Option<u32>,
    /// `FINISHED`, `RELEASING`, `NOT_YET_RELEASED`, `CANCELLED`, `HIATUS`.
    pub status: Option<String>,
    /// `TV`, `MOVIE`, …
    pub format: Option<String>,
    /// Year of first airing.
    pub season_year: Option<i32>,
    /// Titles.
    pub title: Titles,
    /// Next episode, while airing.
    pub next_airing_episode: Option<Airing>,
    /// Weighted mean of user scores, 0–100 (null until enough users scored it).
    #[serde(default)]
    pub average_score: Option<u8>,
    /// Number of users with the anime on their list.
    #[serde(default)]
    pub popularity: Option<u32>,
    /// Alternative titles (searches only).
    #[serde(default)]
    pub synonyms: Vec<String>,
}

/// Title variants.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Titles {
    /// Romanized title.
    pub romaji: Option<String>,
    /// English title.
    pub english: Option<String>,
    /// Native title (searches only).
    pub native: Option<String>,
}

/// Next airing episode.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Airing {
    /// Episode number.
    pub episode: u32,
    /// Unix time it airs.
    pub airing_at: i64,
}

impl Media {
    /// Every name `AniList` knows this anime by: its titles and synonyms.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        let t = &self.title;
        [&t.romaji, &t.english, &t.native].into_iter().flatten().chain(&self.synonyms).map(String::as_str)
    }

    /// One line about the anime: title, format, year and id.
    pub fn describe(&self) -> String {
        let title = self.title.romaji.as_deref().or(self.title.english.as_deref()).unwrap_or("?");
        super::describe_anime(title, self.format.as_deref(), self.season_year, self.episodes, self.id)
    }

    /// Update a cache row with live data, keeping offline-db facts that `AniList` lacks.
    pub fn apply(&self, m: &mut SeriesMeta, now: i64) {
        m.anilist = Some(self.id);
        if let Some(t) = self.title.romaji.clone().or_else(|| self.title.english.clone()) {
            m.title = Some(t);
        }
        if self.episodes.is_some() {
            m.episodes = self.episodes;
        }
        m.status = self.status.clone().or_else(|| m.status.take());
        m.format = self.format.clone().or_else(|| m.format.take());
        m.year = self.season_year.or(m.year);
        m.next_ep = self.next_airing_episode.as_ref().map(|a| a.episode);
        m.next_airing = self.next_airing_episode.as_ref().map(|a| a.airing_at);
        m.score = self.average_score.or(m.score);
        m.popularity = self.popularity.or(m.popularity);
        m.refreshed_at = Some(now);
    }
}

/// The outcome of a query sent in batches of 50: a failing batch doesn't
/// discard the others.
#[derive(Debug)]
pub struct Batched<T> {
    /// What the batches that succeeded returned.
    pub items: Vec<T>,
    /// Every id sent in a batch that succeeded, whether `AniList` returned it or not.
    pub answered: Vec<u64>,
    /// Why batches failed (an [`Unavailable`] only the first time it's said).
    /// Ids not answered are in neither list above.
    pub errors: Vec<anyhow::Error>,
}

/// Fetch media for the given ids (any number; batched by 50).
pub fn fetch(http: &dyn Http, ids: &[u64]) -> Batched<Media> {
    query_media(http, QUERY, ids, |media| serde_json::from_value(media).context("unexpected AniList response"))
}

const RELATIONS_QUERY: &str = "query ($ids: [Int]) {
  Page(perPage: 50) {
    media(id_in: $ids, type: ANIME) {
      id
      relations { edges { relationType node { id type } } }
    }
  }
}";

/// The direct prequels and sequels (anime only) of an id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relations {
    /// `AniList` id.
    pub id: u64,
    /// Its prequels' ids.
    pub prequels: Vec<u64>,
    /// Its sequels' ids.
    pub sequels: Vec<u64>,
}

/// The direct prequels and sequels (anime only) of each id, for spotting new
/// seasons and grouping franchises. Every requested id that `AniList` knows
/// is returned, with empty lists when it has none.
pub fn fetch_relations(http: &dyn Http, ids: &[u64]) -> Batched<Relations> {
    query_media(http, RELATIONS_QUERY, ids, |media| {
        let mut out = Vec::new();
        for m in media.as_array().into_iter().flatten() {
            let Some(id) = m.get("id").and_then(Value::as_u64) else { continue };
            let related = |kind: &str| -> Vec<u64> {
                m.pointer("/relations/edges")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter(|e| e.get("relationType").and_then(Value::as_str) == Some(kind))
                    .filter(|e| e.pointer("/node/type").and_then(Value::as_str) == Some("ANIME"))
                    .filter_map(|e| e.pointer("/node/id").and_then(Value::as_u64))
                    .collect()
            };
            out.push(Relations { id, prequels: related("PREQUEL"), sequels: related("SEQUEL") });
        }
        Ok(out)
    })
}

/// Run `query` for `ids` in batches of 50, turning each page's `media` array
/// into items with `parse`. A batch whose answer or `parse` fails is reported
/// in [`Batched::errors`] and the other batches are kept. Once the client
/// gives up ([`Outage::stops`]) the rest aren't sent.
fn query_media<T>(
    http: &dyn Http,
    query: &str,
    ids: &[u64],
    mut parse: impl FnMut(Value) -> Result<Vec<T>>,
) -> Batched<T> {
    let mut out = Batched { items: Vec::new(), answered: Vec::new(), errors: Vec::new() };
    for chunk in ids.chunks(50) {
        match page_media(http, &json!({ "query": query, "variables": { "ids": chunk } })).and_then(&mut parse) {
            Ok(mut items) => {
                out.items.append(&mut items);
                out.answered.extend_from_slice(chunk);
            }
            Err(e) => {
                let outage = Outage::of(&e);
                if outage != Some(Outage::GaveUpBefore) {
                    out.errors.push(e);
                }
                if outage.is_some_and(Outage::stops) {
                    break;
                }
            }
        }
    }
    out
}

/// POST `body` and return the page's `media` array, or the error `AniList` sent.
fn page_media(http: &dyn Http, body: &Value) -> Result<Value> {
    let mut v = http.post_json(ENDPOINT, body)?;
    if let Some(errs) = v.get("errors").filter(|e| !e.is_null()) {
        bail!("AniList error: {errs}");
    }
    Ok(v.pointer_mut("/data/Page/media").map_or(Value::Array(vec![]), Value::take))
}

const SEARCH_QUERY: &str = "query ($q: String) {
  Page(perPage: 10) {
    media(search: $q, type: ANIME) {
      id episodes status format seasonYear averageScore popularity
      title { romaji english native }
      synonyms
      nextAiringEpisode { episode airingAt }
    }
  }
}";

/// The anime `AniList` finds for `title`, best first (at most 10). Sends only
/// the title text.
pub fn search(http: &dyn Http, title: &str) -> Result<Vec<Media>> {
    let media = page_media(http, &json!({ "query": SEARCH_QUERY, "variables": { "q": title } }))?;
    serde_json::from_value(media).context("unexpected AniList response")
}

/// A fake `AniList` for tests: answers every query with the requested ids as
/// finished 10-episode shows (or, `unknown`, with nothing, as for ids it does
/// not know), records which ids were asked for, and fails request number
/// `fail_on` (counting from 1), as [`Ureq`] does: once, or (`gives_up`) for
/// good, failing every later request unsent.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct Echo {
    /// Every id asked for, in order.
    pub asked: RefCell<Vec<u64>>,
    fail_on: Option<usize>,
    gives_up: bool,
    unknown: bool,
    calls: Cell<usize>,
}

#[cfg(test)]
impl Echo {
    /// An `AniList` that knows none of the ids it is asked about.
    pub fn unknown() -> Self {
        Self { unknown: true, ..Self::default() }
    }

    /// An `AniList` whose request number `n` fails, once.
    pub fn failing(n: usize) -> Self {
        Self { fail_on: Some(n), ..Self::default() }
    }

    /// An `AniList` the client gives up on at request number `n`.
    pub fn giving_up(n: usize) -> Self {
        Self { fail_on: Some(n), gives_up: true, ..Self::default() }
    }
}

#[cfg(test)]
impl Http for Echo {
    fn post_json(&self, url: &str, body: &Value) -> Result<Value> {
        assert_eq!(url, ENDPOINT);
        self.calls.set(self.calls.get() + 1);
        let outage = match self.fail_on.map(|n| self.calls.get().cmp(&n)) {
            Some(std::cmp::Ordering::Equal) if self.gives_up => Some(Outage::GaveUp),
            Some(std::cmp::Ordering::Equal) => Some(Outage::Once),
            Some(std::cmp::Ordering::Greater) if self.gives_up => Some(Outage::GaveUpBefore),
            _ => None,
        };
        if let Some(outage) = outage {
            return Err(Unavailable::test("connection reset", outage).into());
        }
        let ids: Vec<u64> = serde_json::from_value(body["variables"]["ids"].clone()).unwrap_or_default();
        self.asked.borrow_mut().extend(&ids);
        let media: Vec<Value> = if self.unknown {
            Vec::new()
        } else {
            ids.iter()
                .map(|id| {
                    json!({"id": id, "episodes": 10, "status": "FINISHED", "format": "TV",
                   "averageScore": 81, "popularity": 12_345,
                   "title": {"romaji": "Refreshed"}, "nextAiringEpisode": null, "relations": {"edges": []}})
                })
                .collect()
        };
        Ok(json!({"data": {"Page": {"media": media}}}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct Fake {
        calls: RefCell<Vec<Value>>,
        reply: Value,
    }

    impl Http for Fake {
        fn post_json(&self, url: &str, body: &Value) -> Result<Value> {
            assert_eq!(url, ENDPOINT);
            self.calls.borrow_mut().push(body.clone());
            Ok(self.reply.clone())
        }
    }

    fn reply() -> Value {
        json!({"data": {"Page": {"media": [
            {"id": 21, "episodes": null, "status": "RELEASING", "format": "TV", "seasonYear": 1999,
             "averageScore": null, "popularity": 612_000,
             "title": {"romaji": "ONE PIECE", "english": "One Piece"},
             "nextAiringEpisode": {"episode": 1181, "airingAt": 1_791_500_000}}
        ]}}})
    }

    #[test]
    fn ids_and_urls() {
        assert_eq!(parse_ref("21"), Some(21));
        assert_eq!(parse_ref(" https://anilist.co/anime/21/ONE-PIECE/ "), Some(21));
        assert_eq!(parse_ref("https://anilist.co/anime/21"), Some(21));
        assert_eq!(parse_ref("https://myanimelist.net/anime/21"), None);
        assert_eq!(parse_ref("one piece"), None);
        for ok in [
            "anilist.co/anime/21",
            "http://anilist.co/anime/21",
            "https://www.anilist.co/anime/21",
            "www.anilist.co/anime/21/Cowboy-Bebop",
            "https://anilist.co/anime/21/Cowboy-Bebop/",
            "https://anilist.co/anime/21/",
            "https://anilist.co/anime/21?x",
            "https://anilist.co/anime/21#x",
            "https://anilist.co/anime/21/Cowboy-Bebop?x=1#y",
        ] {
            assert_eq!(parse_ref(ok), Some(21), "{ok}");
        }
        for bad in [
            "https://anilist.co/manga/21",
            "anilist.co/manga/21/Cowboy-Bebop",
            "https://evil.example/anime/21",
            "https://anilist.co.evil.example/anime/21",
            "https://notanilist.co/anime/21",
            "https://anilist.co/anime/",
            "https://anilist.co/anime/+21",
            "https://anilist.co/anime/21x",
            "ftp://anilist.co/anime/21",
            "+21",
            "",
        ] {
            assert_eq!(parse_ref(bad), None, "{bad}");
        }
    }

    #[test]
    fn status_errors_show_the_body() {
        let err = status_error(502, "<html>Bad Gateway</html>\n").to_string();
        assert_eq!(err, "AniList returned HTTP 502: <html>Bad Gateway</html>");
        let err = status_error(500, &"x".repeat(1000)).to_string();
        assert!(err.ends_with(&format!(": {x}", x = "x".repeat(200))), "long bodies are cut to 200 chars");
    }

    #[test]
    fn batches_and_parses() {
        let fake = Fake { calls: RefCell::new(vec![]), reply: reply() };
        let ids: Vec<u64> = (1..=120).collect();
        let media = fetch(&fake, &ids).items;
        assert_eq!(fake.calls.borrow().len(), 3, "120 ids → 3 requests");
        assert_eq!(fake.calls.borrow()[2]["variables"]["ids"].as_array().unwrap().len(), 20);
        assert_eq!(media.len(), 3);
        assert_eq!(media[0].next_airing_episode, Some(Airing { episode: 1181, airing_at: 1_791_500_000 }));
    }

    #[test]
    fn errors_surface() {
        let fake = Fake { calls: RefCell::new(vec![]), reply: json!({"errors": [{"message": "boom"}], "data": null}) };
        let res = fetch(&fake, &[1]);
        assert_eq!(res.errors.len(), 1);
        assert!(res.answered.is_empty());
    }

    #[test]
    fn apply_keeps_known_totals() {
        let m: Media = serde_json::from_value(reply()["data"]["Page"]["media"][0].clone()).unwrap();
        let mut row =
            SeriesMeta { series: "one piece".into(), episodes: Some(1168), score: Some(87), ..SeriesMeta::default() };
        m.apply(&mut row, 9);
        assert_eq!(row.episodes, Some(1168), "null total doesn't erase the offline count");
        assert_eq!(row.score, Some(87), "null score doesn't erase the offline one");
        assert_eq!(row.popularity, Some(612_000));
        assert_eq!(row.next_ep, Some(1181));
        assert_eq!(row.status.as_deref(), Some("RELEASING"));
        assert_eq!(row.title.as_deref(), Some("ONE PIECE"));
        assert_eq!(row.refreshed_at, Some(9));
    }

    #[test]
    fn relations_are_parsed() {
        let reply = json!({"data": {"Page": {"media": [
            {"id": 199_111, "relations": {"edges": [
                {"relationType": "PREQUEL", "node": {"id": 182_309, "type": "ANIME"}},
                {"relationType": "SEQUEL", "node": {"id": 5, "type": "ANIME"}},
                {"relationType": "PREQUEL", "node": {"id": 7, "type": "MANGA"}}
            ]}},
            {"id": 100_922, "relations": {"edges": []}}
        ]}}});
        let fake = Fake { calls: RefCell::new(vec![]), reply };
        let got = fetch_relations(&fake, &[199_111, 100_922]).items;
        let rel = |id, prequels, sequels| Relations { id, prequels, sequels };
        assert_eq!(got, vec![rel(199_111, vec![182_309], vec![5]), rel(100_922, vec![], vec![])]);
    }

    /// docs/anilist.md R14: Regression: a failing batch no longer discards the batches that
    /// succeeded, before or after it. Once the client gives up, the rest
    /// aren't sent.
    #[test]
    fn a_failed_batch_keeps_the_others() {
        let ids: Vec<u64> = (1..=120).collect();
        let res = fetch(&Echo::failing(2), &ids);
        assert_eq!(res.errors.len(), 1);
        let expect: Vec<u64> = (1..=50).chain(101..=120).collect();
        assert_eq!(res.items.iter().map(|m| m.id).collect::<Vec<_>>(), expect);
        assert_eq!(res.answered, expect);
        let res = fetch_relations(&Echo::failing(2), &ids);
        assert_eq!(res.errors.len(), 1);
        assert_eq!(res.items.iter().map(|r| r.id).collect::<Vec<_>>(), expect);
        assert_eq!(res.answered, expect);

        let fake = Echo::giving_up(2);
        let res = fetch(&fake, &ids);
        assert_eq!(res.errors.len(), 1);
        assert_eq!(res.answered, (1..=50).collect::<Vec<_>>());
        assert_eq!(fake.asked.borrow().len(), 50, "the third batch isn't sent");
    }

    /// docs/anilist.md R15: An error `AniList` answers for one batch, as a GraphQL error or an
    /// HTTP status, doesn't stop the others; neither does an [`Unavailable`]
    /// the client didn't give up on. One said already isn't said again.
    #[test]
    fn an_answered_error_keeps_going() {
        let fake = Fake { calls: RefCell::new(vec![]), reply: json!({"errors": [{"message": "boom"}], "data": null}) };
        let ids: Vec<u64> = (1..=120).collect();
        let res = fetch(&fake, &ids);
        assert_eq!((res.errors.len(), fake.calls.borrow().len()), (3, 3));
        let failing = |error| {
            let http = Failing { calls: Cell::new(0), error };
            let res = fetch(&http, &ids);
            (res.errors.len(), http.calls.get())
        };
        assert_eq!(failing(|| status_error(404, "Not Found")), (3, 3));
        assert_eq!(failing(|| Unavailable::test("down", Outage::Once).into()), (3, 3));
        assert_eq!(failing(|| Unavailable::test("down", Outage::GaveUp).into()), (1, 1));
        assert_eq!(failing(|| Unavailable::test("down", Outage::GaveUpBefore).into()), (0, 1));
    }

    /// Fails every request with `error`, and counts them.
    struct Failing {
        calls: Cell<usize>,
        error: fn() -> anyhow::Error,
    }

    impl Http for Failing {
        fn post_json(&self, _url: &str, _body: &Value) -> Result<Value> {
            self.calls.set(self.calls.get() + 1);
            Err((self.error)())
        }
    }

    /// A [`Ureq`] on a fake clock and a limiter of its own, answering every
    /// request with `send`; what it slept is in the clock.
    fn ureq_test<'a>(
        clock: &'a crate::meta::ratelimit::tests::Fake,
        limiter: &'a Mutex<Limiter>,
        send: &'a dyn Fn(&str, &Value) -> Result<Reply>,
    ) -> Ureq<'a> {
        Ureq { send, limiter, clock, ..Ureq::new(INTERACTIVE_WAIT, &|_| {}) }
    }

    /// docs/anilist.md R10, R13: One failed request (sent twice) isn't an outage; two in a row are,
    /// whether no answer or a server error: the client gives up, and later
    /// requests fail at once, without being sent.
    #[test]
    fn the_client_gives_up_after_two_failures() {
        let refused = |_: &str, _: &Value| -> Result<Reply> { bail!("contacting AniList: connection refused") };
        let down = |_: &str, _: &Value| Ok(Reply { status: 503, headers: RateHeaders::default(), body: "down".into() });
        for send in [&refused as &dyn Fn(&str, &Value) -> Result<Reply>, &down] {
            let (clock, limiter) = (crate::meta::ratelimit::tests::Fake::new(), Mutex::default());
            let ureq = ureq_test(&clock, &limiter, send);
            let ask = || ureq.post_json(ENDPOINT, &json!({})).unwrap_err();
            let first = ask();
            assert_eq!(Outage::of(&first), Some(Outage::Once), "{first:#}");
            let second = ask();
            assert_eq!(Outage::of(&second), Some(Outage::GaveUp));
            let slept = clock.slept.borrow().len();
            let third = ask();
            assert_eq!(Outage::of(&third), Some(Outage::GaveUpBefore));
            assert_eq!(third.to_string(), format!("{second:#}"));
            assert_eq!(clock.slept.borrow().len(), slept, "not sent, so no waiting");
        }
    }

    /// docs/anilist.md R10: An answer that isn't JSON is something else answering (a proxy, a
    /// captive portal): it counts towards giving up, as no answer does. A
    /// JSON error is `AniList` answering, and resets the count.
    #[test]
    fn answers_that_arent_json_give_up() {
        let reply = |status, body: &str| Ok(Reply { status, headers: RateHeaders::default(), body: body.into() });
        let portal = |_: &str, _: &Value| reply(200, "<html>Log in to the Wi-Fi</html>");
        let (clock, limiter) = (crate::meta::ratelimit::tests::Fake::new(), Mutex::default());
        let ureq = ureq_test(&clock, &limiter, &portal);
        let outage = || Outage::of(&ureq.post_json(ENDPOINT, &json!({})).unwrap_err());
        assert_eq!((outage(), outage()), (Some(Outage::Once), Some(Outage::GaveUp)));

        let calls = Cell::new(0);
        let alternating = |_: &str, _: &Value| {
            calls.set(calls.get() + 1);
            if calls.get() % 2 == 1 { reply(403, "<html>Forbidden</html>") } else { reply(404, r#"{"errors":[]}"#) }
        };
        let (clock, limiter) = (crate::meta::ratelimit::tests::Fake::new(), Mutex::default());
        let ureq = ureq_test(&clock, &limiter, &alternating);
        let outages: Vec<_> = (0..4).map(|_| Outage::of(&ureq.post_json(ENDPOINT, &json!({})).unwrap_err())).collect();
        assert_eq!(outages, [Some(Outage::Once), None, Some(Outage::Once), None]);
    }

    /// docs/anilist.md R12: Optional requests don't count towards giving up: a failed title search
    /// and then one failed airing batch is one failure, not two.
    #[test]
    fn optional_failures_dont_count() {
        let refused = |_: &str, _: &Value| -> Result<Reply> { bail!("contacting AniList: connection refused") };
        let (clock, limiter) = (crate::meta::ratelimit::tests::Fake::new(), Mutex::default());
        let ureq = ureq_test(&clock, &limiter, &refused);
        let outage = || Outage::of(&ureq.post_json(ENDPOINT, &json!({})).unwrap_err());
        let mut optional = Vec::new();
        ureq.optional(&mut || optional.extend([outage(), outage()]));
        assert_eq!(optional, [Some(Outage::Once), Some(Outage::Once)]);
        assert_eq!((outage(), outage()), (Some(Outage::Once), Some(Outage::GaveUp)));
    }

    /// docs/anilist.md R8: optional requests may wait out only half of what
    /// is left: a 70 s timeout is too long for a title search's 60 s, but
    /// the airing refresh after it waits it out.
    #[test]
    fn title_searches_get_half_the_budget() {
        let calls = Cell::new(0);
        let send = |_: &str, _: &Value| {
            calls.set(calls.get() + 1);
            let (status, retry_after, body) =
                if calls.get() == 1 { (429, Some(70), "") } else { (200, None, r#"{"data":{}}"#) };
            Ok(Reply { status, headers: RateHeaders { retry_after, ..RateHeaders::default() }, body: body.into() })
        };
        let (clock, limiter) = (crate::meta::ratelimit::tests::Fake::new(), Mutex::default());
        let ureq = ureq_test(&clock, &limiter, &send);
        let mut searched = None;
        ureq.optional(&mut || searched = Some(ureq.post_json(ENDPOINT, &json!({}))));
        let e = searched.unwrap().unwrap_err();
        assert!(Unavailable::rate_limited(&e) && Outage::of(&e) == Some(Outage::Once), "{e:#}");
        assert_eq!(ureq.post_json(ENDPOINT, &json!({})).unwrap(), json!({"data": {}}));
        assert_eq!((calls.get(), clock.slept.take()), (2, vec![Duration::from_secs(70)]));
    }

    /// docs/anilist.md R11, R12: A 429 outlasting the budget gives up, except for optional requests,
    /// which only had a share of it.
    #[test]
    fn a_long_rate_limit_gives_up_unless_optional() {
        let limited = |_: &str, _: &Value| {
            let headers = RateHeaders { retry_after: Some(300), ..RateHeaders::default() };
            Ok(Reply { status: 429, headers, body: String::new() })
        };
        let (clock, limiter) = (crate::meta::ratelimit::tests::Fake::new(), Mutex::default());
        let ureq = ureq_test(&clock, &limiter, &limited);
        let mut optional = None;
        ureq.optional(&mut || optional = Outage::of(&ureq.post_json(ENDPOINT, &json!({})).unwrap_err()));
        assert_eq!(optional, Some(Outage::Once));
        let e = ureq.post_json(ENDPOINT, &json!({})).unwrap_err();
        assert_eq!(Outage::of(&e), Some(Outage::GaveUp));
        assert!(e.to_string().starts_with("AniList rate limit: try again in "), "{e}");
    }
}
