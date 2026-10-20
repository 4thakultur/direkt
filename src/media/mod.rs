//! Media identification: release-name parsing and metadata lookup.
//!
//! For now this provides cover artwork for the web UI. The media organization
//! pipeline will build on the same parser and metadata source.

mod artwork;
pub mod parse;
pub mod tmdb;

pub use artwork::ArtworkService;
#[cfg(test)]
pub(crate) use artwork::id_for_name as artwork_id_for_name;
pub use tmdb::Tmdb;
