//! The thumbnail cache.
//!
//! Flatline shows a preview of a video before the user decides what to do
//! with it. The image is downloaded once and kept in a cache directory
//! beside the program, keyed by the URL it came from, so the same video is
//! never fetched twice.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::Error;
use image_process::{is_renderable_jpeg, normalize_to_jpeg, write_scaled_jpeg};

const USER_AGENT: &str = concat!("Flatline/", env!("CARGO_PKG_VERSION"));

/// The largest preview the queue list needs; the row itself only draws it at
/// 96 by 54 logical pixels, but a little headroom keeps it sharp on HiDPI.
const PREVIEW_MAX_WIDTH: u32 = 320;
const PREVIEW_MAX_HEIGHT: u32 = 180;

/// A thumbnail is a preview, not the media: it must never keep a resolution
/// waiting on a stalled server.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(15);

/// Scratch files older than this were left behind by an interrupted
/// download; no healthy fetch keeps one around that long.
const STALE_PARTIAL_AGE: Duration = Duration::from_secs(60 * 60);

/// The directory downloaded thumbnails are kept in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThumbnailCache {
    directory: PathBuf,
}

impl ThumbnailCache {
    /// Creates a cache that writes into `directory`.
    pub fn new(directory: impl Into<PathBuf>) -> ThumbnailCache {
        ThumbnailCache { directory: directory.into() }
    }

    /// The directory the cached images live in.
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Returns the cached file for a thumbnail, downloading it when needed.
    ///
    /// A thumbnail that is already cached is returned as it is, so a repeated
    /// request never hits the network again. The download lands in a scratch
    /// file first and is moved into place afterwards, so an interrupted
    /// download never leaves a broken image behind.
    ///
    /// The cached thumbnail is always normalized to JPEG so the UI can render
    /// it. A cached file whose header cannot be read is downloaded again.
    pub fn fetch(&self, url: &str) -> Result<PathBuf, Error> {
        let jpeg_path = self.path_for(url);
        if jpeg_path.is_file() {
            if is_renderable_jpeg(&jpeg_path) {
                return Ok(jpeg_path);
            }
            match fs::remove_file(&jpeg_path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(Error::Io(error)),
            }
        }

        fs::create_dir_all(&self.directory)?;
        self.remove_stale_partials();

        // The download is format-agnostic: the scratch name has no meaning,
        // because the normalizer guesses the real format from the bytes.
        let temp_name = format!("{:016x}.part", fnv1a(url.as_bytes()));
        let temporary = self.directory.join(temp_name);
        let _scratch = ScratchFile(&temporary);

        {
            let mut response = ureq::get(url)
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

        // Normalize to JPEG (converts if needed, cleans up original temp file)
        normalize_to_jpeg(&temporary, &jpeg_path)?;
        // A preview is what the queue list draws; making it now keeps the
        // queue dialog from re-encoding every cached full-size image later.
        let _ = self.write_preview(&jpeg_path);

        Ok(jpeg_path)
    }

    /// The path a thumbnail is cached at (always .jpg).
    pub fn path_for(&self, url: &str) -> PathBuf {
        self.directory.join(file_name_for(url))
    }

    /// Returns the small copy of a cached thumbnail, making it when needed.
    ///
    /// The full-size image is left untouched, so screens that want a larger
    /// preview keep using it. Callers that only draw a list should use this
    /// path: it decodes much faster and keeps far less pixel memory alive.
    pub fn preview(&self, source: &Path) -> Result<Option<PathBuf>, Error> {
        if !source.is_file() {
            return Ok(None);
        }
        let preview = self.preview_path_for(source);
        if preview.is_file() && is_renderable_jpeg(&preview) {
            return Ok(Some(preview));
        }
        self.write_preview(source)?;
        Ok(Some(preview))
    }

    /// Returns an already prepared preview without doing any image work.
    ///
    /// The queue list draws on the event loop, so it must never decode a
    /// full-size thumbnail there. A missing preview simply means the
    /// background warm-up has not reached that file yet.
    pub fn preview_if_present(&self, source: &Path) -> Option<PathBuf> {
        let preview = self.preview_path_for(source);
        (preview.is_file() && is_renderable_jpeg(&preview)).then_some(preview)
    }

    /// Writes the bounded preview next to a cached thumbnail.
    fn write_preview(&self, source: &Path) -> Result<(), Error> {
        write_scaled_jpeg(
            source,
            &self.preview_path_for(source),
            PREVIEW_MAX_WIDTH,
            PREVIEW_MAX_HEIGHT,
        )
        .map_err(Error::from)
    }

    /// The path a preview is cached at, derived from the full-size name.
    fn preview_path_for(&self, source: &Path) -> PathBuf {
        let stem = source.file_stem().and_then(|stem| stem.to_str()).unwrap_or("thumbnail");
        self.directory.join(format!("{stem}.preview.jpg"))
    }

    /// Removes cached thumbnails when the active queue is replaced.
    pub fn clear(&self) -> Result<(), Error> {
        match fs::remove_dir_all(&self.directory) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(Error::Io(error)),
        }
    }

    /// Removes cached thumbnails other than the one being put in the new queue.
    pub fn retain(&self, thumbnail: Option<&Path>) -> Result<(), Error> {
        let entries = match fs::read_dir(&self.directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(Error::Io(error)),
        };
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            let preview = thumbnail.map(|thumbnail| self.preview_path_for(thumbnail));
            if thumbnail != Some(path.as_path())
                && preview.as_deref() != Some(path.as_path())
                && entry.file_type()?.is_file()
            {
                fs::remove_file(path)?;
            }
        }
        Ok(())
    }

    /// Removes cached thumbnails that are no longer referenced, plus any
    /// file that is not a healthy JPEG.
    ///
    /// The caller's job is to pass every thumbnail URL that is still in use,
    /// most conveniently the ones from the active queue and the history.
    /// Files that miss from that set, or that a fetch left behind in a bad
    /// state, are deleted, so the cache does not grow with dead weight.
    pub fn prune(&self, keep: &[String]) -> Result<(), Error> {
        let entries = match fs::read_dir(&self.directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(Error::Io(error)),
        };
        let mut keep_names: HashSet<String> = HashSet::new();
        for url in keep {
            let name = file_name_for(url);
            if let Some(stem) = Path::new(&name).file_stem().and_then(|stem| stem.to_str()) {
                keep_names.insert(format!("{stem}.preview.jpg"));
            }
            keep_names.insert(name);
        }
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let name = match path.file_name().and_then(|n| n.to_str()) {
                Some(name) => name,
                None => continue,
            };
            if !name.ends_with(".jpg") || !keep_names.contains(name) || !is_renderable_jpeg(&path) {
                let _ = fs::remove_file(&path);
            }
        }
        Ok(())
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

/// The file name a thumbnail URL is cached under (always .jpg).
pub fn file_name_for(url: &str) -> String {
    format!("{:016x}.jpg", fnv1a(url.as_bytes()))
}

/// A 64 bit FNV-1a hash, used only for cache file names.
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

    #[test]
    fn the_file_name_always_ends_with_jpg() {
        assert!(file_name_for("https://example.com/image.jpg").ends_with(".jpg"));
        assert!(file_name_for("https://example.com/image.png").ends_with(".jpg"));
        assert!(file_name_for("https://example.com/image.webp").ends_with(".jpg"));
        assert!(file_name_for("https://example.com/image").ends_with(".jpg"));
    }

    #[test]
    fn different_urls_get_different_names() {
        let first = file_name_for("https://example.com/a.jpg");
        let second = file_name_for("https://example.com/b.jpg");
        assert_ne!(first, second);
    }

    #[test]
    fn the_same_url_gets_the_same_name() {
        let url = "https://example.com/a.jpg?x=1";
        assert_eq!(file_name_for(url), file_name_for(url));
    }

    #[test]
    fn the_path_lives_in_the_cache_directory() {
        let cache = ThumbnailCache::new("/tmp/flatline-cache");
        let path = cache.path_for("https://example.com/a.jpg");
        assert_eq!(path.parent(), Some(Path::new("/tmp/flatline-cache")));
    }

    #[test]
    fn stale_scratch_files_are_removed_but_fresh_ones_are_kept() {
        let temp = tempfile::tempdir().unwrap();
        let cache = ThumbnailCache::new(temp.path());
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
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("thumbnail.part");
        fs::write(&path, b"partial").unwrap();

        {
            let _scratch = ScratchFile(&path);
            assert!(path.exists());
        }

        assert!(!path.exists());
    }

    #[test]
    fn a_preview_is_written_beside_the_full_thumbnail() {
        let temp = tempfile::tempdir().unwrap();
        let cache = ThumbnailCache::new(temp.path());
        let full = cache.path_for("https://example.com/a.jpg");
        write_valid_jpeg(&full);

        let preview = cache.preview(&full).expect("preview works").expect("preview exists");

        assert!(full.is_file());
        assert!(preview.is_file());
        let expected =
            format!("{}.preview.jpg", full.file_stem().and_then(|stem| stem.to_str()).unwrap());
        assert_eq!(preview.file_name().and_then(|name| name.to_str()), Some(expected.as_str()));
    }

    #[test]
    fn a_missing_preview_is_not_written_while_reading_the_present_one() {
        let temp = tempfile::tempdir().unwrap();
        let cache = ThumbnailCache::new(temp.path());
        let full = cache.path_for("https://example.com/a.jpg");
        write_valid_jpeg(&full);

        assert!(cache.preview_if_present(&full).is_none());
        let expected = cache.preview(&full).expect("preview works").expect("preview exists");
        assert_eq!(cache.preview_if_present(&full), Some(expected));
    }

    #[test]
    fn prune_keeps_the_preview_of_a_known_url() {
        let temp = tempfile::tempdir().unwrap();
        let cache = ThumbnailCache::new(temp.path());
        let keep = "https://example.com/a.jpg";
        let full = cache.path_for(keep);
        write_valid_jpeg(&full);
        let preview = cache.preview(&full).expect("preview works").expect("preview exists");

        cache.prune(&[keep.to_string()]).unwrap();

        assert!(full.is_file());
        assert!(preview.is_file());
    }

    #[test]
    fn prune_removes_orphaned_files_and_keeps_known_urls() {
        let temp = tempfile::tempdir().unwrap();
        let cache = ThumbnailCache::new(temp.path());
        let keep = "https://example.com/a.jpg";
        let remove = "https://example.com/b.jpg";
        write_valid_jpeg(&cache.path_for(keep));
        write_valid_jpeg(&cache.path_for(remove));

        cache.prune(&[keep.to_string()]).unwrap();

        assert!(cache.path_for(keep).is_file());
        assert!(!cache.path_for(remove).is_file());
    }

    #[test]
    fn prune_removes_non_jpeg_files() {
        let temp = tempfile::tempdir().unwrap();
        let cache = ThumbnailCache::new(temp.path());
        let jpg = cache.path_for("https://example.com/a.jpg");
        write_valid_jpeg(&jpg);
        let exe = temp.path().join("malware.exe");
        fs::write(&exe, b"bad").unwrap();
        let part = temp.path().join("scratch.part");
        fs::write(&part, b"partial").unwrap();

        cache.prune(&["https://example.com/a.jpg".to_string()]).unwrap();

        assert!(jpg.is_file());
        assert!(!exe.is_file());
        assert!(!part.is_file());
    }

    fn write_valid_jpeg(path: &Path) {
        let image = image::ImageBuffer::from_fn(2, 2, |_, _| image::Rgb([255u8, 0, 0]));
        let mut bytes = Vec::new();
        image
            .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Jpeg)
            .expect("a valid JPEG encodes");
        fs::write(path, &bytes).unwrap();
    }
}
