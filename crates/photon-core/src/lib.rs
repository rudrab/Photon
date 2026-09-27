//! photon-core: Domain logic, database layer, and models for Photon.
//!
//! This crate is library-grade — usable by CLI tools, scripts, and GUIs.
//! It owns the database schema, all query logic, and core domain types.

pub mod db;
pub mod error;
pub mod models;

pub use db::Database;
pub use error::PhotonError;
pub use models::{DesktopApp, EditRecord, Image, ImageFormat, LibraryQuery, Preferences, Tag};
