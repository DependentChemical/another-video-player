//! The thumbnail cache.
//!
//! Flatline keeps three sized copies of every thumbnail (full, mid and
//! small) under `cache/thumbnails`, so the same video is never fetched twice
//! and each screen draws the size it actually needs.
//!
//! A thumbnail is addressed by a stable id made of a prefix and the hash of
//! the cleaned video URL (`thumb_<md5>`), so a video that appears in the
//! queue twice reuses one thumbnail instead of being downloaded twice. The
//! prefix is what tells a new-style id apart from an old-style direct file
//! path.
//!
//! The database stores the thumbnail id, remote source URL, and remote
//! candidate URLs. File paths are derived from the cache directory at runtime
//! so the cache remains valid when the portable application directory moves.
//! Queue entries only hold the deterministic thumbnail reference; thumbnail
//! refresh reads queue URLs and updates this cache, never queue rows.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use database::Database;
use media_model::video_id_hash;
use rusqlite::{Connection, OptionalExtension, params};

use app_logging;

use crate::Error;
use image_process::{is_renderable_jpeg, normalize_to_jpeg, write_scaled_jpeg};

const USER_AGENT: &str = concat!("Flatline/", env!("CARGO_PKG_VERSION"));

/// Prefix that distinguishes a new-style thumbnail id from an old-style
/// direct file path.
pub const THUMBNAIL_ID_PREFIX: &str = "thumb_";

/// The largest preview the queue list needs; the row itself only draws it at
/// 96 by 54 logical pixels, but a little headroom keeps it sharp on HiDPI.
const SMALL_MAX_WIDTH: u32 = 320;
const SMALL_MAX_HEIGHT: u32 = 180;

/// The size used by the "add to queue / play now" dialog.
const MID_MAX_WIDTH: u32 = 640;
const MID_MAX_HEIGHT: u32 = 360;

/// Scratch files older than this were left behind by an interrupted
/// download; no healthy fetch keeps one around that long.
const STALE_PARTIAL_AGE: Duration = Duration::from_secs(60 * 60);

/// The directory downloaded thumbnails are kept in.
#[derive(Debug, Clone)]
pub struct ThumbnailCache {
    db: Arc<Database>,
    directory: PathBuf,
}

impl ThumbnailCache {
    /// Creates a cache that writes images into `directory` and keeps its
    /// records in the database `db`.
    ///
    /// The `thumbnail_cache` table is created when it is missing.
    pub fn new(db: Database, directory: impl Into<PathBuf>) -> ThumbnailCache {
        let directory = directory.into();
        let _ = fs::create_dir_all(&directory);
        let cache = ThumbnailCache { db: Arc::new(db), directory };
        cache.ensure_schema().ok();
        cache
    }

    /// The directory the cached images live in.
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Whether `value` is a new-style thumbnail id rather than an old-style
    /// direct file path.
    pub fn is_thumbnail_id(value: &str) -> bool {
        value.starts_with(THUMBNAIL_ID_PREFIX)
    }

    /// The stable id of the thumbnail for `video_url`.
    ///
    /// The id is the prefix plus the hash of the cleaned video URL, so the
    /// same video always reuses the same thumbnail and is never downloaded
    /// twice.
    pub fn thumbnail_id_for(video_url: &str) -> String {
        format!("{}{}", THUMBNAIL_ID_PREFIX, video_id_hash(video_url))
    }

    /// Ensures the `thumbnail_cache` table exists and migrates older layouts.
    fn ensure_schema(&self) -> Result<(), Error> {
        self.db
            .with_connection(|connection| {
                connection.execute_batch(
                    "CREATE TABLE IF NOT EXISTS thumbnail_cache (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    thumbnail_id TEXT NOT NULL UNIQUE,
                    source_url TEXT,
                    thumbnail_urls TEXT,
                    created_at INTEGER NOT NULL
                );
                CREATE INDEX IF NOT EXISTS thumbnail_cache_thumbnail_id
                    ON thumbnail_cache(thumbnail_id);",
                )?;
                ensure_thumbnail_cache_column(connection, "thumbnail_urls", "TEXT")?;
                migrate_thumbnail_cache(connection)?;
                Ok(())
            })
            .map_err(Error::from)
    }

    /// Returns whether a row for `id` already exists.
    fn thumbnail_exists(&self, id: &str) -> Result<bool, Error> {
        let exists = self
            .db
            .with_connection(|connection| {
                let count: i64 = connection.query_row(
                    "SELECT COUNT(*) FROM thumbnail_cache WHERE thumbnail_id = ?1",
                    params![id],
                    |row| row.get(0),
                )?;
                Ok(count > 0)
            })
            .map_err(Error::from)?;
        Ok(exists)
    }

    /// The path the full-size copy of `id` is stored at.
    fn full_file(&self, id: &str) -> PathBuf {
        self.directory.join(format!("{id}.jpg"))
    }

    /// The path the mid-size copy of `id` is stored at.
    fn mid_file(&self, id: &str) -> PathBuf {
        self.directory.join(format!("{id}.mid.jpg"))
    }

    /// The path the small copy of `id` is stored at.
    fn small_file(&self, id: &str) -> PathBuf {
        self.directory.join(format!("{id}.small.jpg"))
    }

    /// Returns the full-size thumbnail for `id`, when it is on disk and
    /// readable.
    pub fn full_path(&self, id: &str) -> Option<PathBuf> {
        if !self.thumbnail_exists(id).unwrap_or(false) {
            return None;
        }
        let path = self.full_file(id);
        (path.is_file() && is_renderable_jpeg(&path)).then_some(path)
    }

    /// Returns the mid-size thumbnail for `id`, when it is on disk and
    /// readable.
    pub fn mid_path(&self, id: &str) -> Option<PathBuf> {
        if !self.thumbnail_exists(id).unwrap_or(false) {
            return None;
        }
        let path = self.mid_file(id);
        (path.is_file() && is_renderable_jpeg(&path)).then_some(path)
    }

    /// Returns the small thumbnail for `id`, when it is on disk and readable.
    pub fn small_path(&self, id: &str) -> Option<PathBuf> {
        if !self.thumbnail_exists(id).unwrap_or(false) {
            return None;
        }
        let path = self.small_file(id);
        (path.is_file() && is_renderable_jpeg(&path)).then_some(path)
    }

    /// Returns the cached thumbnail for `video_url`, downloading it from the
    /// first working URL in `thumbnail_urls`.
    ///
    /// The same video always yields the same id, so a queue that lists the
    /// video twice downloads the thumbnail only once. When a thumbnail is
    /// already cached the network is never touched. Remote candidate URLs are
    /// persisted in `thumbnail_cache` so future refreshes can read them without
    /// touching queue rows. Returns the thumbnail id and the URL that succeeded.
    pub fn fetch(
        &self,
        video_url: &str,
        thumbnail_urls: &[String],
    ) -> Result<(String, String), Error> {
        let id = ThumbnailCache::thumbnail_id_for(video_url);
        let candidates = thumbnail_candidate_urls(thumbnail_urls);
        let source_url = candidates.first().cloned();
        self.upsert_thumbnail_metadata(&id, source_url.as_deref(), &candidates)?;
        if self.full_path(&id).is_some() {
            let cached_source_url = self.thumbnail_source_url(&id)?.unwrap_or_default();
            app_logging::debug(format!(
                "thumbnail cache hit: id={id} video_url={video_url} source_url={cached_source_url}"
            ));
            return Ok((id, cached_source_url));
        }
        if self.thumbnail_exists(&id)? {
            app_logging::warning(format!(
                "thumbnail cache row broken for {id} video_url={video_url}; deleting files before re-download"
            ));
            self.remove_thumbnail_files_only(&id);
        }

        if candidates.is_empty() {
            app_logging::warning(format!(
                "thumbnail download skipped for {video_url}: no thumbnail URL candidates id={id}"
            ));
            return Err(Error::Http("missing thumbnail URL".to_string()));
        }

        let full_path = self.full_file(&id);
        let mid_path = self.mid_file(&id);
        let small_path = self.small_file(&id);
        fs::create_dir_all(&self.directory)?;
        self.remove_stale_partials();

        let mut last_error = None;
        for (index, thumbnail_url) in candidates.iter().enumerate() {
            app_logging::debug(format!(
                "thumbnail candidate {}/{} for {video_url}: id={id} url={thumbnail_url}",
                index + 1,
                candidates.len()
            ));
            match self.download_thumbnail_candidate(thumbnail_url) {
                Ok(temporary) => {
                    if let Err(error) = self.install_thumbnail_files(
                        &temporary,
                        &full_path,
                        &mid_path,
                        &small_path,
                        thumbnail_url,
                    ) {
                        last_error = Some(error.to_string());
                        continue;
                    }
                    let now = unix_time();
                    let candidates_json = serialize_thumbnail_urls(&candidates);
                    self.db
                        .with_connection(|connection| {
                            connection.execute(
                                "INSERT INTO thumbnail_cache (thumbnail_id, source_url, thumbnail_urls, created_at)
                                 VALUES (?1, ?2, ?3, ?4)
                                 ON CONFLICT(thumbnail_id) DO UPDATE SET
                                     source_url = excluded.source_url,
                                     thumbnail_urls = excluded.thumbnail_urls",
                                params![id, thumbnail_url, candidates_json, now],
                            )?;
                            Ok(())
                        })
                        .map_err(Error::from)?;
                    app_logging::notice(format!(
                        "thumbnail cached: id={id} video_url={video_url} source_url={thumbnail_url} candidates={}",
                        candidates.len()
                    ));
                    return Ok((id, thumbnail_url.clone()));
                }
                Err(error) => last_error = Some(error.to_string()),
            }
        }

        Err(Error::Http(last_error.unwrap_or_else(|| "missing thumbnail URL".to_string())))
    }

    /// Stores remote thumbnail candidates for a video without downloading files.
    ///
    /// The queue remains the source of queue metadata; this only updates the
    /// cache's own metadata so refresh can discover URLs without mutating queue
    /// rows.
    pub fn remember_thumbnail_urls(
        &self,
        video_url: &str,
        thumbnail_urls: &[String],
    ) -> Result<String, Error> {
        let id = ThumbnailCache::thumbnail_id_for(video_url);
        let candidates = thumbnail_candidate_urls(thumbnail_urls);
        let source_url = candidates.first().cloned();
        self.upsert_thumbnail_metadata(&id, source_url.as_deref(), &candidates)?;
        app_logging::debug(format!(
            "thumbnail cache metadata updated: id={id} video_url={video_url} candidates={} source_url={}",
            candidates.len(),
            source_url.as_deref().unwrap_or("<none>")
        ));
        Ok(id)
    }

    /// Returns remote thumbnail candidate URLs previously stored for `id`.
    pub fn thumbnail_urls(&self, id: &str) -> Result<Vec<String>, Error> {
        self.db
            .with_connection(|connection| {
                let raw: Option<Option<String>> = connection
                    .query_row(
                        "SELECT thumbnail_urls FROM thumbnail_cache WHERE thumbnail_id = ?1",
                        params![id],
                        |row| row.get(0),
                    )
                    .optional()?;
                Ok(parse_thumbnail_urls(raw.flatten().as_deref()))
            })
            .map_err(Error::from)
    }

    fn upsert_thumbnail_metadata(
        &self,
        id: &str,
        source_url: Option<&str>,
        thumbnail_urls: &[String],
    ) -> Result<(), Error> {
        let now = unix_time();
        let urls = serialize_thumbnail_urls(thumbnail_urls);
        self.db
            .with_connection(|connection| {
                connection.execute(
                    "INSERT INTO thumbnail_cache (thumbnail_id, source_url, thumbnail_urls, created_at)
                     VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(thumbnail_id) DO UPDATE SET
                         source_url = COALESCE(excluded.source_url, thumbnail_cache.source_url),
                         thumbnail_urls = CASE
                             WHEN excluded.thumbnail_urls IS NULL OR excluded.thumbnail_urls = '' THEN thumbnail_cache.thumbnail_urls
                             ELSE excluded.thumbnail_urls
                         END",
                    params![id, source_url, urls, now],
                )?;
                Ok(())
            })
            .map_err(Error::from)
    }

    fn remove_thumbnail_files_only(&self, id: &str) {
        remove_thumbnail_files(&self.directory, id);
    }

    fn thumbnail_source_url(&self, id: &str) -> Result<Option<String>, Error> {
        self.db
            .with_connection(|connection| {
                connection
                    .query_row(
                        "SELECT source_url FROM thumbnail_cache WHERE thumbnail_id = ?1",
                        params![id],
                        |row| row.get::<_, Option<String>>(0),
                    )
                    .optional()
                    .map(Option::flatten)
            })
            .map_err(Error::from)
    }

    fn download_thumbnail_candidate(&self, thumbnail_url: &str) -> Result<PathBuf, Error> {
        let temp_name = format!("{:016x}.part", fnv1a(thumbnail_url.as_bytes()));
        let temporary = self.directory.join(temp_name);
        let mut last_error = None;
        for (attempt, force_ipv4) in [(1u32, false), (1u32, true)] {
            let mode = if force_ipv4 { "ipv4" } else { "default" };
            let _scratch = ScratchFile(&temporary);
            app_logging::debug(format!(
                "thumbnail download started (attempt {attempt}/1, {mode}): {thumbnail_url}"
            ));
            let result = (|| -> Result<(), Error> {
                let agent = ureq::Agent::config_builder()
                    .ip_family(if force_ipv4 {
                        ureq::config::IpFamily::Ipv4Only
                    } else {
                        ureq::config::IpFamily::Any
                    })
                    .build()
                    .new_agent();
                let mut response = agent
                    .get(thumbnail_url)
                    .header("User-Agent", USER_AGENT)
                    .call()
                    .map_err(|error| Error::Http(error.to_string()))?;
                let mut file = fs::File::create(&temporary)?;
                let mut reader = response.body_mut().as_reader();
                io::copy(&mut reader, &mut file)?;
                file.sync_all()?;
                Ok(())
            })();
            match result {
                Ok(()) => return Ok(temporary.clone()),
                Err(error) => {
                    let message = error.to_string();
                    app_logging::warning(format!(
                        "thumbnail download attempt {attempt}/1 ({mode}) failed for {thumbnail_url}: {message}"
                    ));
                    last_error = Some(message);
                    let _ = fs::remove_file(&temporary);
                }
            }
        }
        Err(Error::Http(last_error.unwrap_or_else(|| "missing thumbnail URL".to_string())))
    }

    fn install_thumbnail_files(
        &self,
        temporary: &Path,
        full_path: &Path,
        mid_path: &Path,
        small_path: &Path,
        thumbnail_url: &str,
    ) -> Result<(), Error> {
        normalize_to_jpeg(temporary, full_path).map_err(|error| {
            app_logging::warning(format!(
                "thumbnail normalize failed for {thumbnail_url}: {error}"
            ));
            Error::from(error)
        })?;
        write_scaled_jpeg(full_path, mid_path, MID_MAX_WIDTH, MID_MAX_HEIGHT).map_err(|error| {
            app_logging::warning(format!(
                "thumbnail mid resize failed for {thumbnail_url}: {error}"
            ));
            Error::from(error)
        })?;
        write_scaled_jpeg(full_path, small_path, SMALL_MAX_WIDTH, SMALL_MAX_HEIGHT).map_err(
            |error| {
                app_logging::warning(format!(
                    "thumbnail small resize failed for {thumbnail_url}: {error}"
                ));
                Error::from(error)
            },
        )?;
        Ok(())
    }

    /// Imports an old-style direct file path into the new cache.
    ///
    /// When the legacy file is already a valid JPEG it is moved straight into
    /// the full-size slot without a lossy re-encode; otherwise it is decoded
    /// and re-encoded. The mid and small copies are always derived from the
    /// full-size file. Returns the thumbnail id the active queue should now
    /// point at. When an entry for the video already exists nothing is done
    /// and that id is returned.
    pub fn register_legacy(&self, video_url: &str, legacy_path: &Path) -> Result<String, Error> {
        let id = ThumbnailCache::thumbnail_id_for(video_url);
        if self.thumbnail_exists(&id)? {
            return Ok(id);
        }
        if !legacy_path.is_file() {
            return Err(Error::Io(io::Error::new(
                io::ErrorKind::NotFound,
                "legacy thumbnail file is missing",
            )));
        }

        let full_path = self.full_file(&id);
        let mid_path = self.mid_file(&id);
        let small_path = self.small_file(&id);

        fs::create_dir_all(&self.directory)?;

        // A legacy JPEG is already a finished full-size image: move it into
        // place instead of re-encoding it, so we avoid generational quality
        // loss. Anything that is not a valid JPEG still goes through the
        // normalizer.
        if is_renderable_jpeg(legacy_path) {
            let _ = fs::remove_file(&full_path);
            fs::rename(legacy_path, &full_path)?;
        } else {
            normalize_to_jpeg(legacy_path, &full_path)?;
        }
        write_scaled_jpeg(&full_path, &mid_path, MID_MAX_WIDTH, MID_MAX_HEIGHT)
            .map_err(Error::from)?;
        write_scaled_jpeg(&full_path, &small_path, SMALL_MAX_WIDTH, SMALL_MAX_HEIGHT)
            .map_err(Error::from)?;

        let now = unix_time();
        self.db
            .with_connection(|connection| {
                connection.execute(
                    "INSERT INTO thumbnail_cache (thumbnail_id, source_url, created_at)
                     VALUES (?1, ?2, ?3)
                     ON CONFLICT(thumbnail_id) DO UPDATE SET source_url = COALESCE(thumbnail_cache.source_url, excluded.source_url)",
                    params![id, Option::<String>::None, now],
                )?;
                Ok(())
            })
            .map_err(Error::from)?;

        Ok(id)
    }

    /// Deletes the thumbnail record `id` and its three image files, but only
    /// when no entry in any playlist still points at it.
    pub fn release_thumbnail(&self, id: &str) -> Result<(), Error> {
        let referenced = self
            .db
            .with_connection(|connection| Ok(referenced_thumbnail_ids(connection)?.contains(id)))
            .map_err(Error::from)?;

        if !referenced {
            app_logging::debug(format!("thumbnail cache releasing thumbnail: {id}"));
            self.delete_thumbnail(id)?;
        }
        Ok(())
    }

    /// Deletes every thumbnail record and image file that no entry in any
    /// playlist points at.
    ///
    /// Recently created rows are left alone for a short grace period so a
    /// background download that has written the cache row but not yet attached
    /// it to a queue entry cannot be deleted out from under it.
    pub fn release_unreferenced_thumbnails(&self) -> Result<(), Error> {
        let now = unix_time();
        self.db
            .with_connection(|connection| {
                let referenced = referenced_thumbnail_ids(connection)?;

                let mut statement =
                    connection.prepare("SELECT thumbnail_id, created_at FROM thumbnail_cache")?;
                let rows = statement
                    .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)))?;
                for row in rows.filter_map(Result::ok) {
                    let (id, created_at) = row;
                    if referenced.contains(&id) {
                        continue;
                    }
                    if now.saturating_sub(created_at) < 300 {
                        app_logging::debug(format!(
                            "thumbnail cache keeping unreferenced recent row: {id}"
                        ));
                        continue;
                    }
                    app_logging::warning(format!(
                        "thumbnail cache releasing unreferenced thumbnail: {id}"
                    ));
                    let _ = connection.execute(
                        "DELETE FROM thumbnail_cache WHERE thumbnail_id = ?1",
                        params![id],
                    );
                    remove_thumbnail_files(&self.directory, &id);
                }
                Ok(())
            })
            .map_err(Error::from)
    }

    /// Deletes the thumbnail record `id` and its three image files.
    fn delete_thumbnail(&self, id: &str) -> Result<(), Error> {
        let _ = self
            .db
            .with_connection(|connection| {
                connection
                    .execute("DELETE FROM thumbnail_cache WHERE thumbnail_id = ?1", params![id])
            })
            .map_err(Error::from)?;
        remove_thumbnail_files(&self.directory, id);
        Ok(())
    }

    /// Removes every image file in the cache directory that does not belong
    /// to a row in `thumbnail_cache`, plus any scratch file left behind.
    ///
    /// Files whose id is still referenced by the active queue are kept even if
    /// their cache row is missing, so a crash that lost the row does not wipe
    /// thumbnails that can be repaired.
    ///
    /// This is the start-up orphan cleanup and is meant to run in the
    /// background.
    pub fn prune_orphans(&self) -> Result<(), Error> {
        let referenced = self
            .db
            .with_connection(|connection| {
                let mut statement =
                    connection.prepare("SELECT thumbnail_id FROM thumbnail_cache")?;
                let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
                Ok(rows.filter_map(Result::ok).collect::<HashSet<_>>())
            })
            .map_err(Error::from)?;
        // A thumbnail any playlist still references is kept even when its
        // cache row is missing, so a crash does not wipe a repairable file.
        let protected_ids =
            self.db.with_connection(referenced_thumbnail_ids).map_err(Error::from)?;

        let entries = match fs::read_dir(&self.directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(Error::Io(error)),
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let name = match path.file_name().and_then(|n| n.to_str()) {
                Some(name) => name,
                None => continue,
            };
            let Some(id) = thumbnail_id_from_file_name(name) else {
                if name.ends_with(".part") {
                    continue;
                }
                let _ = fs::remove_file(&path);
                continue;
            };
            if !referenced.contains(&id) && !protected_ids.contains(&id) {
                let _ = fs::remove_file(&path);
            }
        }
        Ok(())
    }

    /// Recreates a missing `thumbnail_cache` row for `id` when its files exist.
    ///
    /// This repairs thumbnails whose files survived a crash but whose database
    /// record did not, so the active queue can show them without downloading
    /// them again.
    pub fn repair_row(&self, id: &str) -> Result<bool, Error> {
        if self.thumbnail_exists(id)? {
            return Ok(true);
        }
        let full = self.full_file(id);
        let mid = self.mid_file(id);
        let small = self.small_file(id);
        if !full.is_file() || !mid.is_file() || !small.is_file() || !is_renderable_jpeg(&full) {
            return Ok(false);
        }
        let now = unix_time();
        self.db
            .with_connection(|connection| {
                connection.execute(
                    "INSERT OR IGNORE INTO thumbnail_cache (thumbnail_id, source_url, created_at)
                     VALUES (?1, ?2, ?3)",
                    params![id, Option::<String>::None, now],
                )?;
                Ok(())
            })
            .map_err(Error::from)?;
        Ok(true)
    }

    /// Removes scratch files left behind by an interrupted download.
    fn remove_stale_partials(&self) {
        let Ok(entries) = fs::read_dir(&self.directory) else {
            return;
        };
        let now = SystemTime::now();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("part") {
                continue;
            }
            let stale = entry
                .metadata()
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .and_then(|modified| now.duration_since(modified).ok())
                .is_some_and(|age| age >= STALE_PARTIAL_AGE);
            if stale {
                let _ = fs::remove_file(path);
            }
        }
    }
}

/// Migrates older `thumbnail_cache` layouts to the ID-only schema.
///
/// Older databases stored absolute `full_path/mid_path/small_path` columns.
/// The cache is now portable: files live at deterministic names under the
/// current cache directory and only the thumbnail id/source URL are stored.
/// Existing rows are preserved, local absolute source paths are scrubbed, and
/// missing files are copied from old absolute paths when available.
fn ensure_thumbnail_cache_column(
    connection: &Connection,
    name: &str,
    definition: &str,
) -> rusqlite::Result<()> {
    let mut statement = connection.prepare("PRAGMA table_info(thumbnail_cache)")?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<HashSet<_>, _>>()?;
    if columns.contains(name) {
        return Ok(());
    }
    connection.execute_batch(&format!("ALTER TABLE thumbnail_cache ADD COLUMN {name} {definition}"))
}

fn migrate_thumbnail_cache(connection: &Connection) -> rusqlite::Result<()> {
    let mut statement = connection.prepare("PRAGMA table_info(thumbnail_cache)")?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<HashSet<_>, _>>()?;
    if !columns.contains("full_path")
        && !columns.contains("mid_path")
        && !columns.contains("small_path")
    {
        return Ok(());
    }

    let cache_directory = thumbnail_cache_directory(connection);
    let legacy_rows = {
        let mut statement = connection.prepare(
            "SELECT thumbnail_id, source_url, full_path, mid_path, small_path, created_at
             FROM thumbnail_cache",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })?;
        rows.collect::<Result<Vec<_>, _>>()?
    };

    for (id, source_url, full, mid, small, _) in &legacy_rows {
        if let Some(cache_directory) = cache_directory.as_deref() {
            migrate_legacy_thumbnail_files(
                cache_directory,
                id,
                full.as_deref(),
                mid.as_deref(),
                small.as_deref(),
            );
        }
        let source_url = sanitize_source_url(source_url.as_deref());
        connection.execute(
            "UPDATE thumbnail_cache SET source_url = ?2 WHERE thumbnail_id = ?1",
            params![id, source_url],
        )?;
    }

    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS thumbnail_cache_new (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            thumbnail_id TEXT NOT NULL UNIQUE,
            source_url TEXT,
            thumbnail_urls TEXT,
            created_at INTEGER NOT NULL
        );
        INSERT OR REPLACE INTO thumbnail_cache_new (id, thumbnail_id, source_url, thumbnail_urls, created_at)
            SELECT id, thumbnail_id, source_url, thumbnail_urls, created_at FROM thumbnail_cache;
        DROP TABLE thumbnail_cache;
        ALTER TABLE thumbnail_cache_new RENAME TO thumbnail_cache;
        CREATE INDEX IF NOT EXISTS thumbnail_cache_thumbnail_id
            ON thumbnail_cache(thumbnail_id);",
    )?;
    Ok(())
}

/// Resolves the cache directory from the database path when possible.
///
/// The database lives at `<root>/app.db` and thumbnails at
/// `<root>/cache/thumbnails`.
fn thumbnail_cache_directory(connection: &Connection) -> Option<PathBuf> {
    let mut statement = connection.prepare("PRAGMA database_list").ok()?;
    let mut rows = statement.query([]).ok()?;
    while let Ok(Some(row)) = rows.next() {
        let name: String = row.get(1).ok()?;
        if name != "main" {
            continue;
        }
        let file: String = row.get(2).ok()?;
        if file.is_empty() {
            return None;
        }
        let root = Path::new(&file).parent()?;
        return Some(root.join("cache").join("thumbnails"));
    }
    None
}

/// Copies old absolute thumbnail files into the current cache directory.
fn migrate_legacy_thumbnail_files(
    cache_directory: &Path,
    id: &str,
    full: Option<&str>,
    mid: Option<&str>,
    small: Option<&str>,
) {
    let _ = fs::create_dir_all(cache_directory);
    for (target, source) in [
        (cache_directory.join(format!("{id}.jpg")), full),
        (cache_directory.join(format!("{id}.mid.jpg")), mid),
        (cache_directory.join(format!("{id}.small.jpg")), small),
    ] {
        if target.is_file() || !is_renderable_jpeg(&target) && target.exists() {
            continue;
        }
        let Some(source) = source else { continue };
        let source = Path::new(source);
        if source == target || !source.is_file() || !is_renderable_jpeg(source) {
            continue;
        }
        let _ = fs::copy(source, &target);
    }
}

/// Removes machine-local file paths from thumbnail source metadata.
fn sanitize_source_url(source_url: Option<&str>) -> Option<String> {
    let value = source_url?.trim();
    if value.is_empty() {
        return None;
    }
    if looks_like_local_path(value) {
        return None;
    }
    Some(value.to_string())
}

fn looks_like_local_path(value: &str) -> bool {
    let bytes = value.as_bytes();
    value.starts_with('/')
        || value.starts_with('\\')
        || value.starts_with("file://")
        || (bytes.len() >= 3 && bytes[1] == b':' && (bytes[2] == b'\\' || bytes[2] == b'/'))
}

fn parse_thumbnail_urls(raw: Option<&str>) -> Vec<String> {
    let Some(raw) = raw else { return Vec::new() };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    serde_json::from_str::<Vec<String>>(trimmed).unwrap_or_else(|_| vec![trimmed.to_string()])
}

fn serialize_thumbnail_urls(urls: &[String]) -> Option<String> {
    if urls.is_empty() { None } else { serde_json::to_string(urls).ok() }
}

fn thumbnail_candidate_urls(urls: &[String]) -> Vec<String> {
    let mut candidates = Vec::new();
    for url in urls {
        let trimmed = url.trim();
        if trimmed.is_empty() {
            continue;
        }
        push_thumbnail_candidate(&mut candidates, trimmed);
        for fallback in youtube_thumbnail_fallbacks(trimmed) {
            push_thumbnail_candidate(&mut candidates, &fallback);
        }
    }
    candidates
}

fn push_thumbnail_candidate(candidates: &mut Vec<String>, url: &str) {
    if !candidates.iter().any(|existing| existing == url) {
        candidates.push(url.to_string());
    }
}

fn youtube_thumbnail_fallbacks(url: &str) -> Vec<String> {
    let Ok(parsed) = url::Url::parse(url) else { return Vec::new() };
    let Some(host) = parsed.host_str() else { return Vec::new() };
    if host != "i.ytimg.com" && host != "img.youtube.com" {
        return Vec::new();
    }
    let segments = parsed.path_segments().map(|s| s.collect::<Vec<_>>()).unwrap_or_default();
    if segments.len() < 2 {
        return Vec::new();
    }
    let kind = segments[0];
    if kind != "vi" && kind != "vi_webp" {
        return Vec::new();
    }
    let id = segments[1];
    if id.is_empty() {
        return Vec::new();
    }
    let names = ["maxresdefault", "sddefault", "hqdefault", "mqdefault", "default", "0"];
    let exts = ["jpg", "webp"];
    let mut out = Vec::new();
    for name in names {
        for ext in exts {
            let mut candidate = parsed.clone();
            candidate.set_path(&format!("/{kind}/{id}/{name}.{ext}"));
            candidate.set_query(None);
            candidate.set_fragment(None);
            out.push(candidate.to_string());
        }
    }
    out
}

/// The thumbnail id encoded in a cache file name.
fn thumbnail_id_from_file_name(name: &str) -> Option<String> {
    let id = name
        .strip_suffix(".mid.jpg")
        .or_else(|| name.strip_suffix(".small.jpg"))
        .or_else(|| name.strip_suffix(".jpg"))?;
    ThumbnailCache::is_thumbnail_id(id).then(|| id.to_string())
}

/// Removes the three image files owned by a thumbnail id.
fn remove_thumbnail_files(directory: &Path, id: &str) {
    for path in [
        directory.join(format!("{id}.jpg")),
        directory.join(format!("{id}.mid.jpg")),
        directory.join(format!("{id}.small.jpg")),
    ] {
        let _ = fs::remove_file(path);
    }
}

/// Every thumbnail id any playlist entry still points at.
///
/// Thumbnails are shared between the active queue and every user playlist, so
/// cleanup has to look at all their tables. Missing tables are ignored, which
/// keeps the cache usable in tests that only create the cache schema.
fn referenced_thumbnail_ids(connection: &Connection) -> rusqlite::Result<HashSet<String>> {
    let mut tables = vec!["active_queue".to_string()];
    if let Ok(mut statement) = connection.prepare("SELECT table_name FROM playlist_table")
        && let Ok(rows) = statement.query_map([], |row| row.get::<_, String>(0))
    {
        tables.extend(rows.flatten().filter(|table| {
            table.starts_with("playlist_")
                && table
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '_')
        }));
    }

    let mut ids = HashSet::new();
    for table in tables {
        let sql = format!(
            "SELECT DISTINCT thumbnail FROM {table} WHERE thumbnail IS NOT NULL AND thumbnail <> ''"
        );
        match connection.prepare(&sql) {
            Ok(mut statement) => {
                let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
                ids.extend(rows.flatten().filter(|value| ThumbnailCache::is_thumbnail_id(value)));
            }
            Err(error) if error.to_string().contains("no such table") => {}
            Err(error) => return Err(error),
        }
    }
    Ok(ids)
}

/// Removes a partially downloaded thumbnail when a fetch ends, whether it
/// succeeded or failed. A successful conversion has already removed it.
struct ScratchFile<'a>(&'a Path);

impl Drop for ScratchFile<'_> {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.0);
    }
}

/// Seconds since the Unix epoch for timestamps.
fn unix_time() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().min(i64::MAX as u64) as i64)
        .unwrap_or(0)
}

/// A 64 bit FNV-1a hash, used only for scratch file names.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_valid_jpeg(path: &Path) {
        let image = image::ImageBuffer::from_fn(2, 2, |_, _| image::Rgb([255u8, 0, 0]));
        let mut bytes = Vec::new();
        image
            .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Jpeg)
            .expect("a valid JPEG encodes");
        fs::write(path, &bytes).unwrap();
    }

    fn test_cache() -> (ThumbnailCache, tempfile::TempDir) {
        let temp = tempfile::tempdir().unwrap();
        let db = Database::in_memory().expect("database opens");
        let cache = ThumbnailCache::new(db, temp.path());
        (cache, temp)
    }

    #[test]
    fn the_prefix_distinguishes_new_ids_from_legacy_paths() {
        assert!(ThumbnailCache::is_thumbnail_id("thumb_abcdef"));
        assert!(!ThumbnailCache::is_thumbnail_id("/tmp/legacy.jpg"));
        assert!(!ThumbnailCache::is_thumbnail_id("legacy.jpg"));
        assert!(!ThumbnailCache::is_thumbnail_id(""));
    }

    #[test]
    fn thumbnail_id_is_based_on_the_cleaned_video_url() {
        let first = ThumbnailCache::thumbnail_id_for(
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ&list=PLabc&t=30s",
        );
        let second =
            ThumbnailCache::thumbnail_id_for("https://www.youtube.com/watch?v=dQw4w9WgXcQ");
        assert_eq!(first, second);
        assert!(first.starts_with(THUMBNAIL_ID_PREFIX));
    }

    #[test]
    fn different_videos_get_different_ids() {
        let first = ThumbnailCache::thumbnail_id_for("https://example.com/a");
        let second = ThumbnailCache::thumbnail_id_for("https://example.com/b");
        assert_ne!(first, second);
    }

    #[test]
    fn every_playlist_keeps_its_thumbnails_referenced() {
        let db = Database::in_memory().expect("database opens");
        db.with_connection(|connection| {
            connection.execute_batch(
                "CREATE TABLE active_queue (thumbnail TEXT);
                 CREATE TABLE playlist_table (table_name TEXT);
                 CREATE TABLE playlist_abc (thumbnail TEXT);
                 INSERT INTO active_queue (thumbnail) VALUES ('thumb_queue');
                 INSERT INTO playlist_table (table_name) VALUES ('playlist_abc');
                 INSERT INTO playlist_abc (thumbnail) VALUES ('thumb_playlist');",
            )?;
            let referenced = referenced_thumbnail_ids(connection)?;
            assert!(referenced.contains("thumb_queue"));
            assert!(referenced.contains("thumb_playlist"));
            assert_eq!(referenced.len(), 2);
            Ok(())
        })
        .expect("references are read");
    }

    #[test]
    fn full_mid_and_small_files_have_distinct_names() {
        let (cache, _temp) = test_cache();
        let id = ThumbnailCache::thumbnail_id_for("https://example.com/a");
        assert_ne!(cache.full_file(&id), cache.mid_file(&id));
        assert_ne!(cache.mid_file(&id), cache.small_file(&id));
        assert_ne!(cache.full_file(&id), cache.small_file(&id));
    }

    #[test]
    fn prune_orphans_removes_files_not_in_the_cache() {
        let (cache, temp) = test_cache();
        let id = ThumbnailCache::thumbnail_id_for("https://example.com/a");
        let full = cache.full_file(&id);
        let mid = cache.mid_file(&id);
        let small = cache.small_file(&id);
        write_valid_jpeg(&full);
        write_valid_jpeg(&mid);
        write_valid_jpeg(&small);
        cache
            .db
            .with_connection(|connection| {
                connection.execute(
                    "INSERT INTO thumbnail_cache (thumbnail_id, source_url, created_at)
                     VALUES (?1, ?2, ?3)",
                    params![id, "https://example.com/a", 1],
                )?;
                Ok(())
            })
            .unwrap();
        let orphan = temp.path().join("thumb_orphan.jpg");
        write_valid_jpeg(&orphan);
        let stray = temp.path().join("stray.bin");
        fs::write(&stray, b"junk").unwrap();

        cache.prune_orphans().unwrap();

        assert!(full.is_file());
        assert!(mid.is_file());
        assert!(small.is_file());
        assert!(!orphan.is_file());
        assert!(!stray.is_file());
    }

    #[test]
    fn stale_scratch_files_are_removed_but_fresh_ones_are_kept() {
        let (cache, temp) = test_cache();
        let stale = temp.path().join("stale.webp.part");
        let fresh = temp.path().join("fresh.jpg.part");
        fs::write(&stale, b"junk").unwrap();
        fs::write(&fresh, b"junk").unwrap();
        let old = SystemTime::now() - Duration::from_secs(2 * 60 * 60);
        fs::File::options().write(true).open(&stale).unwrap().set_modified(old).unwrap();

        cache.remove_stale_partials();

        assert!(!stale.exists());
        assert!(fresh.exists());
    }

    #[test]
    fn a_scratch_file_is_removed_when_the_fetch_ends() {
        let (cache, _temp) = test_cache();
        let path = cache.directory.join("thumbnail.part");
        fs::write(&path, b"partial").unwrap();

        {
            let _scratch = ScratchFile(&path);
            assert!(path.exists());
        }

        assert!(!path.exists());
    }

    #[test]
    fn the_id_is_the_prefix_plus_the_url_hash() {
        let id = ThumbnailCache::thumbnail_id_for("https://example.com/a");
        assert!(id.starts_with(THUMBNAIL_ID_PREFIX));
        // The hash part is the 32 hex characters of an MD5 digest.
        assert_eq!(id.len(), THUMBNAIL_ID_PREFIX.len() + 32);
        assert!(id[THUMBNAIL_ID_PREFIX.len()..].chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn register_legacy_renames_a_valid_jpeg_without_reencoding() {
        let (cache, temp) = test_cache();
        let id = ThumbnailCache::thumbnail_id_for("https://example.com/a");
        let legacy = temp.path().join("legacy.jpg");
        write_valid_jpeg(&legacy);

        let new_id = cache.register_legacy("https://example.com/a", &legacy).unwrap();

        assert_eq!(new_id, id);
        // The legacy file was moved, not copied: it no longer exists at its
        // old location.
        assert!(!legacy.exists());
        // The full-size slot now holds the moved file.
        assert!(cache.full_file(&id).is_file());
        // Mid and small were derived from it.
        assert!(cache.mid_file(&id).is_file());
        assert!(cache.small_file(&id).is_file());
    }

    #[test]
    fn register_legacy_reencodes_a_non_jpeg_legacy_file() {
        let (cache, temp) = test_cache();
        let id = ThumbnailCache::thumbnail_id_for("https://example.com/b");
        let legacy = temp.path().join("legacy.png");
        // Write a PNG; it is not a renderable JPEG, so the normalizer path is
        // used instead of a plain rename.
        let image = image::ImageBuffer::from_fn(2, 2, |_, _| image::Rgb([255u8, 0, 0]));
        let mut bytes = Vec::new();
        image
            .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
            .expect("a valid PNG encodes");
        fs::write(&legacy, &bytes).unwrap();

        let new_id = cache.register_legacy("https://example.com/b", &legacy).unwrap();

        assert_eq!(new_id, id);
        // The legacy file was consumed by the normalizer.
        assert!(!legacy.exists());
        assert!(cache.full_file(&id).is_file());
        assert!(cache.mid_file(&id).is_file());
        assert!(cache.small_file(&id).is_file());
        // The full-size result is a renderable JPEG.
        assert!(is_renderable_jpeg(&cache.full_file(&id)));
    }

    #[test]
    fn register_legacy_does_not_redownload_when_id_already_exists() {
        let (cache, temp) = test_cache();
        let id = ThumbnailCache::thumbnail_id_for("https://example.com/c");
        // Manually insert a row so the id already exists.
        cache
            .db
            .with_connection(|connection| {
                connection.execute(
                    "INSERT INTO thumbnail_cache (thumbnail_id, source_url, created_at)
                     VALUES (?1, ?2, ?3)",
                    params![id, "https://example.com/c", 1],
                )?;
                Ok(())
            })
            .unwrap();

        let legacy = temp.path().join("legacy.jpg");
        write_valid_jpeg(&legacy);

        let new_id = cache.register_legacy("https://example.com/c", &legacy).unwrap();

        assert_eq!(new_id, id);
        // The legacy file is left untouched because the id already exists.
        assert!(legacy.exists());
    }

    #[test]
    fn cached_paths_are_derived_from_current_directory() {
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("app.db");
        let db = Database::open(&db_path).expect("database opens");
        let old_dir = temp.path().join("old-cache");
        fs::create_dir_all(&old_dir).unwrap();
        let cache = ThumbnailCache::new(db.clone(), &old_dir);
        let id = ThumbnailCache::thumbnail_id_for("https://example.com/moved");
        write_valid_jpeg(&cache.full_file(&id));
        write_valid_jpeg(&cache.mid_file(&id));
        write_valid_jpeg(&cache.small_file(&id));
        db.with_connection(|connection| {
            connection.execute(
                "INSERT INTO thumbnail_cache (thumbnail_id, source_url, created_at) VALUES (?1, ?2, ?3)",
                params![id, "https://example.com/moved.jpg", 1],
            )?;
            Ok(())
        })
        .unwrap();

        let moved_dir = temp.path().join("moved-cache");
        fs::create_dir_all(&moved_dir).unwrap();
        for suffix in [".jpg", ".mid.jpg", ".small.jpg"] {
            fs::rename(
                old_dir.join(format!("{id}{suffix}")),
                moved_dir.join(format!("{id}{suffix}")),
            )
            .unwrap();
        }
        let moved_cache = ThumbnailCache::new(db, &moved_dir);

        assert_eq!(moved_cache.full_path(&id), Some(moved_dir.join(format!("{id}.jpg"))));
        assert_eq!(moved_cache.mid_path(&id), Some(moved_dir.join(format!("{id}.mid.jpg"))));
        assert_eq!(moved_cache.small_path(&id), Some(moved_dir.join(format!("{id}.small.jpg"))));
    }

    #[test]
    fn legacy_absolute_schema_is_migrated_without_redownload() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("portable");
        let cache_dir = root.join("cache").join("thumbnails");
        fs::create_dir_all(&cache_dir).unwrap();
        let db_path = root.join("app.db");
        let db = Database::open(&db_path).expect("database opens");
        let id = ThumbnailCache::thumbnail_id_for("https://example.com/legacy");
        let old_full = temp.path().join("old-full.jpg");
        let old_mid = temp.path().join("old-mid.jpg");
        let old_small = temp.path().join("old-small.jpg");
        write_valid_jpeg(&old_full);
        write_valid_jpeg(&old_mid);
        write_valid_jpeg(&old_small);
        db.with_connection(|connection| {
            connection.execute_batch(
                "CREATE TABLE thumbnail_cache (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    thumbnail_id TEXT NOT NULL UNIQUE,
                    source_url TEXT,
                    full_path TEXT NOT NULL,
                    mid_path TEXT NOT NULL,
                    small_path TEXT NOT NULL,
                    created_at INTEGER NOT NULL
                );",
            )?;
            connection.execute(
                "INSERT INTO thumbnail_cache (thumbnail_id, source_url, full_path, mid_path, small_path, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![id, old_full.to_string_lossy(), old_full.to_string_lossy(), old_mid.to_string_lossy(), old_small.to_string_lossy(), 1],
            )?;
            Ok(())
        })
        .unwrap();

        let cache = ThumbnailCache::new(db.clone(), &cache_dir);

        assert_eq!(cache.full_path(&id), Some(cache_dir.join(format!("{id}.jpg"))));
        assert!(cache_dir.join(format!("{id}.mid.jpg")).is_file());
        assert!(cache_dir.join(format!("{id}.small.jpg")).is_file());
        let source_url: Option<String> = db
            .with_connection(|connection| {
                connection.query_row(
                    "SELECT source_url FROM thumbnail_cache WHERE thumbnail_id = ?1",
                    params![id],
                    |row| row.get(0),
                )
            })
            .unwrap();
        assert_eq!(source_url, None);
        let columns: Vec<String> = db
            .with_connection(|connection| {
                let mut statement = connection.prepare("PRAGMA table_info(thumbnail_cache)")?;
                let rows = statement.query_map([], |row| row.get::<_, String>(1))?;
                rows.collect::<Result<Vec<_>, _>>()
            })
            .unwrap();
        assert!(!columns.iter().any(|column| column == "full_path"));
        assert!(!columns.iter().any(|column| column == "mid_path"));
        assert!(!columns.iter().any(|column| column == "small_path"));
    }
}
