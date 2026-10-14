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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    /// The entry's number (`3`).
    pub no: u32,
    /// The folder's name without its number (`Nisemonogatari`).
    pub name: String,
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
}

/// Series that belong together, in watch order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Franchise {
    /// What to call it: its folder's name, else the first series' title.
    pub name: String,
    /// The watch order.
    pub stops: Vec<Stop>,
}

impl Franchise {
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

    let by_anilist: HashMap<u64, usize> =
        nodes.iter().enumerate().filter_map(|(i, node)| Some((node.anilist?, i))).collect();
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
    let mut out: Vec<Franchise> = members
        .into_values()
        .filter(|m| m.len() > 1)
        .map(|m| {
            let order = watch_order(nodes, &m, inside.get(&groups.find(m[0])).map_or(&[][..], Vec::as_slice));
            let folder = order.iter().find_map(|&i| nodes[i].folder.map(|f| f.name.as_str()));
            let name = folder.unwrap_or(nodes[order[0]].title).to_string();
            Franchise { name, stops: stops(nodes, &order) }
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
            out.push(Stop { series: i, folder: None, items: Vec::new(), watched: false });
        }
        let mut parts: Vec<&(Part, Vec<ItemKey>)> = nodes[i].parts.iter().collect();
        parts.sort_by_key(|(part, _)| part.no);
        out.extend(parts.into_iter().map(|(part, items)| Stop {
            series: i,
            folder: Some(part.clone()),
            items: items.clone(),
            watched: false,
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
        Part { no, name: format!("{no:02}") }
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
