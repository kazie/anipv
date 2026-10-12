//! `anipv meta …`: metadata from the offline anime database and `AniList`.

use std::io::Write;

use anyhow::{Context, Result, bail};

use super::{writing, writing_unless};
use crate::app::{Ctx, Session, find_series};
use crate::events::now;
use crate::fmt::{bold, cyan, dim, green, yellow};
use crate::library::Library;
use crate::meta::offline::{self, OfflineDb};
use crate::meta::{Request, SyncOptions, SyncResult, SyncStep, anilist};

/// `anipv meta …` subcommands.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum MetaAction {
    /// Download the offline anime database (~ weekly updates).
    Update,
    /// Match series to the offline database automatically.
    Match {
        /// Only report, don't link.
        #[arg(long)]
        dry_run: bool,
        /// Also look series with no match up on AniList by title (sends those titles to AniList),
        /// including those searched before. With `anilist_search = true` in the config this
        /// happens without the flag (but not with `--dry-run`), for series not searched lately.
        #[arg(long)]
        search: bool,
    },
    /// Link a series to an AniList id manually (0 to unlink).
    Link {
        /// Series title, key or fuzzy query.
        series: String,
        /// AniList id or page URL.
        #[arg(value_parser = anilist_arg)]
        anilist: u64,
    },
    /// Refresh from AniList: stale airing info for followed series by default,
    /// one series when named, or everything with --all.
    Refresh {
        /// Refresh just this series (title, key or fuzzy query), whatever its status.
        series: Option<String>,
        /// Refresh every tracked series now, and update the offline database if
        /// it is missing or older than a week.
        #[arg(long, conflicts_with = "series")]
        all: bool,
        /// With --all, also every linked series that isn't tracked (untracked,
        /// dropped, skipped), for their score and popularity. Sends AniList the
        /// id of every anime in your library.
        #[arg(long, requires = "all")]
        untracked: bool,
    },
    /// Search the offline database.
    Search {
        /// Title to search for.
        query: String,
    },
}

fn anilist_arg(s: &str) -> Result<u64, String> {
    anilist::parse_ref(s).ok_or_else(|| "expected an AniList id or https://anilist.co/anime/… URL".into())
}

/// Why a command that needs the network doesn't run.
const NETWORK_OFF: &str = "network metadata is turned off (`anilist = false` in the config)";

/// Why a command that needs the offline database doesn't run.
const NO_DATABASE: &str = "offline database not downloaded; run `anipv meta update`";

/// Print warnings: things that didn't stop the command (see
/// [`SyncResult::notes`]), or failures it carries on after.
fn print_warnings(out: &mut impl Write, warnings: impl IntoIterator<Item = impl std::fmt::Display>) -> Result<()> {
    for w in warnings {
        writeln!(out, "{warn} {w}", warn = yellow("!"))?;
    }
    Ok(())
}

/// Run a `meta` subcommand.
pub(super) fn run(ctx: &Ctx, a: MetaAction, out: &mut impl Write) -> Result<()> {
    match a {
        MetaAction::Update => {
            let urls = offline::source_urls(ctx.cfg.offline_db_url.as_deref());
            writeln!(out, "downloading {} …", dim(&urls[0]))?;
            let (db, notes) = OfflineDb::download(&ctx.paths.cache_dir, &urls)?;
            ctx.store_sync(&SyncResult { db_version: Some(db.version_key().to_string()), ..SyncResult::default() })?;
            writeln!(out, "{ok} {n} anime with AniList ids", ok = green("✓"), n = db.entries.len())?;
            print_warnings(out, &notes)?;
            writeln!(out, "  next: {}", cyan("anipv meta match"))?;
            Ok(())
        }
        MetaAction::Match { dry_run, search } => {
            writing_unless(dry_run, ctx, out, |s, out| match_series(ctx, &s.lib, dry_run, search, out))
        }
        MetaAction::Link { series, anilist } => writing(ctx, out, |s, out| link(ctx, s, &series, anilist, out)),
        MetaAction::Refresh { series, all, untracked } => {
            // Said here: with the network off, the plan below would just be empty.
            if !ctx.cfg.anilist {
                bail!(NETWORK_OFF);
            }
            writing(ctx, out, |s, out| refresh(ctx, s, series.as_deref(), all, untracked, out))
        }
        MetaAction::Search { query } => {
            for e in OfflineDb::load(&ctx.paths.cache_dir)?.context(NO_DATABASE)?.search(&query, 15) {
                writeln!(out, "  {}", e.describe())?;
            }
            Ok(())
        }
    }
}

/// `meta match`: match against the offline database, storing the links
/// unless `dry_run`. With `search`, or `anilist_search` in the config (but not
/// in a dry run, since the titles do go to `AniList`), series it has no match
/// for are also looked up by title (see [`crate::meta::add_title_search`]),
/// which needs no database.
fn match_series(ctx: &Ctx, lib: &Library, dry_run: bool, search: bool, out: &mut impl Write) -> Result<()> {
    if search && !ctx.cfg.anilist {
        bail!(NETWORK_OFF);
    }
    let ts = now();
    let searching = search || (ctx.cfg.anilist && ctx.cfg.anilist_search && !dry_run);
    // Without a downloaded database a title search still can: every series
    // without a link is unmatched then.
    let mut rep = match OfflineDb::load(&ctx.paths.cache_dir)? {
        Some(db) => crate::meta::match_library(lib, &db, ts, None),
        None if searching => crate::meta::unmatched_without_database(lib, None),
        None => bail!(NO_DATABASE),
    };
    let before = rep.rows.len() - rep.scored;
    // `--search` asks again about series searched before.
    let found = searching.then(|| crate::meta::add_title_search(&mut rep, lib, &anilist::Ureq, ts, search));
    // Rows that only add a score to a link already made aren't matches.
    let matched = rep.rows.len() - rep.scored;
    if !dry_run {
        let (rows, attempts) = (std::mem::take(&mut rep.rows), std::mem::take(&mut rep.attempts));
        let db_version = Some(rep.db_version.clone());
        ctx.store_sync(&SyncResult { rows, attempts, db_version, ..SyncResult::default() })?;
    }
    writeln!(out, "{ok} matched {matched} series", ok = green("✓"))?;
    if rep.scored > 0 {
        writeln!(out, "{ok} scored {n} series already matched", ok = green("✓"), n = rep.scored)?;
    }
    if let Some(found) = found {
        writeln!(out, "  {n} of them by title on AniList", n = matched - before)?;
        print_warnings(out, &found.stopped)?;
        if found.skipped > 0 {
            writeln!(out, "  {n} more not searched this time; run again", n = found.skipped)?;
        }
    }
    if rep.ambiguous().next().is_some() {
        writeln!(out, "\n{} (link with {}):", bold("ambiguous"), cyan("anipv meta link <series> <anilist-id>"))?;
        for u in rep.ambiguous().take(40) {
            writeln!(out, "  {}", yellow(&u.series))?;
            for x in u.candidates.iter().take(4) {
                writeln!(out, "    {}", dim(x))?;
            }
        }
    }
    writeln!(out, "{} series without a match", rep.unmatched().count())?;
    Ok(())
}

/// `meta link`: record the link (`0` unlinks), then update the series'
/// metadata the way the TUI does after a link (from `AniList` too, when
/// network metadata is on), without updating the offline database.
fn link(ctx: &Ctx, s: &mut Session, query: &str, anilist: u64, out: &mut impl Write) -> Result<()> {
    let series = find_series(&s.lib, query)?;
    let (key, title) = (series.key.clone(), series.title.clone());
    let id = (anilist != 0).then_some(anilist);
    let recorded = ctx.link(&s.lib, &key, id)?;
    s.add(ctx, recorded);
    // The link is in the log and its cached rows are gone: sync on that. (A
    // rebuild is cheap, and `writing` rebuilds again for what the sync stores.)
    s.rebuild(ctx)?;
    let request = Request::Series(key.clone());
    let planned = crate::meta::plan(&s.lib, &ctx.cfg, &s.index.meta.anilist_checks, &request, now());
    let opts = SyncOptions { update_offline: false, ..planned.unwrap_or_default() };
    let http = Some(&anilist::Ureq as &dyn anilist::Http);
    let res = crate::meta::sync(&s.lib, &ctx.paths.cache_dir, http, &opts, now());
    ctx.store_sync(&res)?;
    let linked = res.rows.iter().find(|r| r.series == key).and_then(|r| r.title.clone());
    let target = match (id, linked) {
        (None, _) => "unlinked".to_string(),
        (Some(_), Some(t)) => format!("{t} (anilist:{anilist})"),
        (Some(_), None) => format!("anilist:{anilist}"),
    };
    writeln!(out, "{ok} {title} → {target}", ok = green("✓"), title = bold(&title))?;
    // The link is recorded either way: a failed lookup is retried by the next update.
    print_warnings(out, &res.errors)?;
    Ok(())
}

/// `meta refresh`: refresh from `AniList` (see [`MetaAction::Refresh`]).
fn refresh(
    ctx: &Ctx,
    s: &Session,
    series: Option<&str>,
    all: bool,
    untracked: bool,
    out: &mut impl Write,
) -> Result<()> {
    let lib = &s.lib;
    let request = match (series, all) {
        (Some(q), _) => Request::Series(find_series(lib, q)?.key.clone()),
        (None, true) => Request::All,
        (None, false) => Request::Auto { max_age: 0 },
    };
    let planned = crate::meta::plan(lib, &ctx.cfg, &s.index.meta.anilist_checks, &request, now());
    let Some(opts) = planned.filter(SyncOptions::asks_anilist) else {
        writeln!(out, "nothing to refresh (follow series, or use {})", cyan("--all"))?;
        return Ok(());
    };
    // Only `--all` updates the offline database.
    let opts = SyncOptions { update_offline: all, untracked, ..opts };
    let mut shown = Ok(());
    let first_url = opts.offline_urls.first().map_or("", String::as_str);
    let http = Some(&anilist::Ureq as &dyn anilist::Http);
    let res = crate::meta::sync_with_progress(lib, &ctx.paths.cache_dir, http, &opts, now(), &mut |step| {
        if step == SyncStep::Downloading {
            shown = writeln!(out, "downloading {} …", dim(first_url));
        }
    });
    shown?;
    ctx.store_sync(&res)?;
    print_warnings(out, &res.notes)?;
    // A failed download (with `--all`) or AniList error fails the command.
    if let Some(e) = res.errors.first() {
        bail!("{e}");
    }
    writeln!(out, "{ok} refreshed {n} series from AniList", ok = green("✓"), n = res.rows.len())?;
    Ok(())
}
