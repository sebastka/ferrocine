//! Ferrocine library: everything but the command line, so that integration
//! tests can drive the components.

/// The release, a date such as `2026-10-23` set by the image build
/// (`FERROCINE_VERSION`); the crate version otherwise.
pub const VERSION: &str = match option_env!("FERROCINE_VERSION") {
    Some(v) if !v.is_empty() => v,
    _ => env!("CARGO_PKG_VERSION"),
};

pub mod api;
pub mod app;
pub mod assets;
pub mod auth;
pub mod config;
pub mod lang;
pub mod media;
pub mod metadata;
pub mod metrics;
pub mod reload;
pub mod search;
pub mod store;
pub mod subtitles;
pub mod transcode;
pub mod trickplay;
