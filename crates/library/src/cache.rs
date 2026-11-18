//! The thumbnail cache.
//!
//! Flatline keeps three sized copies of every thumbnail (full, mid and
//! small) in a `thumbnail_cache` table beside the active queue, so the same
//! video is never fetched twice and each screen draws the size it actually
//! needs.
//!
//! A thumbnail is addressed by a stable id made of a prefix and the hash of
//! the cleaned video URL (`thumb_<md5>`), so a video that appears in the
//! queue twice reuses one thumbnail instead of being downloaded twice. The
//! prefix is what tells a new-style id apart from an old-style direct file
//! path.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use database::Database;
use media_model::video_id_hash;
use rusqlite::{OptionalExtension, params};

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

/// A thumbnail is a preview, not the media: it must never keep a resolution
/// waiting on a stalled server.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(15);

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

    /// Ensures the `thumbnail_cache` table exists.
    fn ensure_schema(&self) -> Result<(), Error> {
        self.db
            .with_connection(|connection| {
                connection.execute_batch(
                    "CREATE TABLE IF NOT EXISTS thumbnail_cache (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    thumbnail_id TEXT NOT NULL UNIQUE,
                    source_url TEXT,
                    full_path TEXT NOT NULL,
                    mid_path TEXT NOT NULL,
                    small_path TEXT NOT NULL,
                    created_at INTEGER NOT NULL
                );
                CREATE INDEX IF NOT EXISTS thumbnail_cache_thumbnail_id
                    ON thumbnail_cache(thumbnail_id);",
                )?;
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
        let path = match self.db.with_connection(|connection| {
            connection.query_row(
                "SELECT full_path FROM thumbnail_cache WHERE thumbnail_id = ?1",
                params![id],
                |row| row.get::<_, String>(0).map(PathBuf::from),
            )
        }) {
            Ok(path) => path,
            _ => return None,
        };
        (path.is_file() && is_renderable_jpeg(&path)).then_some(path)
    }

    /// Returns the mid-size thumbnail for `id`, when it is on disk and
    /// readable.
    pub fn mid_path(&self, id: &str) -> Option<PathBuf> {
        let path = match self.db.with_connection(|connection| {
            connection.query_row(
                "SELECT mid_path FROM thumbnail_cache WHERE thumbnail_id = ?1",
                params![id],
                |row| row.get::<_, String>(0).map(PathBuf::from),
            )
        }) {
            Ok(path) => path,
            _ => return None,
        };
        (path.is_file() && is_renderable_jpeg(&path)).then_some(path)
    }

    /// Returns the small thumbnail for `id`, when it is on disk and readable.
    pub fn small_path(&self, id: &str) -> Option<PathBuf> {
        let path = match self.db.with_connection(|connection| {
            connection.query_row(
                "SELECT small_path FROM thumbnail_cache WHERE thumbnail_id = ?1",
                params![id],
                |row| row.get::<_, String>(0).map(PathBuf::from),
            )
        }) {
            Ok(path) => path,
            _ => return None,
        };
        (path.is_file() && is_renderable_jpeg(&path)).then_some(path)
    }

    /// Returns the cached thumbnail for `video_url`, downloading it from
    /// `thumbnail_url` when needed.
    ///
    /// The same video always yields the same id, so a queue that lists the
    /// video twice downloads the thumbnail only once. When a thumbnail is
    /// already cached the network is never touched. Returns the thumbnail id
    /// the active queue should store.
    pub fn fetch(&self, video_url: &str, thumbnail_url: Option<&str>) -> Result<String, Error> {
        let id = ThumbnailCache::thumbnail_id_for(video_url);
        if self.thumbnail_exists(&id)? {
            return Ok(id);
        }
        let Some(thumbnail_url) = thumbnail_url else {
            return Ok(id);
        };

        let full_path = self.full_file(&id);
        let mid_path = self.mid_file(&id);
        let small_path = self.small_file(&id);

        fs::create_dir_all(&self.directory)?;
        self.remove_stale_partials();

        // The download is format-agnostic: the scratch name has no meaning,
        // because the normalizer guesses the real format from the bytes.
        let temp_name = format!("{:016x}.part", fnv1a(thumbnail_url.as_bytes()));
        let temporary = self.directory.join(temp_name);
        let _scratch = ScratchFile(&temporary);

        {
            let mut response = ureq::get(thumbnail_url)
                .header("User-Agent", USER_AGENT)
                .config()
                .timeout_global(Some(DOWNLOAD_TIMEOUT))
                .build()
                .call()
                .map_err(|error| Error::Http(error.to_string()))?;
            let mut file = fs::File::create(&temporary)?;
            let mut reader = response.body_mut().as_reader();
            io::copy(&mut reader, &mut file)?;
            file.sync_all()?;
        }

        // Normalize to full-size JPEG, then derive the mid and small copies
        // by fitting each inside its target dimensions without distorting the
        // aspect ratio. The source download is removed by the normalizer.
        normalize_to_jpeg(&temporary, &full_path)?;
        write_scaled_jpeg(&full_path, &mid_path, MID_MAX_WIDTH, MID_MAX_HEIGHT)
            .map_err(Error::from)?;
        write_scaled_jpeg(&full_path, &small_path, SMALL_MAX_WIDTH, SMALL_MAX_HEIGHT)
            .map_err(Error::from)?;

        let now = unix_time();
        self.db.with_connection(|connection| {
            connection.execute(
                "INSERT INTO thumbnail_cache (thumbnail_id, source_url, full_path, mid_path, small_path, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    id,
                    thumbnail_url,
                    full_path.to_string_lossy(),
                    mid_path.to_string_lossy(),
                    small_path.to_string_lossy(),
                    now,
                ],
            )?;
            Ok(())
        }).map_err(Error::from)?;

        Ok(id)
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
        self.db.with_connection(|connection| {
            connection.execute(
                "INSERT INTO thumbnail_cache (thumbnail_id, source_url, full_path, mid_path, small_path, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    id,
                    legacy_path.to_string_lossy(),
                    full_path.to_string_lossy(),
                    mid_path.to_string_lossy(),
                    small_path.to_string_lossy(),
                    now,
                ],
            )?;
            Ok(())
        }).map_err(Error::from)?;

        Ok(id)
    }

    /// Deletes the thumbnail record `id` and its three image files, but only
    /// when no entry in the active queue still points at it.
    pub fn release_thumbnail(&self, id: &str) -> Result<(), Error> {
        let referenced = self
            .db
            .with_connection(|connection| {
                let count: i64 = connection.query_row(
                    "SELECT COUNT(*) FROM active_queue WHERE thumbnail = ?1",
                    params![id],
                    |row| row.get(0),
                )?;
                Ok(count)
            })
            .map_err(Error::from)?;

        if referenced == 0 {
            self.delete_thumbnail(id)?;
        }
        Ok(())
    }

    /// Deletes every thumbnail record and image file that no entry in the
    /// active queue points at.
    pub fn release_unreferenced_thumbnails(&self) -> Result<(), Error> {
        self.db
            .with_connection(|connection| {
                // The ids still referenced by the queue.
                let mut statement = connection.prepare(
                    "SELECT DISTINCT thumbnail FROM active_queue
                 WHERE thumbnail IS NOT NULL AND thumbnail <> ''",
                )?;
                let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
                let referenced: HashSet<String> = rows.filter_map(Result::ok).collect();

                // Every record in the cache.
                let mut statement = connection.prepare(
                    "SELECT thumbnail_id, full_path, mid_path, small_path FROM thumbnail_cache",
                )?;
                let rows = statement.query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                })?;
                for row in rows.filter_map(Result::ok) {
                    let (id, full, mid, small) = row;
                    if !referenced.contains(&id) {
                        let _ = connection.execute(
                            "DELETE FROM thumbnail_cache WHERE thumbnail_id = ?1",
                            params![id],
                        );
                        for path in [&full, &mid, &small] {
                            let _ = fs::remove_file(Path::new(path));
                        }
                    }
                }
                Ok(())
            })
            .map_err(Error::from)
    }

    /// Deletes the thumbnail record `id` and its three image files.
    fn delete_thumbnail(&self, id: &str) -> Result<(), Error> {
        let paths = self.db.with_connection(|connection| {
            connection
                .query_row(
                    "SELECT full_path, mid_path, small_path FROM thumbnail_cache WHERE thumbnail_id = ?1",
                    params![id],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                        ))
                    },
                )
                .optional()
        }).map_err(Error::from)?;

        let _ = self
            .db
            .with_connection(|connection| {
                connection
                    .execute("DELETE FROM thumbnail_cache WHERE thumbnail_id = ?1", params![id])
            })
            .map_err(Error::from)?;

        if let Some((full, mid, small)) = paths {
            for path in [&full, &mid, &small] {
                let _ = fs::remove_file(Path::new(path));
            }
        }
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
        let (referenced, protected_ids) = self
            .db
            .with_connection(|connection| {
                let mut statement = connection
                    .prepare("SELECT full_path, mid_path, small_path FROM thumbnail_cache")?;
                let rows = statement.query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })?;
                let mut names: HashSet<String> = HashSet::new();
                for row in rows.filter_map(Result::ok) {
                    for path in [&row.0, &row.1, &row.2] {
                        if let Some(name) = Path::new(path).file_name().and_then(|n| n.to_str()) {
                            names.insert(name.to_string());
                        }
                    }
                }

                let mut protected = HashSet::new();
                let mut statement = connection.prepare(
                    "SELECT DISTINCT thumbnail FROM active_queue
                     WHERE thumbnail IS NOT NULL AND thumbnail <> ''",
                )?;
                let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
                for id in rows.flatten() {
                    if id.starts_with(THUMBNAIL_ID_PREFIX) {
                        protected.insert(id);
                    }
                }
                Ok((names, protected))
            })
            .map_err(Error::from)?;

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
            if !referenced.contains(name) {
                let id = name
                    .strip_suffix(".mid.jpg")
                    .or_else(|| name.strip_suffix(".small.jpg"))
                    .or_else(|| name.strip_suffix(".jpg"))
                    .unwrap_or(name);
                if protected_ids.contains(id) {
                    continue;
                }
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
                    "INSERT OR IGNORE INTO thumbnail_cache (thumbnail_id, source_url, full_path, mid_path, small_path, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        id,
                        Option::<String>::None,
                        full.to_string_lossy(),
                        mid.to_string_lossy(),
                        small.to_string_lossy(),
                        now,
                    ],
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
                    "INSERT INTO thumbnail_cache (thumbnail_id, source_url, full_path, mid_path, small_path, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![id, "https://example.com/a", full.to_string_lossy(), mid.to_string_lossy(), small.to_string_lossy(), 1],
                )?;
                Ok(())
            })
            .unwrap();
        let orphan = temp.path().join("orphan.jpg");
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
                    "INSERT INTO thumbnail_cache (thumbnail_id, source_url, full_path, mid_path, small_path, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![id, "https://example.com/c", "/tmp/full.jpg", "/tmp/mid.jpg", "/tmp/small.jpg", 1],
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
}
