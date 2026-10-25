//! Episodes numbered on through several `AniList` entries: Kusuriya no
//! Hitorigoto 01–52 on disk is 24 of the first season, 24 of the second and
//! the first 4 of the third. [`split`] finds the entries a series' episode
//! numbers run through, from the cached prequel/sequel links and episode
//! counts, so each episode has its home (episode 25 is the second season's
//! first). Nothing is recorded: items keep their absolute numbers.

use std::collections::HashSet;

use crate::app::Graph;
use crate::index::db::SeriesMeta;

/// How many links a split follows back from the linked entry, or on from
/// where it starts, at most.
const MAX_STEPS: usize = 64;

/// One `AniList` entry a series' episodes run through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitPart {
    /// Its data (`anilist` is its id).
    pub meta: SeriesMeta,
    /// The series' number of its first episode (`25`).
    pub first: u32,
}

impl SplitPart {
    /// Its `AniList` id.
    pub fn id(&self) -> u64 {
        self.meta.anilist.unwrap_or_default()
    }

    /// What it is called.
    pub fn title(&self) -> &str {
        self.meta.title.as_deref().unwrap_or_default()
    }

    /// The series' number of its last episode, when its length is known.
    pub fn last(&self) -> Option<u32> {
        Some(self.first + self.meta.episodes?.checked_sub(1)?)
    }

    /// Its airing summary, in the series' numbers (see
    /// [`crate::library::airing_of`]).
    pub fn airing(&self, now: i64) -> Option<(String, bool)> {
        crate::library::airing_of(&self.meta, self.first - 1, now)
    }

    /// Its own number of the series' episode `n` (`25` → `1`).
    pub fn own_number(&self, n: u32) -> u32 {
        n.saturating_sub(self.first) + 1
    }
}

/// The entries a series' episodes run through, in order (see [`split`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Split {
    /// The entries, from the one holding episode 1.
    pub parts: Vec<SplitPart>,
    /// The part airing now: the first not finished, else the last.
    pub current: usize,
    /// More is announced after the parts (an entry of the run not finished
    /// airing that holds none of the episodes yet, like a part 2 to come).
    pub more: bool,
    /// The entries of the run another series links, passed over, where they
    /// start in this series' numbers (see [`crate::library::Library`]: a
    /// season you have apart is folded in there).
    pub held: Vec<SplitPart>,
}

impl Split {
    /// The part holding the series' episode `n` (an episode 0 or a number
    /// before the first part: the first).
    pub fn part_of(&self, n: u32) -> usize {
        self.parts.iter().rposition(|p| p.first <= n).unwrap_or(0)
    }

    /// [`Split::part_of`], but `None` for a number between two parts: a
    /// season another series holds (Kusuriya's 25–48, between its first
    /// season and its third). Past the last part, still the last.
    pub fn holding(&self, n: u32) -> Option<usize> {
        let at = self.part_of(n);
        let last = at + 1 == self.parts.len();
        (last || self.parts[at].last().is_none_or(|end| n <= end)).then_some(at)
    }

    /// The part airing now.
    pub fn current_part(&self) -> &SplitPart {
        &self.parts[self.current]
    }

    /// Every `AniList` id it covers.
    pub fn ids(&self) -> impl Iterator<Item = u64> + '_ {
        self.parts.iter().map(SplitPart::id)
    }
}

/// An entry of a run and the series' number of its first episode.
type Start = (u64, u32);

/// Formats whose entries number on from each other: TV and web series. An
/// entry of another format (a film, an OVA) only runs on into one of its own.
fn family(format: Option<&str>) -> Option<&str> {
    match format {
        Some("TV" | "TV_SHORT" | "ONA") => Some("series"),
        other => other,
    }
}

/// The entries the episodes of a series linked to `linked` (with data
/// `linked_meta`), numbered `range` (lowest, highest), run through: `None`
/// when they all fit it (a 12-episode season with 12 on disk).
///
/// From the linked entry, the run follows prequel and sequel links to
/// entries of the same family of formats (see `family`) with cached data, as
/// long as there is one such link. An entry another series links (`taken`
/// gives its data: a season you have apart) is passed over: its episodes
/// count, but it holds none of these. Where the linked entry starts in the series'
/// numbers:
/// - `numbering`, when you set it (see [`crate::events::EventBody::Numbering`]);
/// - else as few entries before it as it takes for the counts to hold the
///   highest episode: Steel Ball Run's twelve episodes linked to its
///   11-episode second part start with the one-episode first;
/// - else, a *lead-in*: when the series starts at episode 1 and the linked
///   entry follows a one-episode entry that aired its year or the year
///   before, that one is episode 1.
///
/// An entry of unknown length (airing) ends the run and holds everything
/// after it.
pub fn split<'a>(
    linked: u64,
    linked_meta: &'a SeriesMeta,
    range: (u32, u32),
    numbering: Option<u32>,
    graph: &'a Graph,
    taken: impl Fn(u64) -> Option<&'a SeriesMeta>,
) -> Option<Split> {
    let (lowest, top) = range;
    let kind = family(linked_meta.format.as_deref())?;
    let meta = |id: u64| -> Option<&SeriesMeta> {
        if id == linked {
            return Some(linked_meta);
        }
        let m = taken(id).or_else(|| graph.media.get(&id))?;
        (family(m.format.as_deref()) == Some(kind)).then_some(m)
    };
    let held_apart = |id: u64| id != linked && taken(id).is_some();
    let count = |id: u64| meta(id).and_then(|m| m.episodes);
    // The one link of the family, if there is exactly one.
    let one = |ids: Option<&Vec<u64>>| -> Option<u64> {
        let mut found = ids.into_iter().flatten().copied().filter(|&id| meta(id).is_some());
        let first = found.next()?;
        found.next().is_none().then_some(first)
    };
    let prequel = |id: u64| one(graph.prequels.get(&id));
    let sequel = |id: u64| one(graph.sequels.get(&id).map(|(ids, _)| ids));

    // Whether the run from `start` holds episodes up to `top`, and how many
    // come before the linked entry in it (`None`: it doesn't go through it).
    let holds = |start: u64| -> (bool, Option<u32>) {
        let (mut id, mut held, mut before, mut seen) = (start, 0, None, HashSet::new());
        loop {
            if id == linked {
                before = Some(held);
            }
            let Some(c) = count(id) else { return (true, before) };
            held += c;
            match sequel(id).filter(|next| seen.insert(*next) && seen.len() < MAX_STEPS) {
                Some(next) if held < top || before.is_none() => id = next,
                _ => return (held >= top, before),
            }
        }
    };
    let first = numbering.map_or_else(
        || {
            // The first start that holds the episodes and puts the lowest on
            // disk in the linked entry (a season numbered on, 13–24, starts
            // after its prequel's 12), else the first that holds them.
            let mut start = linked;
            let mut seen = HashSet::from([linked]);
            let mut fitting = None;
            loop {
                if let (true, Some(before)) = holds(start) {
                    let first = before + 1;
                    let ends = count(linked).map(|c| first + c);
                    if lowest >= first && ends.is_none_or(|end| lowest < end) {
                        break first;
                    }
                    fitting.get_or_insert(first);
                }
                match prequel(start).filter(|p| seen.insert(*p) && seen.len() < MAX_STEPS) {
                    Some(p) => start = p,
                    None => break fitting.unwrap_or(1),
                }
            }
        },
        |f| f.max(1),
    );
    // A one-episode lead-in aired just before.
    // Only while the episodes don't fill the linked entry, or it is still
    // airing (Steel Ball Run's eleventh would fill it a week before its
    // twelfth): a finished season numbered by itself (12 of 12) has no
    // episode to spare for one.
    let airing = linked_meta.status.as_deref() == Some("RELEASING");
    let unfilled = airing || linked_meta.episodes.is_none_or(|n| top < n);
    let lead_in = numbering.is_none() && first == 1 && lowest == 1 && unfilled && {
        let year = linked_meta.year;
        // Of the same format (Steel Ball Run's ONA stages), not a pilot of
        // another (a one-episode ONA before a TV season).
        prequel(linked).filter(|p| !held_apart(*p)).and_then(meta).is_some_and(|p| {
            p.episodes == Some(1)
                && p.format == linked_meta.format
                && p.year.zip(year).is_some_and(|(py, y)| py == y || py + 1 == y)
        })
    };
    let first = if lead_in { 2 } else { first };

    // Back from the linked entry, the entries holding the episodes before it.
    let mut before: Vec<(u64, u32)> = Vec::new();
    let (mut need, mut at) = (first - 1, linked);
    while need > 0 {
        let Some((p, c)) = prequel(at).filter(|p| !before.iter().any(|b| b.0 == *p)).and_then(|p| Some((p, count(p)?)))
        else {
            break;
        };
        before.push((p, need.saturating_sub(c) + 1));
        need = need.saturating_sub(c);
        at = p;
    }
    if let Some(earliest) = before.last_mut() {
        earliest.1 = 1;
    }
    before.reverse();
    // On from it, the entries holding the episodes after it.
    let mut ids = before;
    let (mut id, mut start) = (linked, first);
    loop {
        ids.push((id, start));
        let Some(c) = count(id) else { break };
        start += c;
        match sequel(id).filter(|next| !ids.iter().any(|i| i.0 == *next) && ids.len() < MAX_STEPS) {
            Some(next) if start <= top => id = next,
            _ => break,
        }
    }
    let part = |(id, first): (u64, u32)| {
        Some(SplitPart { meta: SeriesMeta { anilist: Some(id), ..meta(id)?.clone() }, first })
    };
    let (held, mut ids): (Vec<Start>, Vec<Start>) = ids.into_iter().partition(|(id, _)| held_apart(*id));
    let held: Vec<SplitPart> = held.into_iter().filter_map(part).collect();
    // Not the entries before the series' own episodes (a season numbered
    // on, 13–24: its first season isn't a part of it).
    let skip = ids.iter().skip(1).take_while(|(_, next)| *next <= lowest).count();
    ids.drain(..skip);
    // One part is a split only when it isn't the linked entry: the files are
    // another season's than the one linked (Jujutsu Kaisen linked to S1,
    // its files 25–47).
    if ids.len() < 2 && ids.first().is_none_or(|(id, _)| *id == linked) {
        return None;
    }
    let parts: Vec<SplitPart> = ids.into_iter().map(part).collect::<Option<_>>()?;
    let current = parts.iter().position(|p| !p.meta.is_finished()).unwrap_or(parts.len() - 1);
    // Not a next season another series holds: that one is that series'.
    let after = parts.last().and_then(|p| sequel(p.id())).filter(|id| !held_apart(*id)).and_then(meta);
    let more = after.is_some_and(|m| !m.is_finished());
    Some(Split { parts, current, more, held })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: u64, format: &str, episodes: Option<u32>, status: &str) -> (u64, SeriesMeta) {
        let m = SeriesMeta {
            anilist: Some(id),
            title: Some(format!("Entry {id}")),
            format: Some(format.into()),
            episodes,
            status: Some(status.into()),
            ..SeriesMeta::default()
        };
        (id, m)
    }

    /// A chain of entries, each the sequel of the one before.
    fn chain(entries: &[(u64, SeriesMeta)]) -> Graph {
        let mut g = Graph::default();
        for w in entries.windows(2) {
            g.sequels.entry(w[0].0).or_insert((Vec::new(), 0)).0.push(w[1].0);
            g.prequels.entry(w[1].0).or_default().push(w[0].0);
        }
        g.media = entries.iter().cloned().collect();
        g
    }

    fn firsts(s: &Split) -> Vec<(u64, u32)> {
        s.parts.iter().map(|p| (p.id(), p.first)).collect()
    }

    /// Kusuriya no Hitorigoto: 24 + 24 + 12 (airing) + 12 (to come), 52 on
    /// disk under the first season.
    fn kusuriya() -> Graph {
        chain(&[
            entry(161_645, "TV", Some(24), "FINISHED"),
            entry(176_301, "TV", Some(24), "FINISHED"),
            entry(195_516, "TV", Some(12), "RELEASING"),
            entry(200_927, "TV", Some(12), "NOT_YET_RELEASED"),
        ])
    }

    #[test]
    fn episodes_numbered_on_run_through_the_seasons() {
        let g = kusuriya();
        let s = split(161_645, &g.media[&161_645], (1, 52), None, &g, |_| None).unwrap();
        assert_eq!(firsts(&s), [(161_645, 1), (176_301, 25), (195_516, 49)]);
        assert_eq!((s.current, s.more), (2, true));
        assert_eq!((s.part_of(24), s.part_of(25), s.part_of(52), s.part_of(0)), (0, 1, 2, 0));
        assert_eq!((s.holding(25), s.holding(60)), (Some(1), Some(2)));
        assert_eq!(s.parts[1].own_number(25), 1);
        // Stardust Crusaders: two halves of 24.
        let g = chain(&[entry(1, "TV", Some(24), "FINISHED"), entry(2, "TV", Some(24), "FINISHED")]);
        let s = split(1, &g.media[&1], (1, 48), None, &g, |_| None).unwrap();
        assert_eq!((firsts(&s), s.current, s.more), (vec![(1, 1), (2, 25)], 1, false));
    }

    /// Steel Ball Run: a one-episode first part, then eleven; the twelve
    /// episodes start with the first part whichever of the two is linked,
    /// and never reach back into Stone Ocean.
    #[test]
    fn a_split_starts_before_the_linked_entry_when_it_must() {
        let g = chain(&[
            entry(146_722, "ONA", Some(26), "FINISHED"),
            entry(190_327, "ONA", Some(1), "FINISHED"),
            entry(210_482, "ONA", Some(11), "RELEASING"),
        ]);
        for linked in [190_327, 210_482] {
            let s = split(linked, &g.media[&linked], (1, 12), None, &g, |_| None).unwrap();
            assert_eq!(firsts(&s), [(190_327, 1), (210_482, 2)], "linked to {linked}");
        }
    }

    /// No split where the episodes fit, the run is unclear, or the next
    /// entry is another series'.
    #[test]
    fn episodes_that_fit_or_runs_that_stop_are_not_split() {
        let g = kusuriya();
        let s1 = &g.media[&161_645];
        assert_eq!(split(161_645, s1, (1, 24), None, &g, |_| None), None);
        // The second season is a series of its own: passed over, its
        // episodes counted; with nothing after it, nothing to split.
        let s = split(161_645, s1, (1, 52), None, &g, |id| (id == 176_301).then(|| &g.media[&176_301])).unwrap();
        assert_eq!((firsts(&s), s.current), (vec![(161_645, 1), (195_516, 49)], 1));
        // The second season's numbers are in neither part.
        assert_eq!((s.holding(24), s.holding(25), s.holding(48), s.holding(49)), (Some(0), None, None, Some(1)));
        assert_eq!(split(161_645, s1, (1, 24), None, &g, |id| (id == 176_301).then(|| &g.media[&176_301])), None);
        // Its third season held apart too: nothing more is announced for it.
        let s = split(161_645, s1, (1, 48), None, &g, |id| (id == 195_516).then(|| &g.media[&id])).unwrap();
        assert!(!s.more, "S3 is another series'");
        // Two sequels of the family: which one runs on is unclear.
        let mut two = g.clone();
        two.sequels.get_mut(&161_645).unwrap().0.push(999);
        two.media.insert(999, entry(999, "TV", Some(12), "FINISHED").1);
        assert_eq!(split(161_645, s1, (1, 52), None, &two, |_| None), None);
        // A film sequel doesn't count, so the run goes on as before.
        let mut film = g.clone();
        film.sequels.get_mut(&161_645).unwrap().0.push(998);
        film.media.insert(998, entry(998, "MOVIE", Some(1), "FINISHED").1);
        assert_eq!(split(161_645, s1, (1, 52), None, &film, |_| None).map(|s| s.parts.len()), Some(3));
    }

    /// Steel Ball Run with six episodes: they fit its 11-episode second part,
    /// but the one-episode first part aired that year leads in, so 01 is
    /// that. One from years before doesn't; `numbering` decides either way.
    #[test]
    fn a_one_episode_entry_just_before_leads_in() {
        let year = |(id, m): (u64, SeriesMeta), y| (id, SeriesMeta { year: Some(y), ..m });
        let sbr = |lead_year| {
            chain(&[
                year(entry(146_722, "ONA", Some(26), "FINISHED"), 2022),
                year(entry(190_327, "ONA", Some(1), "FINISHED"), lead_year),
                year(entry(210_482, "ONA", Some(11), "RELEASING"), 2026),
            ])
        };
        let g = sbr(2026);
        let linked = &g.media[&210_482];
        let s = split(210_482, linked, (1, 6), None, &g, |_| None).unwrap();
        assert_eq!(firsts(&s), [(190_327, 1), (210_482, 2)]);
        // Numbered by hand: no lead-in, or where you say.
        assert_eq!(split(210_482, linked, (1, 6), Some(1), &g, |_| None), None);
        let s = split(210_482, linked, (1, 6), Some(2), &g, |_| None).unwrap();
        assert_eq!(firsts(&s), [(190_327, 1), (210_482, 2)]);
        // Eleven while it airs: still the lead-in and ten.
        let s = split(210_482, linked, (1, 11), None, &g, |_| None).unwrap();
        assert_eq!(firsts(&s), [(190_327, 1), (210_482, 2)]);
        // A finished season full by itself: no episode to spare.
        let finished = SeriesMeta { status: Some("FINISHED".into()), ..linked.clone() };
        assert_eq!(split(210_482, &finished, (1, 11), None, &g, |_| None), None);
        // A pilot of another format is no lead-in.
        let mut pilot = g.clone();
        pilot.media.get_mut(&190_327).unwrap().format = Some("TV".into());
        assert_eq!(split(210_482, linked, (1, 6), None, &pilot, |_| None), None);
        // A series that starts later (02–06) has no episode 1 to lead in with.
        assert_eq!(split(210_482, linked, (2, 6), None, &g, |_| None), None);
        let old = sbr(2019);
        assert_eq!(split(210_482, &old.media[&210_482], (1, 6), None, &old, |_| None), None);
    }

    /// A season numbered on (13–24) linked to its own entry, with the next
    /// cached: its episodes are its own, not the next season's; with more
    /// (13–36), the next holds the rest.
    #[test]
    fn a_season_numbered_on_starts_after_its_prequel() {
        let g = chain(&[
            entry(1, "TV", Some(12), "FINISHED"),
            entry(2, "TV", Some(12), "FINISHED"),
            entry(3, "TV", Some(12), "FINISHED"),
        ]);
        assert_eq!(split(2, &g.media[&2], (13, 24), None, &g, |_| None), None);
        let s = split(2, &g.media[&2], (13, 36), None, &g, |_| None).unwrap();
        assert_eq!(firsts(&s), [(2, 13), (3, 25)]);
        // Linked to the first season, the files the second's (13–24): that one.
        let s = split(1, &g.media[&1], (13, 24), None, &g, |_| None).unwrap();
        assert_eq!(firsts(&s), [(2, 13)]);
    }

    /// An entry of unknown length (airing) holds everything after it.
    #[test]
    fn an_entry_of_unknown_length_ends_the_run() {
        let g = chain(&[
            entry(1, "TV", Some(12), "FINISHED"),
            entry(2, "TV", None, "RELEASING"),
            entry(3, "TV", Some(12), "NOT_YET_RELEASED"),
        ]);
        let s = split(1, &g.media[&1], (1, 40), None, &g, |_| None).unwrap();
        assert_eq!((firsts(&s), s.current), (vec![(1, 1), (2, 13)], 1));
        assert_eq!(s.part_of(40), 1);
    }
}
