//! The on-disk library of Flatline.
//!
//! # Layer 1 of the isolation ladder
//!
//! This crate owns one capability: *the models Flatline keeps between runs*.
//! It knows the shape of the active queue and the thumbnail cache, and it
//! queries the [`database`] crate for persistence. It knows nothing about
//! resolving URLs, playback or the user interface.

pub mod cache;
pub mod store;

pub use cache::ThumbnailCache;
pub use store::{Queue, QueueEntry};

/// Everything that can go wrong while using the library.
#[derive(Debug)]
pub enum Error {
    /// The database refused an operation.
    Database(database::Error),
    /// A file could not be read or written.
    Io(std::io::Error),
    /// A thumbnail could not be downloaded.
    Http(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Database(error) => write!(f, "the database failed: {error}"),
            Error::Io(error) => write!(f, "a file could not be handled: {error}"),
            Error::Http(message) => write!(f, "a thumbnail could not be downloaded: {message}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<database::Error> for Error {
    fn from(value: database::Error) -> Self {
        Error::Database(value)
    }
}

impl From<std::io::Error> for Error {
    fn from(value: std::io::Error) -> Self {
        Error::Io(value)
    }
}
