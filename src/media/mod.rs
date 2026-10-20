//! Media identification and organization: release-name parsing, metadata
//! lookup, and moving completed downloads into the media library.
//!
//! The confirmation step uses the parser and lookup to suggest a type, title
//! and artwork for a link. After the transfer, [`Organizer`] uses the same
//! parser, the chosen type and the title found to place the file where
//! Jellyfin and Plex find it ([`library`]).

mod artwork;
mod fetch;
pub mod library;
mod organizer;
pub mod parse;
mod steam;
pub mod tmdb;

#[cfg(test)]
pub(crate) use artwork::tests as artwork_tests;
pub use artwork::{Artwork, ArtworkService, Lookup};
pub use organizer::Organizer;
pub use steam::Steam;
pub use tmdb::Tmdb;
