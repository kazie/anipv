//! Configuration (`~/.config/anipv/config.toml`) and well-known paths.
//!
//! Setting `ANIPV_HOME` relocates config, data and cache under a single
//! directory, which is used by tests and the README demo.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// How files below a root are grouped into series.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RootKind {
    /// Loose files (e.g. a download folder): series come from file names.
    Ongoing,
    /// One folder per series (e.g. `Anime/One Piece/…`).
    Archive,
}

/// A scanned media directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Root {
    /// Short name shown in the UI.
    pub name: String,
    /// Directory path; `~` is expanded.
    pub path: PathBuf,
    /// Grouping strategy.
    pub kind: RootKind,
}

impl Root {
    /// A root for tests.
    #[cfg(test)]
    pub fn test(name: &str, path: impl Into<PathBuf>, kind: RootKind) -> Self {
        Self { name: name.into(), path: path.into(), kind }
    }

    /// The path with `~` expanded.
    pub fn resolved(&self) -> PathBuf {
        expand_tilde(&self.path)
    }
}

/// User configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Name of this machine in the event log; defaults to the hostname.
    pub device: Option<String>,
    /// Where per-device event logs live. Point this at a synced folder.
    pub events_dir: Option<PathBuf>,
    /// mpv executable.
    pub mpv: String,
    /// Extra arguments passed to every mpv invocation.
    pub mpv_args: Vec<String>,
    /// Fraction of an episode that counts as watched.
    pub watched_threshold: f64,
    /// Media roots, scanned in order.
    pub roots: Vec<Root>,
    /// Directory names that are never scanned.
    pub ignore_dirs: Vec<String>,
    /// Directory names whose contents are extras (NCOP, menus, …).
    pub extras_dirs: Vec<String>,
    /// Directory names whose contents are specials (OVA, SP, …).
    pub specials_dirs: Vec<String>,
    /// Fetch next-airing info from `AniList` for followed series.
    pub anilist: bool,
    /// Also look series the offline database can't match up on `AniList` by
    /// title (sends those titles to `AniList`). Needs `anilist`.
    pub anilist_search: bool,
    /// Where to download the offline anime database from (an `http(s)` URL
    /// of `anime-offline-database-minified.json`); the built-in sources when unset.
    pub offline_db_url: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            device: None,
            events_dir: None,
            mpv: "mpv".into(),
            mpv_args: Vec::new(),
            watched_threshold: 0.85,
            roots: Vec::new(),
            ignore_dirs: ["Screens", "Screenshots", "Thumbs", "@eaDir", "lost+found"].map(String::from).to_vec(),
            extras_dirs: [
                "Extras",
                "Extra",
                "Bonus",
                "Menus",
                "Menu",
                "NC",
                "NCOP",
                "NCED",
                "Trailers",
                "Trailer",
                "Scans",
                "DVD Extras",
                "Bluray Extras",
                "BD Extras",
                "Bonus Features",
            ]
            .map(String::from)
            .to_vec(),
            specials_dirs: ["Specials", "Special", "SP", "SPs", "OVA", "OVAs", "Omake"].map(String::from).to_vec(),
            anilist: true,
            anilist_search: false,
            offline_db_url: None,
        }
    }
}

/// Resolved filesystem locations.
#[derive(Debug, Clone)]
pub struct Paths {
    /// `config.toml`.
    pub config_file: PathBuf,
    /// Per-device event logs.
    pub events_dir: PathBuf,
    /// Rebuildable SQLite cache.
    pub db_file: PathBuf,
    /// Cache dir (offline metadata DB, …).
    pub cache_dir: PathBuf,
    /// mpv IPC sockets.
    pub runtime_dir: PathBuf,
}

impl Paths {
    /// Create the directories anipv writes to (events, index, cache, sockets).
    pub fn ensure(&self) -> Result<()> {
        let dirs =
            [Some(self.events_dir.as_path()), self.db_file.parent(), Some(&self.cache_dir), Some(&self.runtime_dir)];
        for d in dirs.into_iter().flatten() {
            std::fs::create_dir_all(d).with_context(|| format!("creating {}", d.display()))?;
        }
        Ok(())
    }

    /// Paths with everything under `home` (`config/`, `data/`, `cache/`), as
    /// `ANIPV_HOME` lays them out; what `anipv demo` writes.
    pub fn under(home: &Path) -> Self {
        Self::layout(&home.join("config"), &home.join("data"), home.join("cache"))
    }

    fn layout(config_dir: &Path, data_dir: &Path, cache_dir: PathBuf) -> Self {
        Self {
            config_file: config_dir.join("config.toml"),
            events_dir: data_dir.join("events"),
            db_file: data_dir.join("index.db"),
            runtime_dir: cache_dir.clone(),
            cache_dir,
        }
    }

    /// Compute paths from the environment and (optional) config.
    pub fn resolve(cfg: Option<&Config>) -> Result<Self> {
        let mut paths = if let Some(home) = std::env::var_os("ANIPV_HOME") {
            Self::under(Path::new(&home))
        } else {
            let dirs = directories::ProjectDirs::from("", "", "anipv").context("no home directory")?;
            Self::layout(dirs.config_dir(), dirs.data_dir(), dirs.cache_dir().to_path_buf())
        };
        if let Some(file) = std::env::var_os("ANIPV_CONFIG") {
            paths.config_file = file.into();
        }
        if let Some(dir) = cfg.and_then(|c| c.events_dir.as_deref()) {
            paths.events_dir = expand_tilde(dir);
        }
        paths.runtime_dir = runtime_dir_in(std::env::var_os("XDG_RUNTIME_DIR"), &paths.cache_dir);
        Ok(paths)
    }
}

/// The runtime directory: `$XDG_RUNTIME_DIR` (`value`) when it is an absolute
/// path, like the XDG spec says, else the cache directory.
fn runtime_dir_in(value: Option<std::ffi::OsString>, cache_dir: &Path) -> PathBuf {
    value.map(PathBuf::from).filter(|p| p.is_absolute()).unwrap_or_else(|| cache_dir.to_path_buf())
}

impl Config {
    /// Names of the `ongoing` roots (download folders).
    pub fn ongoing_roots(&self) -> HashSet<&str> {
        self.roots.iter().filter(|r| r.kind == RootKind::Ongoing).map(|r| r.name.as_str()).collect()
    }

    /// Load the config file, falling back to defaults when it does not exist.
    pub fn load(path: &Path) -> Result<Self> {
        let cfg: Self = match std::fs::read_to_string(path) {
            Ok(s) => toml::from_str(&s).with_context(|| format!("parsing {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        cfg.validate().with_context(|| format!("in {}", path.display()))?;
        Ok(cfg)
    }

    /// Root names key the index, so they must be unique. Roots must not
    /// overlap either: a file under two roots would be indexed under both,
    /// with its root and grouping depending on which scan saw it last.
    pub fn validate(&self) -> Result<()> {
        if !(self.watched_threshold > 0.0 && self.watched_threshold <= 1.0) {
            anyhow::bail!("`watched_threshold` must be above 0 and at most 1 (got {})", self.watched_threshold);
        }
        if let Some(d) = &self.device
            && !is_valid_device(d)
        {
            anyhow::bail!(
                "`device` {d:?} must be non-empty and use only letters, digits, `-` and `_` \
                 (it names this device's event log file)"
            );
        }
        if let Some(u) = &self.offline_db_url
            && !(u.starts_with("https://") || u.starts_with("http://"))
        {
            anyhow::bail!("`offline_db_url` {u:?} must be an http:// or https:// URL");
        }
        if let Some(d) = &self.events_dir
            && !is_absolute_or_tilde(d)
        {
            anyhow::bail!(
                "`events_dir` \"{}\" must be absolute or start with `~` (a relative path would depend on the \
                 directory anipv is started from)",
                d.display()
            );
        }
        let mut seen = HashSet::new();
        for r in &self.roots {
            if r.name.trim().is_empty() {
                anyhow::bail!("a root has an empty `name`; give each `[[roots]]` a short name");
            }
            if r.path.as_os_str().is_empty() {
                anyhow::bail!("root {:?} has an empty `path`", r.name);
            }
            if !is_absolute_or_tilde(&r.path) {
                anyhow::bail!(
                    "root {:?} has a relative `path` \"{}\"; use an absolute path or one starting with `~`",
                    r.name,
                    r.path.display()
                );
            }
            if !seen.insert(r.name.as_str()) {
                anyhow::bail!("two roots are named {:?}; give each `[[roots]]` a unique `name`", r.name);
            }
        }
        let dirs: Vec<PathBuf> = self.roots.iter().map(|r| normalized(&r.resolved())).collect();
        for (i, (outer, outer_dir)) in self.roots.iter().zip(&dirs).enumerate() {
            for (inner, inner_dir) in self.roots.iter().zip(&dirs).skip(i + 1) {
                let (outer, inner) = if inner_dir.starts_with(outer_dir) {
                    (outer, inner)
                } else if outer_dir.starts_with(inner_dir) {
                    (inner, outer)
                } else {
                    continue;
                };
                anyhow::bail!(
                    "root {inner:?} ({inner_path}) is inside root {outer:?} ({outer_path}); roots must not overlap \
                     (use `ignore_dirs` to leave a subfolder out)",
                    inner = inner.name,
                    inner_path = inner.path.display(),
                    outer = outer.name,
                    outer_path = outer.path.display(),
                );
            }
        }
        Ok(())
    }

    /// Add a root named after its folder (made unique), storing an absolute path.
    pub fn add_root(&mut self, path: &Path, kind: RootKind) -> Result<()> {
        let path = absolute_keep_tilde(path)?;
        let base = path.file_name().unwrap_or(path.as_os_str()).to_string_lossy().into_owned();
        let mut name = base.clone();
        let mut n = 2;
        // Unique in any case: `scan --root` picks a root by name in any case.
        while self.roots.iter().any(|r| r.name.eq_ignore_ascii_case(&name)) {
            name = format!("{base}-{n}");
            n += 1;
        }
        self.roots.push(Root { name, path, kind });
        Ok(())
    }

    /// Write the config file, creating parent directories. It is written
    /// next to it and renamed into place, so a crash never leaves half a file.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        replace_file(path, toml::to_string_pretty(self)?.as_bytes())?;
        Ok(())
    }

    /// Device name for the event log.
    pub fn device_name(&self) -> String {
        self.device.clone().unwrap_or_else(hostname)
    }

    /// True if `name` is an ignored directory.
    pub fn is_ignored_dir(&self, name: &str) -> bool {
        name.starts_with('.') || listed(&self.ignore_dirs, name)
    }

    /// True if `name` is a directory of extras (see `extras_dirs`).
    pub fn is_extras_dir(&self, name: &str) -> bool {
        listed(&self.extras_dirs, name)
    }

    /// True if `name` is a directory of specials (see `specials_dirs`).
    pub fn is_specials_dir(&self, name: &str) -> bool {
        listed(&self.specials_dirs, name)
    }

    /// The kind of each configured root, by name.
    pub fn root_kinds(&self) -> HashMap<&str, RootKind> {
        self.roots.iter().map(|r| (r.name.as_str(), r.kind)).collect()
    }
}

/// Write `bytes` to `path` through a temporary file next to it (unique per
/// process and call) that is renamed into place, so nothing ever reads half a
/// file. The temporary file never stays behind.
pub fn replace_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".part.{}.{nanos}.{seq}", std::process::id()));
    let res = std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, path));
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

/// True if `name` is in `list` (directory names, any case).
fn listed(list: &[String], name: &str) -> bool {
    list.iter().any(|d| d.eq_ignore_ascii_case(name))
}

/// True if `name` can be used as-is for an event log file name, i.e. the
/// event log would not need to sanitize it.
fn is_valid_device(name: &str) -> bool {
    !name.is_empty() && name.chars().all(crate::events::is_device_char)
}

/// Best-effort hostname without extra dependencies.
fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .or_else(|_| std::fs::read_to_string("/etc/hostname"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "device".into())
}

/// The real path when it exists (symlinks resolved), else the path with
/// `.` and `..` removed lexically.
fn normalized(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| {
        let mut out = PathBuf::new();
        for c in path.components() {
            match c {
                std::path::Component::CurDir => {}
                std::path::Component::ParentDir => {
                    out.pop();
                }
                c => out.push(c),
            }
        }
        out
    })
}

/// Make `p` absolute against the current directory, keeping a leading `~`
/// as-is (expanded only when used, so the config stays portable).
///
/// The one rule for paths stored in the config.
pub fn absolute_keep_tilde(p: &Path) -> std::io::Result<PathBuf> {
    if p.starts_with("~") { Ok(p.to_path_buf()) } else { std::path::absolute(p) }
}

/// True for the paths [`absolute_keep_tilde`] stores.
fn is_absolute_or_tilde(p: &Path) -> bool {
    p.is_absolute() || p.starts_with("~")
}

/// Expand a leading `~` to `$HOME`.
pub fn expand_tilde(p: &Path) -> PathBuf {
    match p.strip_prefix("~") {
        Ok(rest) => std::env::var_os("HOME").map_or_else(|| p.to_path_buf(), |h| PathBuf::from(h).join(rest)),
        Err(_) => p.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_absolute_runtime_dir_counts() {
        let cache = Path::new("/c");
        assert_eq!(runtime_dir_in(Some("/run/user/1000".into()), cache), Path::new("/run/user/1000"));
        for v in ["", "relative"] {
            assert_eq!(runtime_dir_in(Some(v.into()), cache), cache, "{v:?}");
        }
        assert_eq!(runtime_dir_in(None, cache), cache);
    }

    /// Root names differ in more than case: `scan --root` ignores case.
    #[test]
    fn added_roots_differ_in_more_than_case() {
        let mut cfg = Config::default();
        cfg.add_root(Path::new("/a/Anime"), RootKind::Archive).unwrap();
        cfg.add_root(Path::new("/b/anime"), RootKind::Archive).unwrap();
        let names: Vec<&str> = cfg.roots.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["Anime", "anime-2"]);
    }

    /// Saving replaces the file whole, leaving nothing else behind.
    #[test]
    fn saving_replaces_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "garbage").unwrap();
        let cfg = Config { device: Some("desk".into()), ..Config::default() };
        cfg.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap().device.as_deref(), Some("desk"));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn roundtrip_and_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        assert_eq!(Config::load(&path).unwrap(), Config::default());
        let mut cfg = Config::default();
        cfg.roots.push(Root::test("Downloads", "~/Videos/Downloads", RootKind::Ongoing));
        cfg.device = Some("desk".into());
        cfg.save(&path).unwrap();
        let back = Config::load(&path).unwrap();
        assert_eq!(back, cfg);
        assert_eq!(back.device_name(), "desk");
    }

    #[test]
    fn partial_file_uses_defaults() {
        let cfg: Config = toml::from_str("mpv_args = [\"--fs\"]\n").unwrap();
        assert_eq!(cfg.mpv, "mpv");
        assert_eq!(cfg.mpv_args, vec!["--fs"]);
        assert!(cfg.is_ignored_dir("screens"));
        assert!(cfg.is_ignored_dir(".hidden"));
        assert!(!cfg.is_ignored_dir("One Piece"));
    }

    #[test]
    fn roots_get_unique_names_and_absolute_paths() {
        let mut cfg = Config::default();
        cfg.add_root(Path::new("/a/anime"), RootKind::Ongoing).unwrap();
        cfg.add_root(Path::new("/b/anime"), RootKind::Archive).unwrap();
        cfg.add_root(Path::new("rel/dl"), RootKind::Ongoing).unwrap();
        cfg.add_root(Path::new("~/x"), RootKind::Ongoing).unwrap();
        let names: Vec<&str> = cfg.roots.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["anime", "anime-2", "dl", "x"]);
        assert!(cfg.roots[2].path.is_absolute());
        assert_eq!(cfg.roots[3].path, Path::new("~/x"), "~ is kept for portability");
        cfg.validate().unwrap();
        cfg.roots[1].name = "anime".into();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn nested_roots_rejected() {
        let mut cfg = Config::default();
        cfg.add_root(Path::new("/media/Anime"), RootKind::Archive).unwrap();
        cfg.add_root(Path::new("/media/Downloads"), RootKind::Ongoing).unwrap();
        cfg.add_root(Path::new("/media/Anime-old"), RootKind::Archive).unwrap();
        cfg.validate().unwrap();
        cfg.add_root(Path::new("/media/./Anime/../Anime/Done"), RootKind::Archive).unwrap();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains(r#"root "Done""#) && err.contains(r#"inside root "Anime""#), "{err}");
        cfg.roots.pop();
        cfg.add_root(Path::new("/media"), RootKind::Archive).unwrap();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains(r#"root "Anime""#) && err.contains(r#"inside root "media""#), "{err}");
    }

    #[test]
    fn threshold_must_be_a_fraction() {
        for bad in [0.0, -0.5, 1.5, f64::NAN] {
            let cfg = Config { watched_threshold: bad, ..Config::default() };
            assert!(cfg.validate().is_err(), "{bad}");
        }
        Config { watched_threshold: 1.0, ..Config::default() }.validate().unwrap();
    }

    #[test]
    fn offline_db_url_must_be_http() {
        for bad in ["", "ftp://x/db.json", "file:///etc/passwd", "/tmp/db.json", "cedya77/anime-offline-database"] {
            let cfg = Config { offline_db_url: Some(bad.into()), ..Config::default() };
            assert!(cfg.validate().is_err(), "{bad:?}");
        }
        for good in ["https://example.org/db.json", "http://localhost:8000/db.json"] {
            Config { offline_db_url: Some(good.into()), ..Config::default() }.validate().unwrap();
        }
        let cfg: Config = toml::from_str("offline_db_url = \"https://example.org/db.json\"\n").unwrap();
        assert_eq!(cfg.offline_db_url.as_deref(), Some("https://example.org/db.json"));
    }

    #[test]
    fn unknown_keys_rejected() {
        assert!(toml::from_str::<Config>("mvp = \"x\"\n").is_err());
        let typo = "[[roots]]\nname = \"a\"\npath = \"/a\"\nkind = \"archive\"\nignore = true\n";
        assert!(toml::from_str::<Config>(typo).is_err(), "unknown key in a root");
    }

    #[test]
    fn empty_root_name_or_path_rejected() {
        let root = |name: &str, path: &str| Root::test(name, path, RootKind::Archive);
        for bad in [root("", "/a"), root("  ", "/a"), root("a", "")] {
            let cfg = Config { roots: vec![bad.clone()], ..Config::default() };
            assert!(cfg.validate().is_err(), "{bad:?}");
        }
        Config { roots: vec![root("a", "/a")], ..Config::default() }.validate().unwrap();
    }

    #[test]
    fn relative_paths_rejected() {
        let root = |path: &str| Root::test("a", path, RootKind::Archive);
        let err = Config { roots: vec![root("anime")], ..Config::default() }.validate().unwrap_err().to_string();
        assert!(err.contains("relative"), "{err}");
        for good in ["/a", "~/a", "~"] {
            Config { roots: vec![root(good)], ..Config::default() }.validate().unwrap();
        }
        let ev = |d: &str| Config { events_dir: Some(d.into()), ..Config::default() };
        let err = ev("events").validate().unwrap_err().to_string();
        assert!(err.contains("events_dir"), "{err}");
        ev("~/sync/anipv").validate().unwrap();
        ev("/sync/anipv").validate().unwrap();
    }

    #[test]
    fn absolute_keeps_tilde() {
        assert_eq!(absolute_keep_tilde(Path::new("~/x")).unwrap(), Path::new("~/x"));
        assert_eq!(absolute_keep_tilde(Path::new("/x")).unwrap(), Path::new("/x"));
        let rel = absolute_keep_tilde(Path::new("x")).unwrap();
        assert_eq!(rel, std::env::current_dir().unwrap().join("x"));
    }

    #[test]
    fn device_name_must_not_need_sanitizing() {
        for bad in ["", "my laptop", "desk/top", "a.b"] {
            let cfg = Config { device: Some(bad.into()), ..Config::default() };
            assert!(cfg.validate().is_err(), "{bad:?}");
        }
        for good in ["desk", "my-laptop_2", "ノート"] {
            Config { device: Some(good.into()), ..Config::default() }.validate().unwrap();
        }
        // The hostname fallback is sanitized by the event log, never rejected.
        Config::default().validate().unwrap();
    }

    #[test]
    fn tilde() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(expand_tilde(Path::new("~/x")), PathBuf::from(home).join("x"));
        assert_eq!(expand_tilde(Path::new("/abs")), PathBuf::from("/abs"));
    }
}
