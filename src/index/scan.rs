//! Walking media roots.
//!
//! Network mounts (rclone, SMB) are latency bound, so top-level directories
//! are listed by a small pool of threads.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};

use crate::config::{Config, Root};
use crate::parse::is_video;

/// A video file found on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScannedFile {
    /// Absolute path.
    pub path: PathBuf,
    /// Path relative to the root.
    pub rel: PathBuf,
    /// Size in bytes.
    pub size: u64,
    /// Modification time (unix seconds).
    pub mtime: i64,
}

/// Result of walking one root.
#[derive(Debug, Default)]
pub struct ScanResult {
    /// Video files found.
    pub files: Vec<ScannedFile>,
    /// Directories that could not be listed.
    pub errors: Vec<String>,
    /// The resolved root directory that was walked.
    pub base: PathBuf,
    /// Whether the root directory is a mount point (`None` if unknown).
    pub mounted: Option<bool>,
}

impl ScanResult {
    /// True if every directory was listed successfully.
    pub fn complete(&self) -> bool {
        self.errors.is_empty()
    }
}

const WORKERS: usize = 8;

/// Whether `dir` is a mount point: on unix, its device differs from its
/// parent's. `None` when that can't be told (stat fails, or not unix). Two
/// stats, so cheap even on a network mount.
pub fn is_mount_point(dir: &Path) -> Option<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let here = std::fs::metadata(dir).ok()?;
        let parent = std::fs::metadata(dir.join("..")).ok()?;
        Some(here.dev() != parent.dev())
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        None
    }
}

/// Walk `root`, calling `progress` with the running file count.
///
/// Fails only if the root itself is missing or unreadable (e.g. the network
/// drive is not mounted); errors below the root are collected instead.
///
/// Folder symlinks are walked in rounds by how many were followed to reach
/// them: first the real folders, then (in path order) the targets of the links
/// found there, and so on. A target already walked (on unix: the same device
/// and inode) is not walked again, so each linked folder is listed once, under
/// the fewest links and then the smallest path, whichever thread is faster.
///
/// # Panics
///
/// Only if a scanner thread panicked (a poisoned lock), which is a bug.
pub fn scan_root(cfg: &Config, root: &Root, progress: &(dyn Fn(usize) + Sync)) -> Result<ScanResult> {
    let base = root.resolved();
    let Some(base_meta) = std::fs::metadata(&base).ok().filter(std::fs::Metadata::is_dir) else {
        bail!("{} is not a directory (not mounted?)", base.display());
    };
    // Folders listed in earlier rounds.
    let mut done: HashSet<FileId> = file_id(&base_meta).into_iter().collect();
    let mut found = Partial::default();
    let mut linked = false;

    // Round 0 lists the root itself here, so its subfolders spread over the threads.
    let walker = Walker { cfg, base: &base, done: &done };
    let mut queue = Vec::new();
    walker.visit(&base, &Chain::default(), &mut found, &mut |d, chain| queue.push((d, chain)))?;
    progress(found.files.len());
    // The link targets walked this round: only done once the round is (a
    // target inside another one is still listed there, maybe under a smaller path).
    let mut chosen = HashSet::new();
    loop {
        let walker = Walker { cfg, base: &base, done: &done };
        let mut round = walker.walk_all(queue, found.files.len(), progress);
        found.absorb(&mut round);
        done.extend(chosen.drain());
        done.extend(found.walked.drain(..));
        let mut links = std::mem::take(&mut found.links);
        if links.is_empty() {
            break;
        }
        // In path order, so the smallest link to a folder is the one walked.
        links.sort_unstable_by(|a, b| a.path.cmp(&b.path));
        queue = links
            .into_iter()
            .filter(|l| l.id.is_none_or(|id| !done.contains(&id) && chosen.insert(id)))
            .map(|l| (l.path, l.chain))
            .collect();
        linked |= !queue.is_empty();
    }
    let Partial { files, errors, repeats, .. } = found;
    let mut files =
        if linked || repeats { dedupe(files) } else { files.into_iter().map(|f| f.file).collect::<Vec<_>>() };
    files.sort_unstable_by(|a, b| a.path.cmp(&b.path));
    Ok(ScanResult { files, errors, mounted: is_mount_point(&base), base })
}

/// A file reached several ways (links to the same folder, hard links) is kept
/// once, under the path with the fewest links followed, then the smallest:
/// the same path every scan.
fn dedupe(found: Vec<Found>) -> Vec<ScannedFile> {
    let mut at: HashMap<FileId, usize> = HashMap::new();
    let mut kept: Vec<Found> = Vec::with_capacity(found.len());
    for f in found {
        let Some(id) = f.id else {
            kept.push(f);
            continue;
        };
        match at.entry(id) {
            Entry::Vacant(e) => {
                e.insert(kept.len());
                kept.push(f);
            }
            Entry::Occupied(e) => {
                let k = &mut kept[*e.get()];
                if (f.links, &f.file.path) < (k.links, &k.file.path) {
                    *k = f;
                }
            }
        }
    }
    kept.into_iter().map(|f| f.file).collect()
}

/// A file's identity on disk (device, inode), whatever path led to it.
type FileId = (u64, u64);

#[cfg_attr(unix, expect(clippy::unnecessary_wraps, reason = "None where there are no inodes"))]
fn file_id(meta: &std::fs::Metadata) -> Option<FileId> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some((meta.dev(), meta.ino()))
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        None
    }
}

/// True if the file has other hard links, i.e. other paths may lead to it.
fn hard_linked(meta: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        meta.nlink() > 1
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        false
    }
}

/// The real folders in which links were followed on the way to a folder;
/// shared, since every folder below a link has the same one.
type Chain = Arc<[PathBuf]>;

/// A video file found by a walk, before duplicates are dropped.
struct Found {
    id: Option<FileId>,
    /// Folder symlinks followed to get here.
    links: usize,
    file: ScannedFile,
}

/// A folder symlink to walk in the next round (unless its target is walked already).
struct Link {
    path: PathBuf,
    /// The target folder's identity.
    id: Option<FileId>,
    /// The chain to walk it with (see [`follow`]).
    chain: Chain,
}

/// What one thread's walk found.
#[derive(Default)]
struct Partial {
    files: Vec<Found>,
    errors: Vec<String>,
    /// Folder symlinks found, for the next round.
    links: Vec<Link>,
    /// Real folders listed.
    walked: Vec<FileId>,
    /// Whether any file found has other hard links.
    repeats: bool,
}

impl Partial {
    /// Move everything `other` found into `self`.
    fn absorb(&mut self, other: &mut Self) {
        self.files.append(&mut other.files);
        self.errors.append(&mut other.errors);
        self.links.append(&mut other.links);
        self.walked.append(&mut other.walked);
        self.repeats |= other.repeats;
    }
}

/// What one round of a root's scan shares between its threads.
struct Walker<'a> {
    cfg: &'a Config,
    base: &'a Path,
    /// Folders listed in earlier rounds: not listed again.
    done: &'a HashSet<FileId>,
}

impl Walker<'_> {
    /// List one directory: video files go to `out`, real subdirectories to
    /// `subdir`, folder symlinks to `out.links`. Failures below `dir` are
    /// recorded in `out.errors` (so an incomplete scan never marks files as
    /// gone); failing to list `dir` itself is returned.
    ///
    /// Symlinks are followed (a library may link series folders from other
    /// drives); a link back to one of its ancestors is skipped to avoid loops
    /// (see [`follow`]), and a dangling link is ignored. `chain` is the real
    /// folders in which links were followed on the way to `dir`.
    fn visit(
        &self,
        dir: &Path,
        chain: &Chain,
        out: &mut Partial,
        subdir: &mut dyn FnMut(PathBuf, Chain),
    ) -> std::io::Result<()> {
        let (cfg, base) = (self.cfg, self.base);
        let failed = |p: &Path, e: &dyn std::fmt::Display| format!("{}: {e}", p.display());
        // `dir` with links resolved, once there is a folder link to check.
        let mut here: Option<PathBuf> = None;
        for entry in std::fs::read_dir(dir)? {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    out.errors.push(failed(dir, &e));
                    continue;
                }
            };
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(t) => t,
                Err(e) => {
                    out.errors.push(failed(&path, &e));
                    continue;
                }
            };
            // Only symlinks need a stat to learn what they point at.
            let target = if file_type.is_symlink() {
                match std::fs::metadata(&path) {
                    Ok(m) => Some(m),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(e) => {
                        out.errors.push(failed(&path, &e));
                        continue;
                    }
                }
            } else {
                None
            };
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let (is_dir, is_file) = match &target {
                Some(m) => (m.is_dir(), m.is_file()),
                None => (file_type.is_dir(), file_type.is_file()),
            };
            if is_dir {
                if cfg.is_ignored_dir(&name) {
                    continue;
                }
                if let Some(t) = &target {
                    let resolved = match &here {
                        Some(h) => Ok(h),
                        None => std::fs::canonicalize(dir).map(|h| &*here.insert(h)),
                    };
                    match resolved.and_then(|h| follow(h, &path, chain)) {
                        Ok(Some(chain)) => out.links.push(Link { path, id: file_id(t), chain }),
                        Ok(None) => {}
                        // Unknown: skip it, but record it so the scan counts as
                        // incomplete and nothing below is marked as gone.
                        Err(e) => out.errors.push(failed(&path, &e)),
                    }
                } else {
                    // Relative to the open directory: no second full-path lookup on network mounts.
                    let id = entry.metadata().ok().as_ref().and_then(file_id);
                    if let Some(id) = id {
                        if self.done.contains(&id) {
                            continue;
                        }
                        out.walked.push(id);
                    }
                    subdir(path, Arc::clone(chain));
                }
            } else if is_file && is_video(&name) && !is_resource_fork(&name) {
                match target.map_or_else(|| entry.metadata(), Ok) {
                    Ok(m) => match scanned(base, path, &m) {
                        Ok(file) => {
                            out.repeats |= hard_linked(&m);
                            out.files.push(Found { id: file_id(&m), links: chain.len(), file });
                        }
                        Err(e) => out.errors.push(e),
                    },
                    Err(e) => out.errors.push(failed(&path, &e)),
                }
            }
        }
        Ok(())
    }

    fn walk(&self, dir: &Path, chain: &Chain, out: &mut Partial) {
        let mut subdirs = Vec::new();
        if let Err(e) = self.visit(dir, chain, out, &mut |d, c| subdirs.push((d, c))) {
            out.errors.push(format!("{}: {e}", dir.display()));
        }
        for (d, c) in subdirs {
            self.walk(&d, &c, out);
        }
    }

    /// Walk the folders in `queue` on a few threads; `before` files were found
    /// already (for `progress`).
    fn walk_all(&self, queue: Vec<(PathBuf, Chain)>, before: usize, progress: &(dyn Fn(usize) + Sync)) -> Partial {
        let threads = WORKERS.min(queue.len());
        let queue = Mutex::new(queue);
        let shared = Mutex::new(Partial::default());
        std::thread::scope(|s| {
            for _ in 0..threads {
                s.spawn(|| {
                    loop {
                        let Some((dir, chain)) = queue.lock().expect("queue lock").pop() else { break };
                        let mut local = Partial::default();
                        self.walk(&dir, &chain, &mut local);
                        let mut all = shared.lock().expect("result lock");
                        all.absorb(&mut local);
                        progress(before + all.files.len());
                    }
                });
            }
        });
        shared.into_inner().expect("result lock")
    }
}

/// The `chain` to walk the directory symlink `link` with, or `None` if it
/// points at an ancestor of `here` (the folder it is in, links resolved) or
/// of a folder where a link was followed on the way here (`chain`): walking it
/// would loop, e.g. `A/toB` then `B/toA`. An error means it couldn't be
/// resolved (e.g. a network mount hiccup).
fn follow(here: &Path, link: &Path, chain: &[PathBuf]) -> std::io::Result<Option<Chain>> {
    let target = std::fs::canonicalize(link)?;
    if here.starts_with(&target) || chain.iter().any(|d| d.starts_with(&target)) {
        return Ok(None);
    }
    Ok(Some(chain.iter().cloned().chain([here.to_path_buf()]).collect()))
}

/// `._name.mkv`: macOS resource forks that SMB/NAS shares keep next to the real file.
fn is_resource_fork(name: &str) -> bool {
    name.starts_with("._")
}

fn scanned(base: &Path, path: PathBuf, meta: &std::fs::Metadata) -> Result<ScannedFile, String> {
    let mtime = meta.modified().map_or(0, crate::events::unix_secs);
    let rel = path.strip_prefix(base).map_err(|e| format!("{}: {e}", path.display()))?.to_path_buf();
    Ok(ScannedFile { rel, path, size: meta.len(), mtime })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RootKind;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn touch(p: &Path) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, b"x").unwrap();
    }

    #[test]
    fn walks_tree_and_filters() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        touch(&d.join("a - 01.mkv"));
        touch(&d.join("notes.txt"));
        touch(&d.join("Show/Show - 01.mkv"));
        touch(&d.join("Show/Extras/NCOP.mkv"));
        touch(&d.join("Screens/clip.webm"));
        touch(&d.join(".hidden/x.mkv"));
        touch(&d.join("Show/._Show - 01.mkv"));
        let root = Root::test("r", d.to_path_buf(), RootKind::Ongoing);
        let calls = AtomicUsize::new(0);
        let res = scan_root(&Config::default(), &root, &|_| {
            calls.fetch_add(1, Ordering::Relaxed);
        })
        .unwrap();
        assert_eq!(rels(&res), vec!["Show/Extras/NCOP.mkv", "Show/Show - 01.mkv", "a - 01.mkv"]);
        assert!(res.complete());
        assert!(calls.load(Ordering::Relaxed) >= 1);
        assert_eq!(res.files[0].size, 1);
    }

    #[test]
    fn unreadable_files_make_the_scan_incomplete() {
        let dir = tempfile::tempdir().unwrap();
        // A path outside the root can't be made relative: recorded, not skipped.
        let meta = std::fs::metadata(dir.path()).unwrap();
        assert!(scanned(dir.path(), PathBuf::from("/elsewhere/x.mkv"), &meta).is_err());
        // A directory we can't list is recorded, not silently skipped.
        let mut out = Partial::default();
        let walker = Walker { cfg: &Config::default(), base: dir.path(), done: &HashSet::new() };
        walker.walk(&dir.path().join("missing"), &Chain::default(), &mut out);
        assert!(!out.errors.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn follows_symlinks_without_looping() {
        use std::os::unix::fs::symlink;
        let lib = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        touch(&other.path().join("Linked Show/Linked Show - 01.mkv"));
        touch(&other.path().join("loose.mkv"));
        std::fs::create_dir_all(lib.path().join("Show")).unwrap();
        symlink(other.path().join("Linked Show"), lib.path().join("Linked Show")).unwrap();
        symlink(other.path().join("loose.mkv"), lib.path().join("Show/Show - 02.mkv")).unwrap();
        symlink(lib.path(), lib.path().join("Show/loop")).unwrap();
        symlink(lib.path().join("missing"), lib.path().join("dangling.mkv")).unwrap();
        let root = Root::test("r", lib.path().to_path_buf(), RootKind::Archive);
        let res = scan_root(&Config::default(), &root, &|_| {}).unwrap();
        assert_eq!(rels(&res), vec!["Linked Show/Linked Show - 01.mkv", "Show/Show - 02.mkv"]);
        assert!(res.complete(), "loops and dangling links are not errors: {:?}", res.errors);
    }

    fn rels(res: &ScanResult) -> Vec<String> {
        res.files.iter().map(|f| f.rel.to_string_lossy().into_owned()).collect()
    }

    /// Two folders linking to each other: the walk stops instead of going on
    /// until the path gets too long, and each file is listed once, at its real
    /// path.
    #[cfg(unix)]
    #[test]
    fn mutual_symlinks_do_not_loop() {
        use std::os::unix::fs::symlink;
        let lib = tempfile::tempdir().unwrap();
        touch(&lib.path().join("A/A - 01.mkv"));
        touch(&lib.path().join("B/B - 01.mkv"));
        symlink(lib.path().join("B"), lib.path().join("A/toB")).unwrap();
        symlink(lib.path().join("A"), lib.path().join("B/toA")).unwrap();
        let root = Root::test("r", lib.path().to_path_buf(), RootKind::Archive);
        let res = scan_root(&Config::default(), &root, &|_| {}).unwrap();
        assert!(res.complete(), "{:?}", res.errors);
        assert_eq!(rels(&res), vec!["A/A - 01.mkv", "B/B - 01.mkv"]);
    }

    /// Regression: with several links to one folder, the path a file is indexed
    /// under depended on which thread got there first, so files flipped between
    /// scans. Now it is always the smallest.
    #[cfg(unix)]
    #[test]
    fn links_to_the_same_folder_give_stable_paths() {
        use std::os::unix::fs::symlink;
        let lib = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        touch(&other.path().join("Show/Show - 01.mkv"));
        for name in ["d", "c", "b", "a"] {
            symlink(other.path().join("Show"), lib.path().join(name)).unwrap();
        }
        let root = Root::test("r", lib.path().to_path_buf(), RootKind::Archive);
        for _ in 0..20 {
            let res = scan_root(&Config::default(), &root, &|_| {}).unwrap();
            assert_eq!(rels(&res), vec!["a/Show - 01.mkv"]);
        }
    }

    /// Links to a folder and to its parent, and a deeper link back to a linked
    /// folder: each file once, under the fewest links, then the smallest path.
    #[cfg(unix)]
    #[test]
    fn nested_links_keep_the_shortest_way() {
        use std::os::unix::fs::symlink;
        let lib = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        touch(&other.path().join("Show/Show - 01.mkv"));
        touch(&other.path().join("Deep/Deep - 01.mkv"));
        symlink(other.path().join("Show"), lib.path().join("z")).unwrap();
        symlink(other.path(), lib.path().join("m")).unwrap();
        symlink(other.path().join("Show"), other.path().join("Deep/again")).unwrap();
        touch(&lib.path().join("Real/Real - 01.mkv"));
        symlink(lib.path().join("Real"), other.path().join("Deep/back")).unwrap();
        let root = Root::test("r", lib.path().to_path_buf(), RootKind::Archive);
        let res = scan_root(&Config::default(), &root, &|_| {}).unwrap();
        assert!(res.complete(), "{:?}", res.errors);
        assert_eq!(rels(&res), vec!["Real/Real - 01.mkv", "m/Deep/Deep - 01.mkv", "m/Show/Show - 01.mkv"]);
    }

    /// Hard links to one file are one file, under the smallest path.
    #[cfg(unix)]
    #[test]
    fn hard_links_are_kept_once() {
        let lib = tempfile::tempdir().unwrap();
        touch(&lib.path().join("b/Show - 01.mkv"));
        std::fs::create_dir(lib.path().join("a")).unwrap();
        std::fs::hard_link(lib.path().join("b/Show - 01.mkv"), lib.path().join("a/Show - 01.mkv")).unwrap();
        let root = Root::test("r", lib.path().to_path_buf(), RootKind::Archive);
        let res = scan_root(&Config::default(), &root, &|_| {}).unwrap();
        assert_eq!(rels(&res), vec!["a/Show - 01.mkv"]);
    }

    #[test]
    fn follow_skips_ancestors() {
        let lib = tempfile::tempdir().unwrap();
        let lib = std::fs::canonicalize(lib.path()).unwrap();
        let dir = lib.join("Show");
        std::fs::create_dir_all(&dir).unwrap();
        // canonicalize fails for a path through a non-directory.
        std::fs::write(lib.join("file.txt"), b"").unwrap();
        assert!(follow(&dir, &lib.join("file.txt/sub"), &[]).is_err());
        // An ancestor is a loop; a sibling folder is not, unless a link was
        // followed from inside it on the way here.
        assert_eq!(follow(&dir, &lib, &[]).unwrap(), None);
        let sibling = lib.join("Other");
        std::fs::create_dir_all(sibling.join("sub")).unwrap();
        assert_eq!(follow(&dir, &sibling, &[]).unwrap().as_deref(), Some(&[dir.clone()][..]));
        assert_eq!(follow(&dir, &sibling, &[sibling.join("sub")]).unwrap(), None);
    }

    #[cfg(unix)]
    #[test]
    fn plain_folders_are_not_mount_points() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        assert_eq!(is_mount_point(&dir.path().join("sub")), Some(false));
        let root = Root::test("r", dir.path().to_path_buf(), RootKind::Ongoing);
        let res = scan_root(&Config::default(), &root, &|_| {}).unwrap();
        assert_eq!((res.base.as_path(), res.mounted), (dir.path(), Some(false)));
        assert_eq!(is_mount_point(&dir.path().join("missing")), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn proc_is_a_mount_point() {
        assert_eq!(is_mount_point(Path::new("/proc")), Some(true));
    }

    #[test]
    fn missing_root_errors() {
        let root = Root::test("r", "/nonexistent/anipv", RootKind::Ongoing);
        assert!(scan_root(&Config::default(), &root, &|_| {}).is_err());
    }
}
