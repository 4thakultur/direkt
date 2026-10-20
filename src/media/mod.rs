//! Media identification: release-name parsing and metadata lookup.
//!
//! The confirmation step uses it to suggest a type, title and artwork for a
//! link. The media organization pipeline will build on the same parser and
//! metadata sources.

mod artwork;
mod fetch;
pub mod parse;
mod steam;
pub mod tmdb;

#[cfg(test)]
pub(crate) use artwork::tests as artwork_tests;
pub use artwork::{Artwork, ArtworkService, Lookup};
pub use steam::Steam;
pub use tmdb::Tmdb;
