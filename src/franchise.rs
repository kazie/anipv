//! Franchises: series that belong together (seasons, sequels, the numbered
//! entries of a franchise folder), in watch order.
//!
//! Three kinds of link join two series:
//! - the same franchise folder (`Monogatari Series/03 - Nisemonogatari`, see
//!   [`crate::index::classify::entry_dir`]), ordered by the folders' numbers;
//! - the same title but for a season marker (`yuru camp`, `yuru camp s2`),
//!   ordered by season;
//! - an `AniList` prequel or sequel link.
//!
//! Every series linked to another, directly or not, is in one franchise. Its
//! series are ordered so that every link points forward where the links
//! allow it (a prequel before its sequel, folder 03 before 04), ties going to
//! the earlier year, then the key.
//!
//! The watch order is a list of [`Stop`]s: a whole series, or, in a franchise
//! folder, the part of a series in one entry folder. One anime can span
//! several folders with another in between (`05`, `06` and `08` hold
//! *Second Season*, `07` *Hanamonogatari*); the stops follow the folders, as
//! you would watching them one by one.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::model::ItemKey;

/// What [`group`] needs to know about one series.
#[derive(Debug, Clone, Default)]
pub struct Node<'a> {
    /// Series key.
    pub key: &'a str,
    /// Display title.
    pub title: &'a str,
    /// Linked `AniList` id.
    pub anilist: Option<u64>,
    /// The other `AniList` entries it covers (its episodes run through them,
    /// see [`crate::split`]): links to them are links to it.
    pub also: &'a [u64],
    /// `AniList` ids of its prequels.
    pub prequels: &'a [u64],
    /// `AniList` ids of its sequels.
    pub sequels: &'a [u64],
    /// The franchise folder its files are in.
    pub folder: Option<&'a Folder>,
    /// Its parts by entry folder (in a franchise folder), with the items in each.
    pub parts: Vec<(Part, Vec<ItemKey>)>,
    /// Year it started airing.
    pub year: Option<i32>,
    /// Only on `AniList` (see [`crate::library::Library::remote`]): joins a
    /// franchise by its `AniList` links alone, not by title or folder.
    pub remote: bool,
    /// `AniList` has it as an entry related to another (a film, a special),
    /// not as a season: it doesn't join by title's season marker either.
    pub branch: bool,
}

/// A franchise folder: an archive series folder of numbered entry folders
/// (see [`crate::index::classify::entry_dir`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Folder {
    /// The root it is in.
    pub root: String,
    /// Its cleaned name (`Monogatari Series`).
    pub name: String,
    /// The lowest entry number among a series' files in it.
    pub first: u32,
}

/// An entry folder of a franchise folder: its number and name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Part {
    /// The entry's number (`3`).
    pub no: u32,
    /// The folder's name without its number (`Nisemonogatari`).
    pub name: String,
    /// The `AniList` entry it is, for a part of a split series (see
    /// [`crate::split`]); `None` for an entry folder.
    pub anilist: Option<u64>,
    /// That entry's prequels: the stops it comes after.
    pub after: Vec<u64>,
}

/// One step of a franchise's watch order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stop {
    /// Index of the series (into the nodes [`group`] was given).
    pub series: usize,
    /// The entry folder this part of the series is in (`None`: the whole series).
    pub folder: Option<Part>,
    /// The series' items in that folder (empty for the whole series).
    pub items: Vec<ItemKey>,
    /// Every item of it is watched (a whole series: completed, or finished
    /// and fully watched). [`group`] leaves it false; the library sets it.
    pub watched: bool,
    /// What to call it next to the others (see [`short_labels`]). [`group`]
    /// leaves it empty; the library sets it.
    pub short: String,
}

/// Series that belong together, in watch order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Franchise {
    /// What to call it: its folder's name, else the first series' title.
    pub name: String,
    /// The watch order.
    pub stops: Vec<Stop>,
}

/// The longest start of `label`, ending between words, whose series key is
/// `key` (`Ranma 1-2 (2024)` of `Ranma 1-2 (2024) S2` for `ranma 1 2 2024`).
fn key_prefix<'a>(label: &'a str, key: &str) -> Option<&'a str> {
    let ends = label.char_indices().filter(|(_, c)| !c.is_alphanumeric()).map(|(i, _)| i).chain([label.len()]);
    ends.rev().map(|i| &label[..i]).find(|p| crate::identity::series_key(p) == key)
}

/// A franchise's name from its first entry: the title without a season
/// suffix (`Grand Blue` for `Grand Blue S2`).
fn base_title(node: &Node<'_>) -> String {
    // A title that doesn't spell its key (renamed) is kept as it is.
    let without = crate::identity::season_suffix(node.key).and_then(|(base, _)| key_prefix(node.title, base));
    let name =
        without.map_or(node.title, |name| name.trim_end_matches(|c: char| c.is_whitespace() || "-:~–—,".contains(c)));
    name.to_string()
}

/// What to call each of `labels` (a franchise's stops) next to the others:
/// without the franchise `name` they start with, and the separator after it
/// (`Pretty Gray` for `Umayuru - Pretty Gray`). What's left of a season is
/// its number (`S2`); the entry named like the franchise is `S1` among
/// seasons, else keeps its whole label, as do labels not starting with the
/// name (`Kabukimonogatari`). Two can't both be `S1`: series keys fold `S0`
/// and `S1` into the bare name, so `Foo S1` and `Foo` are one series.
pub fn short_labels<'a>(name: &str, labels: &[&'a str]) -> Vec<String> {
    /// What follows the franchise's name in a label.
    enum Rest<'a> {
        Nothing,
        Season(u32),
        Text(&'a str),
        Whole,
    }
    let name = crate::identity::series_key(name);
    let rest = |label: &&'a str| -> Rest<'a> {
        let Some(prefix) = key_prefix(label, &name) else { return Rest::Whole };
        let text = label[prefix.len()..].trim_start_matches(|c: char| !c.is_alphanumeric()).trim_end();
        // A closing bracket whose opening one was cut off goes too: `2024`,
        // not `2024)`, after `Ranma 1-2 (`.
        let unbalanced = |open, close| text.matches(close).count() > text.matches(open).count();
        let text = text.strip_suffix(')').filter(|_| unbalanced('(', ')')).unwrap_or(text);
        let text = text.strip_suffix(']').filter(|_| unbalanced('[', ']')).unwrap_or(text);
        let key = crate::identity::series_key(text);
        let season = key.strip_prefix('s').filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
        match season.and_then(|n| n.parse().ok()) {
            _ if key.is_empty() => Rest::Nothing,
            Some(n) => Rest::Season(n),
            None => Rest::Text(text),
        }
    };
    let rests: Vec<Rest<'a>> = labels.iter().map(rest).collect();
    let seasons = rests.iter().any(|r| matches!(r, Rest::Season(_)));
    labels
        .iter()
        .zip(rests)
        .map(|(label, rest)| match rest {
            Rest::Nothing if seasons => "S1".into(),
            Rest::Season(n) => format!("S{n}"),
            Rest::Text(text) => text.to_string(),
            Rest::Nothing | Rest::Whole => (*label).to_string(),
        })
        .collect()
}

impl Franchise {
    /// The series before and after series `i`: the stop before its first,
    /// the stop after its last. A series split around another (in a
    /// franchise folder) leads on to what follows its last part, and what
    /// follows a stop between its parts leads back to it.
    pub fn neighbours(&self, i: usize) -> (Option<usize>, Option<usize>) {
        let (Some(first), Some(last)) = (self.stops_of(i).next(), self.stops_of(i).last()) else { return (None, None) };
        let series = |n: Option<usize>| n.and_then(|n| self.stops.get(n)).map(|s| s.series);
        (series(first.checked_sub(1)), series(Some(last + 1)))
    }

    /// Whether every stop of series `i` is watched (each marked ✓).
    pub fn series_watched(&self, i: usize) -> bool {
        self.stops_of(i).all(|n| self.stops[n].watched)
    }

    /// Indices of the stops of series `i`, in order.
    pub fn stops_of(&self, i: usize) -> impl Iterator<Item = usize> + '_ {
        self.stops.iter().enumerate().filter(move |(_, s)| s.series == i).map(|(n, _)| n)
    }

    /// Each series once, in the order of its first stop.
    pub fn series(&self) -> Vec<usize> {
        let mut seen = Vec::new();
        for s in &self.stops {
            if !seen.contains(&s.series) {
                seen.push(s.series);
            }
        }
        seen
    }
}

/// The franchises among `nodes`: every group of two or more linked series,
/// ordered by their first entry's index.
pub fn group(nodes: &[Node<'_>]) -> Vec<Franchise> {
    let n = nodes.len();
    // Directed links, earlier → later.
    let mut edges: Vec<(usize, usize)> = Vec::new();

    let mut by_folder: BTreeMap<(&str, &str), Vec<usize>> = BTreeMap::new();
    for (i, node) in nodes.iter().enumerate() {
        if let Some(f) = node.folder {
            by_folder.entry((&f.root, &f.name)).or_default().push(i);
        }
    }
    for members in by_folder.values_mut() {
        members.sort_by_key(|&i| (nodes[i].folder.map(|f| f.first), nodes[i].key));
        edges.extend(members.windows(2).map(|w| (w[0], w[1])));
    }

    let mut by_base: BTreeMap<&str, Vec<(u32, usize)>> = BTreeMap::new();
    for (i, node) in nodes.iter().enumerate() {
        if node.remote || node.branch {
            continue;
        }
        let (base, season) = crate::identity::season_suffix(node.key).unwrap_or((node.key, 1));
        by_base.entry(base).or_default().push((season, i));
    }
    for members in by_base.values_mut().filter(|m| m.len() > 1) {
        members.sort_unstable();
        // Every series of one season links to every one of the next (two
        // keys can share a season: `show` and `show s1`).
        let seasons: Vec<&[(u32, usize)]> = members.chunk_by(|a, b| a.0 == b.0).collect();
        for pair in seasons.windows(2) {
            for &(_, a) in pair[0] {
                edges.extend(pair[1].iter().map(|&(_, b)| (a, b)));
            }
        }
    }

    let mut by_anilist: HashMap<u64, usize> = HashMap::new();
    for (i, node) in nodes.iter().enumerate() {
        for &id in node.anilist.iter().chain(node.also) {
            by_anilist.entry(id).or_insert(i);
        }
    }
    for (i, node) in nodes.iter().enumerate() {
        edges.extend(node.prequels.iter().filter_map(|id| by_anilist.get(id)).map(|&p| (p, i)));
        edges.extend(node.sequels.iter().filter_map(|id| by_anilist.get(id)).map(|&s| (i, s)));
    }
    edges.retain(|(a, b)| a != b);
    edges.sort_unstable();
    edges.dedup();

    let mut groups = UnionFind::new(n);
    for &(a, b) in &edges {
        groups.union(a, b);
    }
    // Each group's links (both ends are in one group: they joined it).
    let mut inside: HashMap<usize, Vec<(usize, usize)>> = HashMap::new();
    for &(a, b) in &edges {
        inside.entry(groups.find(a)).or_default().push((a, b));
    }
    let mut members: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for i in 0..n {
        members.entry(groups.find(i)).or_default().push(i);
    }
    // A series alone is a franchise when it runs through several AniList
    // entries (see `Node::also`) and has a part in more than one.
    let several = |m: &Vec<usize>| m.len() > 1 || (!nodes[m[0]].also.is_empty() && nodes[m[0]].parts.len() > 1);
    let mut out: Vec<Franchise> = members
        .into_values()
        .filter(several)
        .map(|m| {
            let order = watch_order(nodes, &m, inside.get(&groups.find(m[0])).map_or(&[][..], Vec::as_slice));
            let folder = order.iter().find_map(|&i| nodes[i].folder.map(|f| f.name.as_str()));
            // Named after a series you have, rather than what is only on AniList.
            let first = order.iter().copied().find(|&i| !nodes[i].remote).unwrap_or(order[0]);
            let name = folder.map_or_else(|| base_title(&nodes[first]), str::to_string);
            let mut stops = stops(nodes, &order);
            // A split series' parts with another series' season between them.
            if order.iter().any(|&i| !nodes[i].also.is_empty()) {
                stops = chain_order(nodes, stops);
            }
            Franchise { name, stops }
        })
        .collect();
    out.sort_by_key(|f| f.stops.iter().map(|s| s.series).min());
    out
}

/// The stops of series in `order`: one per entry folder of a series that
/// has parts, else the whole series. The stops of each franchise folder are
/// then put in folder order among the places they take, the others (and
/// another folder's) staying where they are.
fn stops(nodes: &[Node<'_>], order: &[usize]) -> Vec<Stop> {
    let mut out = Vec::new();
    for &i in order {
        if nodes[i].parts.is_empty() {
            out.push(Stop { series: i, folder: None, items: Vec::new(), watched: false, short: String::new() });
        }
        let mut parts: Vec<&(Part, Vec<ItemKey>)> = nodes[i].parts.iter().collect();
        parts.sort_by_key(|(part, _)| part.no);
        out.extend(parts.into_iter().map(|(part, items)| Stop {
            series: i,
            folder: Some(part.clone()),
            items: items.clone(),
            watched: false,
            short: String::new(),
        }));
    }
    // The franchise folder a folder stop is in.
    let folder_of = |stop: &Stop| stop.folder.as_ref().and_then(|_| nodes[stop.series].folder);
    let folders: BTreeSet<(&str, &str)> =
        out.iter().filter_map(folder_of).map(|f| (f.root.as_str(), f.name.as_str())).collect();
    for (root, name) in folders {
        let here = |stop: &Stop| folder_of(stop).is_some_and(|f| f.root == root && f.name == name);
        let places: Vec<usize> = (0..out.len()).filter(|&n| here(&out[n])).collect();
        let mut in_folder: Vec<Stop> = places.iter().map(|&n| out[n].clone()).collect();
        in_folder.sort_by_key(|s| s.folder.as_ref().map(|f| f.no));
        for (n, stop) in places.into_iter().zip(in_folder) {
            out[n] = stop;
        }
    }
    out
}

/// `stops` reordered so each comes after its `AniList` prequels: a part of a
/// split series after its entry's prequels ([`Part::after`]), a whole series
/// after its node's. Kahn's algorithm, the earliest stop first among those
/// free to go, so stops that `AniList` doesn't order keep their order; on a
/// cycle, the earliest left. Kusuriya's S1 and S3 (one series) around its S2
/// (another) read S1, S2, S3.
fn chain_order(nodes: &[Node<'_>], stops: Vec<Stop>) -> Vec<Stop> {
    let id = |s: &Stop| s.folder.as_ref().and_then(|p| p.anilist).or(nodes[s.series].anilist);
    // Whether stop `s` comes after `AniList` entry `x`.
    let after = |s: &Stop, x: u64| match s.folder.as_ref().filter(|p| p.anilist.is_some()) {
        Some(p) => p.after.contains(&x),
        None => nodes[s.series].prequels.contains(&x),
    };
    let n = stops.len();
    let mut incoming = vec![0usize; n];
    let mut next: Vec<Vec<usize>> = vec![Vec::new(); n];
    for b in 0..n {
        for a in (0..n).filter(|&a| a != b) {
            if id(&stops[a]).is_some_and(|x| after(&stops[b], x)) {
                next[a].push(b);
                incoming[b] += 1;
            }
        }
    }
    let mut left: Vec<usize> = (0..n).collect();
    let mut order = Vec::with_capacity(n);
    while !left.is_empty() {
        let free = left.iter().copied().find(|&i| incoming[i] == 0).unwrap_or(left[0]);
        left.retain(|&i| i != free);
        order.push(free);
        for &b in &next[free] {
            incoming[b] = incoming[b].saturating_sub(1);
        }
    }
    let mut stops: Vec<Option<Stop>> = stops.into_iter().map(Some).collect();
    order.into_iter().filter_map(|i| stops[i].take()).collect()
}

/// `members` ordered so every edge between them (`edges`, all inside the
/// group) points forward (Kahn's
/// algorithm), the earliest year (then key) first among those free to go;
/// on a cycle, the rest in that order.
fn watch_order(nodes: &[Node<'_>], members: &[usize], edges: &[(usize, usize)]) -> Vec<usize> {
    let rank =
        |i: usize| (nodes[i].year.unwrap_or(i32::MAX), nodes[i].folder.map_or(u32::MAX, |f| f.first), nodes[i].key);
    let mut incoming: HashMap<usize, usize> = members.iter().map(|&i| (i, 0)).collect();
    for (_, b) in edges {
        *incoming.entry(*b).or_default() += 1;
    }
    let mut order = Vec::with_capacity(members.len());
    let mut left: Vec<usize> = members.to_vec();
    while !left.is_empty() {
        let free = left.iter().copied().filter(|i| incoming[i] == 0).min_by_key(|&i| rank(i));
        // A cycle: take the earliest of what is left.
        let next = free.unwrap_or_else(|| left.iter().copied().min_by_key(|&i| rank(i)).unwrap_or(left[0]));
        left.retain(|&i| i != next);
        order.push(next);
        for (_, b) in edges.iter().filter(|(a, _)| *a == next) {
            if let Some(c) = incoming.get_mut(b) {
                *c = c.saturating_sub(1);
            }
        }
    }
    order
}

/// Disjoint sets over `0..n`.
struct UnionFind(Vec<usize>);

impl UnionFind {
    fn new(n: usize) -> Self {
        Self((0..n).collect())
    }

    fn find(&mut self, mut i: usize) -> usize {
        while self.0[i] != i {
            self.0[i] = self.0[self.0[i]];
            i = self.0[i];
        }
        i
    }

    fn union(&mut self, a: usize, b: usize) {
        let (a, b) = (self.find(a), self.find(b));
        self.0[a.max(b)] = a.min(b);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Short names: a season number, and only that.
    #[test]
    fn short_labels_drop_the_franchise_name() {
        let ranma = ["Ranma 1-2 (2024)", "Ranma 1-2 (2024) S2", "Ranma 1-2 2024 S3"];
        assert_eq!(short_labels("Ranma 1-2 (2024)", &ranma), ["S1", "S2", "S3"]);
        // What follows the name, past its separator: a spin-off's name.
        let sao = ["Sword Art Online", "Sword Art Online S2", "Sword Art Online Alternative Gun Gale Online S2"];
        assert_eq!(short_labels("Sword Art Online", &sao), ["S1", "S2", "Alternative Gun Gale Online S2"]);
        let uma = ["Umayuru", "Umayuru - Pretty Gray", "Umayuru - Full Gate!"];
        assert_eq!(short_labels("Umayuru", &uma), ["Umayuru", "Pretty Gray", "Full Gate!"]);
        let camp = ["Yuru Camp", "Yuru Camp Movie"];
        assert_eq!(short_labels("Yuru Camp", &camp), ["Yuru Camp", "Movie"]);
        // Named after an entry that is a season itself.
        assert_eq!(short_labels("Grand Blue", &["Grand Blue S2", "Grand Blue S3"]), ["S2", "S3"]);
        // Not starting with the franchise's name, or only by part of a word.
        assert_eq!(short_labels("Monogatari Series", &["Bakemonogatari"]), ["Bakemonogatari"]);
        assert_eq!(short_labels("Ranma", &["Ranma", "Ranmaru S2"]), ["Ranma", "Ranmaru S2"]);
        // Brackets stay balanced.
        assert_eq!(short_labels("Ranma 1-2", &["Ranma 1-2", "Ranma 1-2 (2024)"]), ["Ranma 1-2", "2024"]);
        assert_eq!(short_labels("Spy x Family", &["Spy x Family (Part 2)"]), ["Part 2"]);
    }

    /// The entries around a series split around another: Second Season (1)
    /// at stops 1, 2 and 4, Hanamonogatari (2) at 3, a movie (3) after.
    #[test]
    fn neighbours_follow_the_stops() {
        let stop = |series| Stop { series, folder: None, items: Vec::new(), watched: false, short: String::new() };
        let f = Franchise { name: "Monogatari Series".into(), stops: [0, 1, 1, 2, 1, 3].map(stop).to_vec() };
        assert_eq!(f.neighbours(0), (None, Some(1)));
        assert_eq!(f.neighbours(1), (Some(0), Some(3)), "on from its last part, to the movie");
        assert_eq!(f.neighbours(2), (Some(1), Some(1)), "back to the part it sits in");
        assert_eq!(f.neighbours(3), (Some(1), None), "the movie follows Second Season's last part");
        assert_eq!(f.neighbours(9), (None, None));
    }

    /// A franchise without a folder is named after its first entry, without
    /// the season: `Grand Blue`, not `Grand Blue S2`.
    #[test]
    fn franchise_names_drop_the_season() {
        let name = |key, title| base_title(&Node { key, title, ..Node::default() });
        assert_eq!(name("grand blue s2", "Grand Blue S2"), "Grand Blue");
        assert_eq!(name("ranma 1 2 2024", "Ranma 1-2 (2024)"), "Ranma 1-2 (2024)");
        assert_eq!(name("ranma 1 2 2024 s2", "Ranma 1-2 (2024) Season 2"), "Ranma 1-2 (2024)");
        assert_eq!(name("umayuru", "Umayuru"), "Umayuru");
        // Its symbols stay; a renamed title is kept as it is.
        assert_eq!(name("yuru camp", "Yuru Camp△"), "Yuru Camp△");
        assert_eq!(name("yuru camp s2", "Yuru Camp△ S2"), "Yuru Camp△");
        assert_eq!(name("shingeki no kyojin s2", "Attack on Titan Season 2"), "Attack on Titan Season 2");
    }

    fn node(key: &'static str) -> Node<'static> {
        Node { key, title: key, ..Node::default() }
    }

    fn keys(nodes: &[Node<'_>], f: &Franchise) -> Vec<String> {
        f.series().iter().map(|&i| nodes[i].key.to_string()).collect()
    }

    /// The stops as `key` or `key@folder`.
    fn stops(nodes: &[Node<'_>], f: &Franchise) -> Vec<String> {
        f.stops
            .iter()
            .map(|s| match &s.folder {
                Some(part) => format!("{key}@{no:02}", key = nodes[s.series].key, no = part.no),
                None => nodes[s.series].key.to_string(),
            })
            .collect()
    }

    fn ep(n: u32) -> ItemKey {
        ItemKey::episode(crate::model::EpNo::new(n))
    }

    /// Franchise folder `name` in root `anime`, with a series' first entry `first`.
    fn folder(name: &str, first: u32) -> &'static Folder {
        Box::leak(Box::new(Folder { root: "anime".into(), name: name.into(), first }))
    }

    fn part(no: u32) -> Part {
        Part { no, name: format!("{no:02}"), ..Part::default() }
    }

    #[test]
    fn folder_entries_go_in_folder_order() {
        let folder = |key, no| Node { folder: Some(folder("Monogatari Series", no)), ..node(key) };
        let nodes = [
            folder("nisemonogatari", 3),
            node("other"),
            folder("bakemonogatari", 1),
            folder("monogatari series second season", 5),
            folder("nekomonogatari", 4),
        ];
        let got = group(&nodes);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "Monogatari Series");
        assert_eq!(
            keys(&nodes, &got[0]),
            ["bakemonogatari", "nisemonogatari", "nekomonogatari", "monogatari series second season"]
        );
    }

    /// One anime spread over several entry folders with another in between:
    /// the stops follow the folders.
    #[test]
    fn stops_follow_the_folders() {
        let parts = |parts: &[(u32, &[u32])]| -> Vec<(Part, Vec<ItemKey>)> {
            parts.iter().map(|(no, eps)| (part(*no), eps.iter().map(|&e| ep(e)).collect())).collect()
        };
        let folder = |key, no| Node { folder: Some(folder("Monogatari Series", no)), ..node(key) };
        let nodes = [
            Node { parts: parts(&[(4, &[1, 2])]), ..folder("nekomonogatari", 4) },
            Node { parts: parts(&[(5, &[1]), (6, &[2]), (8, &[3])]), ..folder("second season", 5) },
            Node { anilist: Some(1), parts: parts(&[(7, &[1])]), ..folder("hanamonogatari", 7) },
            // Linked by AniList only, outside the folder: stays after its prequel.
            Node { anilist: Some(2), prequels: &[1], ..node("a movie") },
        ];
        let got = group(&nodes);
        assert_eq!(
            stops(&nodes, &got[0]),
            [
                "nekomonogatari@04",
                "second season@05",
                "second season@06",
                "hanamonogatari@07",
                "second season@08",
                "a movie"
            ]
        );
        assert_eq!(got[0].stops[4].items, [ep(3)]);
        assert_eq!(got[0].stops_of(1).collect::<Vec<_>>(), [1, 2, 4]);
        assert_eq!(keys(&nodes, &got[0]), ["nekomonogatari", "second season", "hanamonogatari", "a movie"]);
    }

    #[test]
    fn seasons_and_anilist_links_join_series() {
        let nodes = [
            node("yuru camp s2"),
            node("yuru camp"),
            node("yuru camp s3"),
            // Max Heart names Futari wa Precure as its prequel; Splash Star is
            // named Max Heart's sequel.
            Node { anilist: Some(2), prequels: &[1], sequels: &[3], ..node("futari wa precure max heart") },
            Node { anilist: Some(1), ..node("futari wa precure") },
            Node { anilist: Some(3), ..node("futari wa precure splash star") },
            Node { anilist: Some(9), sequels: &[404], ..node("unrelated") },
            node("pocket monsters 2023"),
            node("pocket monsters"),
        ];
        let got = group(&nodes);
        let all: Vec<Vec<String>> = got.iter().map(|f| keys(&nodes, f)).collect();
        assert_eq!(
            all,
            [
                vec!["yuru camp", "yuru camp s2", "yuru camp s3"],
                vec!["futari wa precure", "futari wa precure max heart", "futari wa precure splash star"],
            ],
            "a year alone doesn't join series"
        );
        assert_eq!(got[0].name, "yuru camp", "the first entry's title");

        // Two keys of one season both join (`show` and `show s1`).
        let nodes = [node("show"), node("show s1"), node("show s2")];
        assert_eq!(keys(&nodes, &group(&nodes)[0]), ["show", "show s1", "show s2"]);
    }

    /// Two franchise folders in one franchise (joined by AniList): each
    /// folder's entries stay together, in its own order.
    #[test]
    fn franchise_folders_keep_their_own_order() {
        let a = |key, no| Node { folder: Some(folder("A", no)), parts: vec![(part(no), vec![ep(1)])], ..node(key) };
        let b = |key, no| Node { folder: Some(folder("B", no)), parts: vec![(part(no), vec![ep(1)])], ..node(key) };
        let nodes = [
            Node { anilist: Some(1), ..a("a1", 1) },
            Node { anilist: Some(2), sequels: &[3], ..a("a2", 2) },
            Node { anilist: Some(3), ..b("b1", 1) },
            b("b2", 2),
        ];
        assert_eq!(stops(&nodes, &group(&nodes)[0]), ["a1@01", "a2@02", "b1@01", "b2@02"]);
    }

    /// Links that contradict each other still give every entry once.
    #[test]
    fn cycles_fall_back_to_year_order() {
        let nodes = [
            Node { anilist: Some(1), prequels: &[2], year: Some(2010), ..node("a") },
            Node { anilist: Some(2), prequels: &[1], year: Some(2005), ..node("b") },
        ];
        assert_eq!(keys(&nodes, &group(&nodes)[0]), ["b", "a"]);
    }
}
