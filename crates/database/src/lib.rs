//! The SQLite database of Flatline.
//!
//! # Layer 1 of the isolation ladder
//!
//! This crate owns one capability: *opening and giving access to the
//! program's SQLite database file*. It knows nothing about what the tables
//! mean; other crates such as `library` build their models on top of it.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

pub use rusqlite;
use rusqlite::Connection;

/// Everything that can go wrong while opening the database.
#[derive(Debug)]
pub enum Error {
    /// SQLite refused an operation.
    Sqlite(rusqlite::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Sqlite(error) => write!(f, "the database failed: {error}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<rusqlite::Error> for Error {
    fn from(value: rusqlite::Error) -> Self {
        Error::Sqlite(value)
    }
}

/// A shared handle to the program database.
pub struct Database {
    connection: Mutex<Connection>,
}

impl Database {
    /// Opens the database at `path`, creating the file when needed.
    pub fn open(path: impl AsRef<Path>) -> Result<Database, Error> {
        let connection = Connection::open(path)?;
        configure(&connection)?;
        Ok(Database { connection: Mutex::new(connection) })
    }

    /// Opens a database that lives only in memory, which is what tests use.
    pub fn in_memory() -> Result<Database, Error> {
        let connection = Connection::open_in_memory()?;
        configure(&connection)?;
        Ok(Database { connection: Mutex::new(connection) })
    }

    /// Runs work against the underlying connection.
    pub fn with_connection<T>(
        &self,
        f: impl FnOnce(&Connection) -> Result<T, rusqlite::Error>,
    ) -> Result<T, Error> {
        let connection = self.connection();
        Ok(f(&connection)?)
    }

    /// Runs work against the underlying connection, mapping errors freely.
    pub fn try_with_connection<T, E>(
        &self,
        f: impl FnOnce(&Connection) -> Result<T, E>,
    ) -> Result<T, E> {
        let connection = self.connection();
        f(&connection)
    }

    /// The borrowed connection, guarded against concurrent access.
    fn connection(&self) -> MutexGuard<'_, Connection> {
        self.connection.lock().expect("the database is not poisoned")
    }
}

/// Applies the settings every connection needs.
fn configure(connection: &Connection) -> Result<(), rusqlite::Error> {
    connection.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA foreign_keys = ON;",
    )?;
    Ok(())
}
