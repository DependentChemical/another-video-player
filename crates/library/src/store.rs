use std::path::Path;

use database::{Database, rusqlite};
use rusqlite::{Connection, OptionalExtension, params};

use crate::Error;

/// The columns every queue query reads, in the order [`entry_from_row`]
/// expects.
const COLUMNS: &str = "position, url, title, thumbnail, current, resolved_source, resolved_at";

/// One item of the active queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueEntry {
    /// Position in the queue, starting at zero.
    pub position: i64,
    /// The URL the entry was added from.
    pub url: String,
    /// Human readable title of the media.
    pub title: String,
    /// A thumbnail for the entry: the cached file when one was downloaded,
    /// otherwise the remote URL the extractor reported.
    pub thumbnail: Option<String>,
    /// Whether this entry is the one playing now.
    pub current: bool,
    /// A serialized resolver result that may be reused while it remains fresh.
    pub resolved_source: Option<String>,
    /// Unix timestamp when the resolver result was created.
    pub resolved_at: Option<i64>,
}

/// The active queue, stored in a SQLite database.
///
/// The handle is shared, so the queue can be read from the thread that draws
/// the interface and written from the background work at the same time.
pub struct Queue {
    pub(crate) db: Database,
}

impl Queue {
    /// Opens a queue, creating the database and its table when needed.
    pub fn open(path: impl AsRef<Path>) -> Result<Queue, Error> {
        let db = Database::open(path)?;
        let queue = Queue { db };
        queue.migrate()?;
        Ok(queue)
    }

    /// Opens a queue that lives only in memory, which is what tests use.
    pub fn in_memory() -> Result<Queue, Error> {
        let db = Database::in_memory()?;
        let queue = Queue { db };
        queue.migrate()?;
        Ok(queue)
    }

    /// Creates the table the queue lives in.
    fn migrate(&self) -> Result<(), Error> {
        self.db
            .with_connection(|connection| {
                connection.execute_batch(
                    "CREATE TABLE IF NOT EXISTS active_queue (
                        id INTEGER PRIMARY KEY AUTOINCREMENT,
                        position INTEGER NOT NULL,
                        url TEXT NOT NULL,
                        title TEXT NOT NULL DEFAULT '',
                        thumbnail TEXT,
                        current INTEGER NOT NULL DEFAULT 0,
                        resolved_source TEXT,
                        resolved_at INTEGER
                    );
                    CREATE INDEX IF NOT EXISTS active_queue_position
                        ON active_queue(position);",
                )?;
                ensure_column(connection, "resolved_source", "TEXT")?;
                ensure_column(connection, "resolved_at", "INTEGER")
            })
            .map_err(Error::from)
    }

    /// Every entry, in the order it will be played.
    pub fn entries(&self) -> Result<Vec<QueueEntry>, Error> {
        self.db
            .with_connection(|connection| {
                let mut statement = connection
                    .prepare(&format!("SELECT {COLUMNS} FROM active_queue ORDER BY position"))?;
                let rows = statement.query_map([], entry_from_row)?;
                rows.collect::<Result<Vec<_>, _>>()
            })
            .map_err(Error::from)
    }

    /// The entry that is playing now, if the queue names one.
    pub fn current(&self) -> Result<Option<QueueEntry>, Error> {
        self.db.with_connection(current_in).map_err(Error::from)
    }

    /// Appends an entry and returns it.
    ///
    /// The first entry of an empty queue becomes the current one, so a queue
    /// that is built one video at a time never loses track of what plays.
    pub fn push(
        &self,
        url: impl Into<String>,
        title: impl Into<String>,
        thumbnail: Option<&str>,
    ) -> Result<QueueEntry, Error> {
        let url = url.into();
        let title = title.into();
        let thumbnail = thumbnail.map(str::to_string);
        self.db
            .with_connection(|connection| {
                let count: i64 =
                    connection
                        .query_row("SELECT COUNT(*) FROM active_queue", [], |row| row.get(0))?;
                let position: i64 = connection.query_row(
                    "SELECT COALESCE(MAX(position), -1) + 1 FROM active_queue",
                    [],
                    |row| row.get(0),
                )?;
                let entry = QueueEntry {
                    position,
                    url,
                    title,
                    thumbnail,
                    current: count == 0,
                    resolved_source: None,
                    resolved_at: None,
                };
                connection.execute(
                    "INSERT INTO active_queue (position, url, title, thumbnail, current)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        entry.position,
                        entry.url,
                        entry.title,
                        entry.thumbnail,
                        entry.current as i64
                    ],
                )?;
                Ok(entry)
            })
            .map_err(Error::from)
    }

    /// Replaces the whole queue with a single, current entry.
    ///
    /// This is what "play now" does: the old queue is dropped and the new
    /// media becomes the only thing that plays.
    pub fn replace_with(
        &self,
        url: impl Into<String>,
        title: impl Into<String>,
        thumbnail: Option<&str>,
    ) -> Result<QueueEntry, Error> {
        let url = url.into();
        let title = title.into();
        let thumbnail = thumbnail.map(str::to_string);
        self.db
            .with_connection(|connection| {
                connection.execute("DELETE FROM active_queue", [])?;
                let entry = QueueEntry {
                    position: 0,
                    url,
                    title,
                    thumbnail,
                    current: true,
                    resolved_source: None,
                    resolved_at: None,
                };
                connection.execute(
                    "INSERT INTO active_queue (position, url, title, thumbnail, current)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        entry.position,
                        entry.url,
                        entry.title,
                        entry.thumbnail,
                        entry.current as i64
                    ],
                )?;
                Ok(entry)
            })
            .map_err(Error::from)
    }

    /// Moves the current mark to the next entry and returns it.
    ///
    /// When no entry is current yet the first one becomes current, so a queue
    /// that was filled without playing anything still starts at its head.
    /// When the last entry was current, the mark is cleared and `None` is
    /// returned: the queue has been played to its end.
    pub fn advance(&self) -> Result<Option<QueueEntry>, Error> {
        self.db
            .with_connection(|connection| {
                let current_position: Option<i64> = connection
                    .query_row(
                        "SELECT position FROM active_queue WHERE current = 1 ORDER BY position LIMIT 1",
                        [],
                        |row| row.get(0),
                    )
                    .optional()?;

                let Some(position) = current_position else {
                    let first: Option<i64> = connection
                        .query_row(
                            "SELECT position FROM active_queue ORDER BY position LIMIT 1",
                            [],
                            |row| row.get(0),
                        )
                        .optional()?;
                    let Some(first) = first else {
                        return Ok(None);
                    };
                    connection.execute(
                        "UPDATE active_queue SET current = 1 WHERE position = ?1",
                        params![first],
                    )?;
                    return current_in(connection);
                };

                let next: Option<i64> = connection
                    .query_row(
                        "SELECT position FROM active_queue WHERE position > ?1 ORDER BY position LIMIT 1",
                        params![position],
                        |row| row.get(0),
                    )
                    .optional()?;

                connection.execute("UPDATE active_queue SET current = 0", [])?;
                let Some(next) = next else {
                    return Ok(None);
                };
                connection.execute(
                    "UPDATE active_queue SET current = 1 WHERE position = ?1",
                    params![next],
                )?;
                current_in(connection)
            })
            .map_err(Error::from)
    }

    /// Replaces the thumbnail of an entry.
    pub fn set_thumbnail(&self, position: i64, thumbnail: Option<&str>) -> Result<(), Error> {
        self.db
            .with_connection(|connection| {
                connection.execute(
                    "UPDATE active_queue SET thumbnail = ?2 WHERE position = ?1",
                    params![position, thumbnail],
                )?;
                Ok(())
            })
            .map_err(Error::from)
    }

    /// Updates the displayed details of a queue entry after URL resolution.
    pub fn update_details(
        &self,
        position: i64,
        title: &str,
        thumbnail: Option<&str>,
    ) -> Result<(), Error> {
        self.db
            .with_connection(|connection| {
                connection.execute(
                    "UPDATE active_queue SET title = ?2, thumbnail = ?3 WHERE position = ?1",
                    params![position, title, thumbnail],
                )?;
                Ok(())
            })
            .map_err(Error::from)
    }

    /// Saves or clears the resolver result associated with an entry.
    pub fn set_resolved_source(
        &self,
        position: i64,
        source: Option<&str>,
        resolved_at: Option<i64>,
    ) -> Result<(), Error> {
        self.db
            .with_connection(|connection| {
                connection.execute(
                    "UPDATE active_queue
                     SET resolved_source = ?2, resolved_at = ?3 WHERE position = ?1",
                    params![position, source, resolved_at],
                )?;
                Ok(())
            })
            .map_err(Error::from)
    }

    /// How many entries the queue holds.
    pub fn count(&self) -> Result<i64, Error> {
        self.db
            .with_connection(|connection| {
                connection.query_row("SELECT COUNT(*) FROM active_queue", [], |row| row.get(0))
            })
            .map_err(Error::from)
    }

    /// Empties the queue.
    pub fn clear(&self) -> Result<(), Error> {
        self.db
            .with_connection(|connection| {
                connection.execute("DELETE FROM active_queue", [])?;
                Ok(())
            })
            .map_err(Error::from)
    }
}

/// Reads the current entry of an already locked connection.
fn current_in(connection: &Connection) -> Result<Option<QueueEntry>, rusqlite::Error> {
    connection
        .query_row(
            &format!(
                "SELECT {COLUMNS} FROM active_queue WHERE current = 1 ORDER BY position LIMIT 1"
            ),
            [],
            entry_from_row,
        )
        .optional()
}

/// Adds a column to an existing queue database when an older app created it.
fn ensure_column(
    connection: &Connection,
    name: &str,
    definition: &str,
) -> Result<(), rusqlite::Error> {
    let mut statement = connection.prepare("PRAGMA table_info(active_queue)")?;
    let columns = statement.query_map([], |row| row.get::<_, String>(1))?;
    for column in columns {
        if column? == name {
            return Ok(());
        }
    }
    connection.execute_batch(&format!("ALTER TABLE active_queue ADD COLUMN {name} {definition}"))
}

/// Turns a row of [`COLUMNS`] into an entry.
fn entry_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<QueueEntry> {
    Ok(QueueEntry {
        position: row.get(0)?,
        url: row.get(1)?,
        title: row.get(2)?,
        thumbnail: row.get(3)?,
        current: row.get::<_, i64>(4)? != 0,
        resolved_source: row.get(5)?,
        resolved_at: row.get(6)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queue() -> Queue {
        Queue::in_memory().expect("an in-memory queue opens")
    }

    #[test]
    fn an_empty_queue_has_no_current_entry() {
        let queue = queue();
        assert_eq!(queue.count().expect("count works"), 0);
        assert!(queue.current().expect("current works").is_none());
    }

    #[test]
    fn the_first_pushed_entry_becomes_current() {
        let queue = queue();
        let entry = queue.push("https://example.com/a", "A", None).expect("push works");
        assert_eq!(entry.position, 0);
        assert!(entry.current);
        assert_eq!(
            queue.current().expect("current works").expect("one entry").url,
            "https://example.com/a"
        );
    }

    #[test]
    fn later_entries_are_appended_without_taking_over() {
        let queue = queue();
        queue.push("https://example.com/a", "A", None).expect("push works");
        let second =
            queue.push("https://example.com/b", "B", Some("/tmp/b.jpg")).expect("push works");

        assert_eq!(second.position, 1);
        assert!(!second.current);
        assert_eq!(second.thumbnail.as_deref(), Some("/tmp/b.jpg"));
        assert_eq!(
            queue.current().expect("current works").expect("one entry").url,
            "https://example.com/a"
        );
        assert_eq!(queue.entries().expect("entries work").len(), 2);
    }

    #[test]
    fn replace_with_drops_the_old_queue() {
        let queue = queue();
        queue.push("https://example.com/a", "A", None).expect("push works");
        queue.push("https://example.com/b", "B", None).expect("push works");

        let entry = queue.replace_with("https://example.com/c", "C", None).expect("replace works");
        assert_eq!(entry.position, 0);
        assert!(entry.current);
        assert_eq!(queue.count().expect("count works"), 1);
        assert_eq!(
            queue.current().expect("current works").expect("one entry").url,
            "https://example.com/c"
        );
    }

    #[test]
    fn advance_moves_the_current_mark_forward() {
        let queue = queue();
        queue.push("https://example.com/a", "A", None).expect("push works");
        queue.push("https://example.com/b", "B", None).expect("push works");

        let next = queue.advance().expect("advance works").expect("a next entry");
        assert_eq!(next.url, "https://example.com/b");
        assert!(next.current);
        assert!(!queue.entries().expect("entries work")[0].current);
    }

    #[test]
    fn advance_past_the_last_entry_clears_the_mark() {
        let queue = queue();
        queue.push("https://example.com/a", "A", None).expect("push works");

        assert!(queue.advance().expect("advance works").is_none());
        assert!(queue.current().expect("current works").is_none());
        assert_eq!(queue.count().expect("count works"), 1);
    }

    #[test]
    fn advance_on_a_queue_without_a_mark_starts_at_the_head() {
        let queue = queue();
        queue.push("https://example.com/a", "A", None).expect("push works");
        queue.push("https://example.com/b", "B", None).expect("push works");
        queue
            .db
            .with_connection(|connection| {
                connection.execute("UPDATE active_queue SET current = 0", [])?;
                Ok(())
            })
            .expect("clear mark");

        let first = queue.advance().expect("advance works").expect("one entry");
        assert_eq!(first.url, "https://example.com/a");
        assert!(first.current);
    }

    #[test]
    fn thumbnails_can_be_filled_in_later() {
        let queue = queue();
        queue.push("https://example.com/a", "A", None).expect("push works");
        queue.set_thumbnail(0, Some("/cache/a.jpg")).expect("set works");
        assert_eq!(
            queue.entries().expect("entries work")[0].thumbnail.as_deref(),
            Some("/cache/a.jpg")
        );
    }

    #[test]
    fn a_resolved_source_is_saved_with_its_freshness_time() {
        let queue = queue();
        queue.push("https://example.com/a", "A", None).expect("push works");
        queue.set_resolved_source(0, Some("{\"title\":\"A\"}"), Some(1234)).expect("source saves");

        let current = queue.current().expect("current works").expect("one entry");
        assert_eq!(current.resolved_source.as_deref(), Some("{\"title\":\"A\"}"));
        assert_eq!(current.resolved_at, Some(1234));
    }

    #[test]
    fn an_existing_queue_database_gains_source_columns() {
        let db = database::Database::in_memory().expect("database opens");
        db.with_connection(|connection| {
            connection.execute_batch(
                "CREATE TABLE active_queue (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    position INTEGER NOT NULL,
                    url TEXT NOT NULL,
                    title TEXT NOT NULL DEFAULT '',
                    thumbnail TEXT,
                    current INTEGER NOT NULL DEFAULT 0
                );",
            )?;
            Ok(())
        })
        .expect("legacy schema created");
        let queue = Queue { db };
        queue.migrate().expect("legacy schema migrates");
        queue.push("https://example.com/a", "A", None).expect("push works");
        queue
            .set_resolved_source(0, Some("source"), Some(1234))
            .expect("new columns are available");
        assert_eq!(
            queue.current().expect("current works").expect("one entry").resolved_at,
            Some(1234)
        );
    }

    #[test]
    fn clear_empties_the_queue() {
        let queue = queue();
        queue.push("https://example.com/a", "A", None).expect("push works");
        queue.clear().expect("clear works");
        assert_eq!(queue.count().expect("count works"), 0);
    }

    #[test]
    fn a_queue_survives_being_reopened_from_a_file() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the clock is after the Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("flatline-queue-{unique}.db"));

        {
            let queue = Queue::open(&path).expect("the queue opens");
            queue.push("https://example.com/a", "A", None).expect("push works");
            queue.push("https://example.com/b", "B", None).expect("push works");
            queue.advance().expect("advance works");
        }

        let reopened = Queue::open(&path).expect("the queue reopens");
        assert_eq!(reopened.count().expect("count works"), 2);
        assert_eq!(
            reopened.current().expect("current works").expect("one entry").url,
            "https://example.com/b"
        );
        let _ = std::fs::remove_file(path);
    }
}
