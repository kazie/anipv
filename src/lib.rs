//! anipv — an anime-aware mpv wrapper that tracks what you have watched.
//!
//! The crate is split so that all logic lives in the library and `main.rs`
//! is a thin CLI shell. See `docs/architecture.md` for an overview.

#![warn(missing_docs)]

pub mod app;
pub mod cli;
pub mod config;
#[cfg(any(test, feature = "demo"))]
pub mod demo;
pub mod events;
#[cfg(any(test, feature = "demo"))]
#[doc(hidden)]
pub mod fake_mpv;
pub mod fmt;
pub mod folders;
pub mod franchise;
pub mod identity;
pub mod import_fish;
pub mod index;
pub mod library;
pub mod meta;
pub mod model;
pub mod mpv;
pub mod parse;
pub mod split;
pub mod tui;
