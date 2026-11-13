//! The thumbnail cache.
//!
//! Flatline shows a preview of a video before the user decides what to do
//! with it. The image is downloaded once and kept in a cache directory
//! beside the program, keyed by the URL it came from, so the same video is
//! never fetched twice.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::Error;
use image_process::{jpeg_path_for, normalize_to_jpeg};

const USER_AGENT: &str = concat!("Flatline/", env!("CARGO_PKG_VERSION"));

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
    /// The cached thumbnail is always normalized to JPEG so the UI can render it.
    pub fn fetch(&self, url: &str) -> Result<PathBuf, Error> {
        let path = self.path_for(url);
        if path.is_file() {
            return Ok(path);
        }

        fs::create_dir_all(&self.directory)?;
        let temporary = temporary_path(&path);
        {
            let mut response = ureq::get(url)
                .header("User-Agent", USER_AGENT)
                .call()
                .map_err(|error| Error::Http(error.to_string()))?;
            let mut file = fs::File::create(&temporary)?;
            let mut reader = response.body_mut().as_reader();
            io::copy(&mut reader, &mut file)?;
            file.sync_all()?;
        }

        if let Err(error) = fs::rename(&temporary, &path) {
            let _ = fs::remove_file(&temporary);
            return Err(Error::Io(error));
        }

        // Normalize to JPEG (converts if needed, cleans up original)
        let jpeg_path = jpeg_path_for(&path);
        normalize_to_jpeg(&path, &jpeg_path)?;

        Ok(jpeg_path)
    }

    /// The path a thumbnail is cached at (always .jpg).
    pub fn path_for(&self, url: &str) -> PathBuf {
        self.directory.join(file_name_for(url))
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
            if thumbnail != Some(path.as_path()) && entry.file_type()?.is_file() {
                fs::remove_file(path)?;
            }
        }
        Ok(())
    }
}

/// The file name a thumbnail URL is cached under.
///
/// The name is a hash of the URL, so it never carries a character the file
/// system rejects and two different URLs never collide in practice.
/// The extension is always .jpg since thumbnails are normalized to JPEG.
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

/// Path of the scratch file used while downloading.
fn temporary_path(destination: &Path) -> PathBuf {
    let mut name = destination.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    destination.with_file_name(name)
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
}
