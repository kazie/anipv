//! The library as folders: every root's files on disk, folder by folder, with
//! what is watched below each folder (the Files view's tree mode).

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use crate::library::{IndexedFile, Library};
use crate::model::WatchState;

/// A folder: a path relative to a root. The top (an empty root name) holds
/// the roots.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Dir {
    /// Root name (empty for the top).
    pub root: String,
    /// Path relative to the root (empty for the root itself).
    pub rel: PathBuf,
}

impl Dir {
    /// The top: the list of roots.
    #[must_use]
    pub fn top() -> Self {
        Self::default()
    }

    /// The root `name` itself.
    #[must_use]
    pub fn root(name: &str) -> Self {
        Self { root: name.to_string(), rel: PathBuf::new() }
    }

    /// The folder the file `rel` of root `root` is in.
    #[must_use]
    pub fn of_file(root: &str, rel: &Path) -> Self {
        Self { root: root.to_string(), rel: rel.parent().map(Path::to_path_buf).unwrap_or_default() }
    }

    /// True for the top.
    pub fn is_top(&self) -> bool {
        self.root.is_empty()
    }

    /// The folder above (`None` at the top).
    pub fn parent(&self) -> Option<Self> {
        if self.is_top() {
            None
        } else if self.rel.as_os_str().is_empty() {
            Some(Self::top())
        } else {
            Some(Self { root: self.root.clone(), rel: self.rel.parent().map(Path::to_path_buf).unwrap_or_default() })
        }
    }

    /// The folder `name` in this one.
    #[must_use]
    pub fn child(&self, name: &str) -> Self {
        if self.is_top() { Self::root(name) } else { Self { root: self.root.clone(), rel: self.rel.join(name) } }
    }

    /// Its last name: the folder's, or the root's.
    pub fn name(&self) -> String {
        self.rel.file_name().map_or_else(|| self.root.clone(), |n| n.to_string_lossy().into_owned())
    }

    /// `Anime/Monogatari Series`: the root and the path in it.
    pub fn display(&self) -> String {
        if self.rel.as_os_str().is_empty() {
            self.root.clone()
        } else {
            format!("{root}/{rel}", root = self.root, rel = self.rel.to_string_lossy())
        }
    }

    /// Its path from `at` (a folder it is in): `02 - Nisemonogatari`, with
    /// the root when `at` is the top (`Anime/Monogatari Series`).
    pub fn relative(&self, at: &Self) -> String {
        path_from(at, &self.root, &self.rel)
    }

    /// True if `other` is this folder or inside it.
    pub fn contains(&self, other: &Self) -> bool {
        self.is_top() || (self.root == other.root && other.rel.starts_with(&self.rel))
    }
}

/// The path `rel` of root `root` from the folder `at` it is in.
pub fn path_from(at: &Dir, root: &str, rel: &Path) -> String {
    match rel.strip_prefix(&at.rel) {
        Ok(rest) if !at.is_top() && at.root == root => rest.to_string_lossy().into_owned(),
        _ => format!("{root}/{rel}", rel = rel.to_string_lossy()),
    }
}

/// What is below a folder (in it and its subfolders).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stats {
    /// Video files that give an item (episode, special, …).
    pub videos: usize,
    /// Files whose every item is watched.
    pub watched: usize,
    /// Files not watched but started (partly watched, or some of their items).
    pub started: usize,
    /// When the newest file arrived.
    pub added: i64,
    /// Keys of the series the files belong to.
    pub series: BTreeSet<String>,
}

/// One row of a folder's listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    /// A folder in it (see [`Folders::stats`] for what is below it).
    Folder(Dir),
    /// A file in it (index into [`Library::files`]).
    File(usize),
}

impl Entry {
    /// The folder, if it is one.
    pub fn dir(&self) -> Option<&Dir> {
        match self {
            Self::Folder(dir) => Some(dir),
            Self::File(_) => None,
        }
    }

    /// Its path from the folder `at` it is in (see [`path_from`]).
    pub fn path_from(&self, at: &Dir, lib: &Library) -> String {
        match self {
            Self::Folder(dir) => dir.relative(at),
            Self::File(i) => path_from(at, &lib.files[*i].file.root, &lib.files[*i].file.rel),
        }
    }
}

/// A search candidate (see [`Folders::search`]), made an [`Entry`] only when it matches.
enum Hit<'a> {
    Folder(&'a Dir),
    File(usize),
}

/// The folders of a library's files on disk.
#[derive(Debug, Clone, Default)]
pub struct Folders {
    stats: HashMap<Dir, Stats>,
    /// Folder → its subfolders' names, in listing order.
    subfolders: HashMap<Dir, Vec<String>>,
    /// Folder → the files directly in it, in listing order.
    files: HashMap<Dir, Vec<usize>>,
    /// Every folder but the top with its path (`Anime/Monogatari Series`), in
    /// path order: what [`Folders::search`] matches.
    paths: Vec<(Dir, String)>,
    /// Every file on disk (index into [`Library::files`]) with its path
    /// (`Anime/Monogatari Series/…/x.mkv`), in path order: what
    /// [`Folders::search`] matches besides folders.
    file_paths: Vec<(usize, String)>,
}

/// How far a file is watched: every item, some (or one started), none.
pub fn file_state(lib: &Library, f: &IndexedFile) -> (bool, bool) {
    let Some(s) = lib.get(&f.series) else { return (false, false) };
    state_of(&f.items, |k| s.item(k).map(|i| i.state))
}

/// [`file_state`] of a file giving `items`, whose states `state` looks up.
fn state_of(
    items: &[crate::model::ItemKey],
    state: impl Fn(&crate::model::ItemKey) -> Option<WatchState>,
) -> (bool, bool) {
    let (mut all, mut any) = (!items.is_empty(), false);
    for k in items {
        let state = state(k).unwrap_or_default();
        all &= state.is_watched();
        any |= state != WatchState::Unwatched;
    }
    (all, !all && any)
}

impl Folders {
    /// The folders of `lib`'s files on disk, with the roots in the order of
    /// `roots` (the configured ones; others follow by name).
    pub fn build(lib: &Library, roots: &[String]) -> Self {
        let mut out = Self::default();
        let mut subfolders: HashMap<Dir, BTreeSet<String>> = HashMap::new();
        // Each series' item states, looked up once (not per file).
        let mut states: HashMap<&str, HashMap<&crate::model::ItemKey, WatchState>> = HashMap::new();
        for (i, f) in lib.files.iter().enumerate().filter(|(_, f)| f.file.present) {
            let of_series = states.entry(f.series.as_str()).or_insert_with(|| {
                lib.get(&f.series).map(|s| s.items.iter().map(|it| (&it.key, it.state)).collect()).unwrap_or_default()
            });
            let (watched, started) = state_of(&f.items, |k| of_series.get(k).copied());
            let mut dir = Dir::root(&f.file.root);
            let mut chain = vec![Dir::top(), dir.clone()];
            subfolders.entry(Dir::top()).or_default().insert(f.file.root.clone());
            let parent = f.file.rel.parent().unwrap_or_else(|| Path::new(""));
            for c in parent {
                let name = c.to_string_lossy().into_owned();
                subfolders.entry(dir.clone()).or_default().insert(name.clone());
                dir = dir.child(&name);
                chain.push(dir.clone());
            }
            out.files.entry(dir).or_default().push(i);
            // A file that gives no item can't be watched: it doesn't count.
            let counts = !f.items.is_empty();
            for d in chain {
                let s = out.stats.entry(d).or_default();
                s.videos += usize::from(counts);
                s.watched += usize::from(watched);
                s.started += usize::from(started);
                s.added = s.added.max(f.file.added());
                s.series.insert(f.series.clone());
            }
        }
        for (dir, names) in subfolders {
            let mut names: Vec<String> = names.into_iter().collect();
            if dir.is_top() {
                let at = |n: &String| roots.iter().position(|r| r == n).unwrap_or(usize::MAX);
                names.sort_by_cached_key(|n| (at(n), n.to_lowercase()));
            } else {
                names.sort_by_cached_key(|n| (n.to_lowercase(), n.clone()));
            }
            out.subfolders.insert(dir, names);
        }
        for files in out.files.values_mut() {
            files.sort_by_cached_key(|&i| lib.files[i].file.name().to_lowercase());
        }
        out.paths = out.stats.keys().filter(|d| !d.is_top()).map(|d| (d.clone(), d.display())).collect();
        out.paths.sort();
        out.file_paths = lib
            .files
            .iter()
            .enumerate()
            .filter(|(_, f)| f.file.present)
            .map(|(i, f)| (i, format!("{root}/{rel}", root = f.file.root, rel = f.file.rel.to_string_lossy())))
            .collect();
        out
    }

    /// What is below `dir` (`None` for a folder without files on disk).
    pub fn stats(&self, dir: &Dir) -> Option<&Stats> {
        self.stats.get(dir)
    }

    /// The listing of `dir`: its folders, then its files, each by name.
    pub fn list(&self, dir: &Dir) -> Vec<Entry> {
        let folders = self.subfolders.get(dir).into_iter().flatten().map(|name| Entry::Folder(dir.child(name)));
        folders.chain(self.files.get(dir).into_iter().flatten().map(|&i| Entry::File(i))).collect()
    }

    /// The folders and files below `at` whose path from it
    /// (`02 - Nisemonogatari/[G] Nisemonogatari - 04.mkv`) matches `query`
    /// (fuzzy, each word on its own), best first; on a tie, folders first,
    /// then in path order.
    pub fn search(&self, at: &Dir, query: &str) -> Vec<Entry> {
        // The stored paths below `at`, without its own path in front: slices,
        // so a keystroke allocates nothing per file.
        let prefix = if at.is_top() { String::new() } else { format!("{path}/", path = at.display()) };
        let folders = self.paths.iter().filter_map(|(d, path)| Some((Hit::Folder(d), path.strip_prefix(&prefix)?)));
        let files = self.file_paths.iter().filter_map(|(i, path)| Some((Hit::File(*i), path.strip_prefix(&prefix)?)));
        let candidates: Vec<(Hit<'_>, &str)> = folders.chain(files).collect();
        crate::app::fuzzy_rank(candidates.iter(), query, |(_, path)| (*path).into())
            .into_iter()
            .map(|((hit, _), _)| match hit {
                Hit::Folder(d) => Entry::Folder((*d).clone()),
                Hit::File(i) => Entry::File(*i),
            })
            .collect()
    }

    /// The files below `dir` (in it and its subfolders), in path order
    /// ([`Library::files`] is in path order already).
    pub fn files_under(&self, dir: &Dir) -> Vec<usize> {
        let mut out: Vec<usize> =
            self.files.iter().filter(|(d, _)| dir.contains(d)).flat_map(|(_, files)| files.iter().copied()).collect();
        out.sort_unstable();
        out
    }

    /// [`Folders::files_under`] that give an item: the files a folder's
    /// watch state and queue are made of (a file nothing could be made of
    /// can't be watched).
    pub fn countable_under<'l>(&self, lib: &'l Library, dir: &Dir) -> Vec<&'l IndexedFile> {
        self.files_under(dir).into_iter().map(|i| &lib.files[i]).filter(|f| !f.items.is_empty()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{Index, MetaCache};
    use crate::config::{Config, Root, RootKind};
    use crate::events::{Event, EventBody};
    use crate::index::db::FileRow;
    use crate::model::{EpNo, ItemKey};

    fn cfg() -> Config {
        Config {
            roots: vec![Root::test("dl", "/dl", RootKind::Ongoing), Root::test("anime", "/anime", RootKind::Archive)],
            ..Config::default()
        }
    }

    fn lib(files: &[(&str, &str, bool)], events: &[Event]) -> Library {
        let rows: Vec<FileRow> = files
            .iter()
            .map(|(root, rel, present)| FileRow { present: *present, ..FileRow::test(root, rel) })
            .collect();
        Library::build(&cfg(), events, &Index::new(&cfg(), rows, MetaCache::default()))
    }

    fn watched(series: &str, n: u32) -> Event {
        Event::new(1, "t", EventBody::watched(series, ItemKey::episode(EpNo::new(n))))
    }

    fn names(lib: &Library, entries: &[Entry]) -> Vec<String> {
        entries
            .iter()
            .map(|e| match e {
                Entry::Folder(dir) => format!("{name}/", name = dir.name()),
                Entry::File(i) => lib.files[*i].file.name(),
            })
            .collect()
    }

    const MONO: &str = "Monogatari Series";

    fn sample() -> Library {
        let files = [
            ("anime", format!("{MONO}/02 - Nisemonogatari/[G] Nisemonogatari - 01.mkv"), true),
            ("anime", format!("{MONO}/02 - Nisemonogatari/[G] Nisemonogatari - 02.mkv"), true),
            ("anime", format!("{MONO}/01 - Bakemonogatari/[G] Bakemonogatari - 01.mkv"), true),
            ("anime", format!("{MONO}/01 - Bakemonogatari/[G] Bakemonogatari - 02.mkv"), true),
            ("anime", format!("{MONO}/01 - Bakemonogatari/[G] Bakemonogatari - 03.mkv"), false),
            ("anime", "Cowboy Bebop/[G] Cowboy Bebop - 01.mkv".to_string(), true),
            ("dl", "[G] Dungeon Meshi - 01.mkv".to_string(), true),
        ];
        let files: Vec<(&str, &str, bool)> = files.iter().map(|(r, p, on)| (*r, p.as_str(), *on)).collect();
        lib(&files, &[watched("bakemonogatari", 1), watched("bakemonogatari", 2), watched("nisemonogatari", 1)])
    }

    #[test]
    fn listings_put_folders_first_in_name_order() {
        let lib = sample();
        let f = Folders::build(&lib, &["dl".into(), "anime".into()]);
        assert_eq!(names(&lib, &f.list(&Dir::top())), ["dl/", "anime/"], "roots in config order");
        assert_eq!(names(&lib, &f.list(&Dir::root("anime"))), ["Cowboy Bebop/", "Monogatari Series/"]);
        let mono = Dir::root("anime").child(MONO);
        assert_eq!(names(&lib, &f.list(&mono)), ["01 - Bakemonogatari/", "02 - Nisemonogatari/"]);
        let bake = mono.child("01 - Bakemonogatari");
        assert_eq!(
            names(&lib, &f.list(&bake)),
            ["[G] Bakemonogatari - 01.mkv", "[G] Bakemonogatari - 02.mkv"],
            "files no longer on disk are left out"
        );
        assert_eq!(names(&lib, &f.list(&Dir::root("dl"))), ["[G] Dungeon Meshi - 01.mkv"]);
        assert_eq!(bake.parent(), Some(mono));
        assert_eq!(Dir::root("anime").parent(), Some(Dir::top()));
        assert_eq!(Dir::top().parent(), None);
    }

    #[test]
    fn stats_add_up_the_tree() {
        let lib = sample();
        let f = Folders::build(&lib, &[]);
        let mono = Dir::root("anime").child(MONO);
        let s = f.stats(&mono).unwrap();
        assert_eq!((s.videos, s.watched, s.series.len()), (4, 3, 2));
        let s = f.stats(&mono.child("02 - Nisemonogatari")).unwrap();
        assert_eq!((s.videos, s.watched), (2, 1));
        assert_eq!(s.series.iter().collect::<Vec<_>>(), ["nisemonogatari"]);
        let s = f.stats(&Dir::top()).unwrap();
        assert_eq!((s.videos, s.watched), (6, 3));
    }

    #[test]
    fn search_matches_paths_word_by_word() {
        let lib = sample();
        let f = Folders::build(&lib, &[]);
        let names = |at: &Dir, query: &str| -> Vec<String> {
            f.search(at, query).iter().map(|e| e.path_from(at, &lib)).collect()
        };
        let hits = names(&Dir::top(), "mono nise");
        assert_eq!(hits.first(), Some(&format!("anime/{MONO}/02 - Nisemonogatari")), "best first: {hits:?}");
        // Below a folder: its folders and files, by their path from it.
        let mono = Dir::root("anime").child(MONO);
        let hits = names(&mono, "nise");
        assert_eq!(
            hits,
            [
                "02 - Nisemonogatari",
                "02 - Nisemonogatari/[G] Nisemonogatari - 01.mkv",
                "02 - Nisemonogatari/[G] Nisemonogatari - 02.mkv"
            ]
        );
        assert!(names(&Dir::root("dl"), "nise").is_empty(), "nothing outside the folder");
        assert!(names(&Dir::top(), "zzzz").is_empty());
    }

    #[test]
    fn files_under_a_folder_go_in_path_order() {
        let lib = sample();
        let f = Folders::build(&lib, &[]);
        let under = f.files_under(&Dir::root("anime").child(MONO));
        assert_eq!(
            under.iter().map(|&i| lib.files[i].file.name()).collect::<Vec<_>>(),
            [
                "[G] Bakemonogatari - 01.mkv",
                "[G] Bakemonogatari - 02.mkv",
                "[G] Nisemonogatari - 01.mkv",
                "[G] Nisemonogatari - 02.mkv"
            ]
        );
        assert_eq!(f.files_under(&Dir::top()).len(), 6);
    }
}
