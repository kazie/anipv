//! `anipv meta …`: metadata from the offline anime database and `AniList`.

use std::io::Write;

use anyhow::{Result, bail};

use super::report_auto_complete;
use crate::app::{Ctx, find_series};
use crate::events::now;
use crate::fmt::{bold, cyan, dim, green, yellow};
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

/// The downloaded offline database, or an error saying how to get it.
fn downloaded(ctx: &Ctx) -> Result<OfflineDb> {
    match OfflineDb::load(&ctx.paths.cache_dir)? {
        Some(db) => Ok(db),
        None => bail!("offline database not downloaded; run `anipv meta update`"),
    }
}

/// Run a `meta` subcommand.
pub(super) fn run(ctx: &Ctx, a: MetaAction, out: &mut impl Write) -> Result<()> {
    match a {
        MetaAction::Update => {
            writeln!(out, "downloading {} …", dim(offline::URL))?;
            let db = OfflineDb::download(&ctx.paths.cache_dir)?;
            ctx.store_sync(&SyncResult { db_version: db.version.clone(), ..SyncResult::default() })?;
            writeln!(out, "{ok} {n} anime with AniList ids", ok = green("✓"), n = db.entries.len())?;
            writeln!(out, "  next: {}", cyan("anipv meta match"))?;
        }
        MetaAction::Match { dry_run } => {
            let db = downloaded(ctx)?;
            let lib = ctx.library()?;
            let rep = crate::meta::match_library(&lib, &db, now(), None);
            let matched = rep.rows.len();
            if !dry_run {
                let (rows, attempts, db_version) = (rep.rows, rep.attempts, db.version);
                ctx.store_sync(&SyncResult { rows, attempts, db_version, ..SyncResult::default() })?;
            }
            writeln!(out, "{ok} matched {matched} series", ok = green("✓"))?;
            if !dry_run {
                report_auto_complete(ctx, out)?;
            }
            if !rep.ambiguous.is_empty() {
                writeln!(
                    out,
                    "\n{} (link with {}):",
                    bold("ambiguous"),
                    cyan("anipv meta link <series> <anilist-id>")
                )?;
                for (k, c) in rep.ambiguous.iter().take(40) {
                    writeln!(out, "  {}", yellow(k))?;
                    for x in c.iter().take(4) {
                        writeln!(out, "    {}", dim(x))?;
                    }
                }
            }
            writeln!(out, "{} series without a match", rep.unmatched.len())?;
        }
        MetaAction::Link { series, anilist } => {
            let lib = ctx.library()?;
            let s = find_series(&lib, &series)?;
            let id = (anilist != 0).then_some(anilist);
            ctx.link(&lib, &s.key, id)?;
            // Same path as the TUI: the link is now in the log, so syncing applies it.
            let opts = SyncOptions::requested(Some(s.key.clone()));
            let res = crate::meta::sync(&ctx.library()?, &ctx.paths.cache_dir, None, &opts, now());
            ctx.store_sync(&res)?;
            let linked = res.rows.iter().find(|r| r.series == s.key).and_then(|r| r.title.clone());
            let target = match (id, linked) {
                (None, _) => "unlinked".to_string(),
                (Some(_), Some(t)) => format!("{t} (anilist:{anilist})"),
                (Some(_), None) => format!("anilist:{anilist}"),
            };
            writeln!(out, "{ok} {title} → {target}", ok = green("✓"), title = bold(&s.title))?;
        }
        MetaAction::Refresh { series, all } => {
            if !ctx.cfg.anilist {
                bail!("network metadata is turned off (`anilist = false` in the config)");
            }
            let checks = ctx.db.anilist_checks()?;
            let lib = ctx.library()?;
            let request = match (series, all) {
                (Some(q), _) => Request::Series(find_series(&lib, &q)?.key.clone()),
                (None, true) => Request::All,
                (None, false) => Request::Auto { max_age: 0 },
            };
            let planned = crate::meta::plan(&lib, &ctx.cfg, &checks, &request, now());
            let Some(opts) = planned.filter(SyncOptions::asks_anilist) else {
                writeln!(out, "nothing to refresh (follow series, or use {})", cyan("--all"))?;
                return Ok(());
            };
            // Only `--all` updates the offline database.
            let opts = SyncOptions { update_offline: all, ..opts };
            let mut shown = Ok(());
            let http = Some(&anilist::Ureq as &dyn anilist::Http);
            let res = crate::meta::sync_with_progress(&lib, &ctx.paths.cache_dir, http, &opts, now(), &mut |step| {
                if step == SyncStep::Downloading {
                    shown = writeln!(out, "downloading {} …", dim(offline::URL));
                }
            });
            shown?;
            ctx.store_sync(&res)?;
            // A failed download (with `--all`) or AniList error fails the command.
            if let Some(e) = res.errors.first() {
                bail!("{e}");
            }
            writeln!(out, "{ok} refreshed {n} series from AniList", ok = green("✓"), n = res.rows.len())?;
            report_auto_complete(ctx, out)?;
        }
        MetaAction::Search { query } => {
            for e in downloaded(ctx)?.search(&query, 15) {
                writeln!(out, "  {}", e.describe())?;
            }
        }
    }
    Ok(())
}
