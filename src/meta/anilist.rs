//! Anonymous `AniList` GraphQL queries for airing schedules.
//!
//! Only public anime data is requested, by id, in batches of 50. No account,
//! token or information about the user is sent.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::index::db::SeriesMeta;

/// GraphQL endpoint.
pub const ENDPOINT: &str = "https://graphql.anilist.co";

const QUERY: &str = "query ($ids: [Int]) {
  Page(perPage: 50) {
    media(id_in: $ids, type: ANIME) {
      id episodes status format seasonYear
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
    /// POST a JSON body and return the JSON response.
    fn post_json(&self, url: &str, body: &Value) -> Result<Value>;
}

/// Real HTTP client.
pub struct Ureq;

/// One agent for the whole process, so a sync's batches reuse the connection.
static AGENT: std::sync::LazyLock<ureq::Agent> = std::sync::LazyLock::new(|| {
    ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(20)))
        .http_status_as_error(false)
        .build()
        .into()
});

impl Http for Ureq {
    fn post_json(&self, url: &str, body: &Value) -> Result<Value> {
        let mut resp = AGENT
            .post(url)
            .header("Accept", "application/json")
            .header("User-Agent", concat!("anipv/", env!("CARGO_PKG_VERSION")))
            .send_json(body)
            .context("contacting AniList")?;
        let status = resp.status();
        // Errors (rate limits, proxies, outages) often come as HTML or plain
        // text, so check the status before expecting JSON.
        let text = resp.body_mut().read_to_string().context("reading AniList response")?;
        check_status(status.as_u16(), &text)?;
        serde_json::from_str(&text).context("AniList sent a response that isn't JSON")
    }
}

/// Turn a non-success HTTP status into an error, with the start of the body.
fn check_status(status: u16, body: &str) -> Result<()> {
    if status == 429 {
        bail!("AniList rate limit hit; try again in a minute");
    }
    if !(200..300).contains(&status) {
        let excerpt: String = body.trim().chars().take(200).collect();
        bail!("AniList returned HTTP {status}: {excerpt}");
    }
    Ok(())
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
}

/// Title variants.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Titles {
    /// Romanized title.
    pub romaji: Option<String>,
    /// English title.
    pub english: Option<String>,
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
        m.refreshed_at = Some(now);
    }
}

/// The outcome of a query sent in batches of 50: one failing batch doesn't
/// discard the others.
#[derive(Debug)]
pub struct Batched<T> {
    /// What the batches that succeeded returned.
    pub items: Vec<T>,
    /// Every id sent in a batch that succeeded, whether `AniList` returned it or not.
    pub answered: Vec<u64>,
    /// Why each failed batch failed. Its ids are in neither list above.
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

/// The direct prequels (anime only) of each id, for spotting new seasons.
/// Every requested id that `AniList` knows is returned, with an empty list
/// when it has no prequel.
pub fn fetch_prequels(http: &dyn Http, ids: &[u64]) -> Batched<(u64, Vec<u64>)> {
    query_media(http, RELATIONS_QUERY, ids, |media| {
        let mut out = Vec::new();
        for m in media.as_array().into_iter().flatten() {
            let Some(id) = m.get("id").and_then(Value::as_u64) else { continue };
            let prequels = m
                .pointer("/relations/edges")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|e| e.get("relationType").and_then(Value::as_str) == Some("PREQUEL"))
                .filter(|e| e.pointer("/node/type").and_then(Value::as_str) == Some("ANIME"))
                .filter_map(|e| e.pointer("/node/id").and_then(Value::as_u64))
                .collect();
            out.push((id, prequels));
        }
        Ok(out)
    })
}

/// Run `query` for `ids` in batches of 50, turning each page's `media` array
/// into items with `parse`. A batch whose request or `parse` fails is reported
/// in [`Batched::errors`] and the other batches are kept.
fn query_media<T>(
    http: &dyn Http,
    query: &str,
    ids: &[u64],
    mut parse: impl FnMut(Value) -> Result<Vec<T>>,
) -> Batched<T> {
    let mut out = Batched { items: Vec::new(), answered: Vec::new(), errors: Vec::new() };
    for chunk in ids.chunks(50) {
        match query_batch(http, query, chunk).and_then(&mut parse) {
            Ok(mut items) => {
                out.items.append(&mut items);
                out.answered.extend_from_slice(chunk);
            }
            Err(e) => out.errors.push(e),
        }
    }
    out
}

/// One batch of [`query_media`]: the page's `media` array.
fn query_batch(http: &dyn Http, query: &str, ids: &[u64]) -> Result<Value> {
    let body = json!({ "query": query, "variables": { "ids": ids } });
    let v = http.post_json(ENDPOINT, &body)?;
    if let Some(errs) = v.get("errors").filter(|e| !e.is_null()) {
        bail!("AniList error: {errs}");
    }
    Ok(v.pointer("/data/Page/media").cloned().unwrap_or(Value::Array(vec![])))
}

/// A fake `AniList` for tests: answers every query with the requested ids as
/// finished 10-episode shows (or, `unknown`, with nothing, as for ids it does
/// not know), records which ids were asked for, and fails request number
/// `fail_on` (counting from 1).
#[cfg(test)]
#[derive(Default)]
pub(crate) struct Echo {
    /// Every id asked for, in order.
    pub asked: std::cell::RefCell<Vec<u64>>,
    fail_on: Option<usize>,
    unknown: bool,
    calls: std::cell::Cell<usize>,
}

#[cfg(test)]
impl Echo {
    /// An `AniList` that knows none of the ids it is asked about.
    pub fn unknown() -> Self {
        Self { unknown: true, ..Self::default() }
    }

    /// An `AniList` whose request number `n` fails.
    pub fn failing(n: usize) -> Self {
        Self { fail_on: Some(n), ..Self::default() }
    }
}

#[cfg(test)]
impl Http for Echo {
    fn post_json(&self, url: &str, body: &Value) -> Result<Value> {
        assert_eq!(url, ENDPOINT);
        self.calls.set(self.calls.get() + 1);
        if self.fail_on == Some(self.calls.get()) {
            bail!("connection reset");
        }
        let ids: Vec<u64> = serde_json::from_value(body["variables"]["ids"].clone()).unwrap_or_default();
        self.asked.borrow_mut().extend(&ids);
        let media: Vec<Value> = if self.unknown {
            Vec::new()
        } else {
            ids.iter()
                .map(|id| {
                    json!({"id": id, "episodes": 10, "status": "FINISHED", "format": "TV",
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
    fn errors_are_checked_before_parsing() {
        let err = check_status(429, "<html>Too Many Requests</html>").unwrap_err().to_string();
        assert!(err.contains("rate limit"), "{err}");
        let err = check_status(502, "<html>Bad Gateway</html>\n").unwrap_err().to_string();
        assert_eq!(err, "AniList returned HTTP 502: <html>Bad Gateway</html>");
        let err = check_status(500, &"x".repeat(1000)).unwrap_err().to_string();
        assert!(err.ends_with(&format!(": {x}", x = "x".repeat(200))), "long bodies are cut to 200 chars");
        check_status(200, "{}").unwrap();
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
        let mut row = SeriesMeta { series: "one piece".into(), episodes: Some(1168), ..SeriesMeta::default() };
        m.apply(&mut row, 9);
        assert_eq!(row.episodes, Some(1168), "null total doesn't erase the offline count");
        assert_eq!(row.next_ep, Some(1181));
        assert_eq!(row.status.as_deref(), Some("RELEASING"));
        assert_eq!(row.title.as_deref(), Some("ONE PIECE"));
        assert_eq!(row.refreshed_at, Some(9));
    }

    #[test]
    fn prequels_are_parsed() {
        let reply = json!({"data": {"Page": {"media": [
            {"id": 199_111, "relations": {"edges": [
                {"relationType": "PREQUEL", "node": {"id": 182_309, "type": "ANIME"}},
                {"relationType": "SEQUEL", "node": {"id": 5, "type": "ANIME"}},
                {"relationType": "PREQUEL", "node": {"id": 7, "type": "MANGA"}}
            ]}},
            {"id": 100_922, "relations": {"edges": []}}
        ]}}});
        let fake = Fake { calls: RefCell::new(vec![]), reply };
        let got = fetch_prequels(&fake, &[199_111, 100_922]).items;
        assert_eq!(got, vec![(199_111, vec![182_309]), (100_922, vec![])]);
    }

    /// Regression: a failing batch no longer discards the batches that succeeded.
    #[test]
    fn a_failed_batch_keeps_the_others() {
        let ids: Vec<u64> = (1..=120).collect();
        let res = fetch(&Echo::failing(2), &ids);
        assert_eq!(res.errors.len(), 1);
        let expect: Vec<u64> = (1..=50).chain(101..=120).collect();
        assert_eq!(res.items.iter().map(|m| m.id).collect::<Vec<_>>(), expect);
        assert_eq!(res.answered, expect);
        let res = fetch_prequels(&Echo::failing(2), &ids);
        assert_eq!(res.errors.len(), 1);
        assert_eq!(res.items.iter().map(|p| p.0).collect::<Vec<_>>(), expect);
        assert_eq!(res.answered, expect);
    }
}
