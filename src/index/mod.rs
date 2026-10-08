//! The file index: scanning media roots into a SQLite cache.

pub mod classify;
pub mod db;
pub mod scan;

pub use classify::{Classified, classify};
pub use db::{Db, FileRow};
