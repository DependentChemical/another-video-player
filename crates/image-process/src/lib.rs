//! Converts downloaded thumbnails to JPEG so the UI can render them.
//!
//! # Layer 1 of the isolation ladder
//!
//! This crate owns one capability: *normalizing image formats to JPEG*. It
//! knows nothing about where thumbnails come from, how they are cached, or
//! how they are displayed. It only knows how to read an image file and write
//! it as JPEG.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use thiserror::Error;

/// Errors that can occur during image processing.
#[derive(Debug, Error)]
pub enum Error {
    /// The source image could not be read.
    #[error("could not read image: {0}")]
    Read(#[from] image::ImageError),
    /// The converted image could not be encoded.
    #[error("could not encode image: {0}")]
    Encode(image::ImageError),
    /// The destination file could not be written.
    #[error("could not write image: {0}")]
    Write(#[from] std::io::Error),
    /// The source file does not exist.
    #[error("source file does not exist: {0}")]
    NotFound(PathBuf),
    /// The converted image is not a readable JPEG.
    #[error("the converted image is not a readable JPEG: {0}")]
    NotRenderable(PathBuf),
}

/// Converts an image file to JPEG, writing the result to `destination`.
///
/// The source extension is never trusted: the format is guessed from the
/// bytes, so a download named `thumbnail.webp.part` is still handled. The
/// image is always decoded and re-encoded, which also rejects truncated or
/// corrupt downloads instead of caching them.
///
/// The destination is written through a scratch file and moved into place
/// atomically, so a reader never sees a half-written image. The source file
/// is removed after a successful conversion.
pub fn normalize_to_jpeg(source: &Path, destination: &Path) -> Result<(), Error> {
    if !source.is_file() {
        return Err(Error::NotFound(source.to_path_buf()));
    }

    let image = decode(source)?;
    write_jpeg(&image, destination)?;

    std::fs::remove_file(source)?;
    Ok(())
}

/// Converts an image file to a bounded JPEG, writing the result to
/// `destination` without deleting the source.
///
/// This is what the queue preview uses: the full-size cache stays in place
/// for screens that want it, while a small copy is cheap to decode every time
/// a long list is drawn. Images that already fit are left at their size.
pub fn write_scaled_jpeg(
    source: &Path,
    destination: &Path,
    max_width: u32,
    max_height: u32,
) -> Result<(), Error> {
    if !source.is_file() {
        return Err(Error::NotFound(source.to_path_buf()));
    }

    let image = decode(source)?;
    let scaled = if image.width() > max_width || image.height() > max_height {
        image.thumbnail(max_width, max_height)
    } else {
        image
    };
    write_jpeg(&scaled, destination)
}

/// Returns whether `path` is a JPEG whose header can be decoded.
///
/// The cache uses this to discard a file that was left behind by an older
/// build before it hands the thumbnail to the UI.
pub fn is_renderable_jpeg(path: &Path) -> bool {
    let Ok(reader) = image::ImageReader::open(path) else {
        return false;
    };
    let Ok(reader) = reader.with_guessed_format() else {
        return false;
    };
    if reader.format() != Some(image::ImageFormat::Jpeg) {
        return false;
    }
    matches!(reader.into_dimensions(), Ok((width, height)) if width > 0 && height > 0)
}

/// Returns the path where the normalized JPEG should be stored.
///
/// Given a source path like `/cache/abc123.webp`, returns `/cache/abc123.jpg`.
pub fn jpeg_path_for(source: &Path) -> PathBuf {
    source.with_extension("jpg")
}

/// Decodes an image by looking at its bytes, not its file name.
fn decode(source: &Path) -> Result<image::DynamicImage, Error> {
    let reader = image::ImageReader::open(source)
        .map_err(|error| Error::Read(image::ImageError::IoError(error)))?
        .with_guessed_format()
        .map_err(|error| Error::Read(image::ImageError::IoError(error)))?;
    Ok(reader.decode()?)
}

/// Encodes `image` as JPEG next to `destination` and moves it into place.
fn write_jpeg(image: &image::DynamicImage, destination: &Path) -> Result<(), Error> {
    let temporary = scratch_path_for(destination);
    image.save_with_format(&temporary, image::ImageFormat::Jpeg).map_err(Error::Encode)?;

    if !is_renderable_jpeg(&temporary) {
        let _ = std::fs::remove_file(&temporary);
        return Err(Error::NotRenderable(destination.to_path_buf()));
    }

    if destination.exists() {
        std::fs::remove_file(destination)?;
    }
    if let Err(error) = std::fs::rename(&temporary, destination) {
        let _ = std::fs::remove_file(&temporary);
        return Err(Error::Write(error));
    }
    Ok(())
}

/// A per-process scratch name beside `destination`, so concurrent
/// conversions cannot write the same intermediate file.
fn scratch_path_for(destination: &Path) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);

    let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
    let name = destination.file_name().and_then(|name| name.to_str()).unwrap_or("thumbnail");
    destination.with_file_name(format!(".{name}.{}.{sequence}.tmp", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn red_png() -> Vec<u8> {
        let image = image::ImageBuffer::from_fn(2, 2, |_, _| image::Rgb([255u8, 0, 0]));
        let mut bytes = Vec::new();
        image
            .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
            .expect("the test image encodes");
        bytes
    }

    fn red_webp() -> Vec<u8> {
        let image = image::DynamicImage::ImageRgb8(image::ImageBuffer::from_fn(2, 2, |_, _| {
            image::Rgb([255u8, 0, 0])
        }));
        let mut bytes = Vec::new();
        image
            .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::WebP)
            .expect("the test image encodes");
        bytes
    }

    #[test]
    fn converts_png_to_jpeg() {
        let temp = tempfile::tempdir().unwrap();
        let png_path = temp.path().join("test.png");
        let jpg_path = temp.path().join("test.jpg");
        fs::write(&png_path, red_png()).unwrap();

        normalize_to_jpeg(&png_path, &jpg_path).unwrap();

        assert!(jpg_path.exists());
        assert!(!png_path.exists());
        assert!(is_renderable_jpeg(&jpg_path));

        let image = image::open(&jpg_path).unwrap();
        assert_eq!(image.width(), 2);
        assert_eq!(image.height(), 2);
    }

    #[test]
    fn converts_images_with_unknown_extensions() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("thumbnail.webp.part");
        let destination = temp.path().join("thumbnail.jpg");
        fs::write(&source, red_png()).unwrap();

        normalize_to_jpeg(&source, &destination).unwrap();

        assert!(destination.exists());
        assert!(!source.exists());
        assert!(is_renderable_jpeg(&destination));
    }

    #[test]
    fn converts_webp_downloads() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("thumbnail.webp.part");
        let destination = temp.path().join("thumbnail.jpg");
        fs::write(&source, red_webp()).unwrap();

        normalize_to_jpeg(&source, &destination).unwrap();

        assert!(is_renderable_jpeg(&destination));
        assert!(!source.exists());
    }

    #[test]
    fn rejects_corrupt_downloads_without_writing_a_thumbnail() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("thumbnail.jpg.part");
        let destination = temp.path().join("thumbnail.jpg");
        fs::write(&source, b"not an image").unwrap();

        assert!(normalize_to_jpeg(&source, &destination).is_err());
        assert!(!destination.exists());
    }

    #[test]
    fn replaces_a_stale_thumbnail() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("thumbnail.png");
        let destination = temp.path().join("thumbnail.jpg");
        fs::write(&source, red_png()).unwrap();
        fs::write(&destination, b"stale").unwrap();

        normalize_to_jpeg(&source, &destination).unwrap();

        assert!(is_renderable_jpeg(&destination));
    }

    #[test]
    fn scales_a_large_image_to_fit_and_keeps_the_source() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("thumbnail.png");
        let destination = temp.path().join("preview.jpg");
        let image = image::ImageBuffer::from_fn(8, 4, |_, _| image::Rgb([255u8, 0, 0]));
        image.save(&source).unwrap();

        write_scaled_jpeg(&source, &destination, 4, 4).unwrap();

        assert!(source.is_file());
        let preview = image::open(&destination).unwrap();
        assert!(preview.width() <= 4);
        assert!(preview.height() <= 4);
        assert_eq!(preview.width(), 4);
        assert_eq!(preview.height(), 2);
    }

    #[test]
    fn a_small_image_stays_at_its_size() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("thumbnail.png");
        let destination = temp.path().join("preview.jpg");
        let image = image::ImageBuffer::from_fn(2, 2, |_, _| image::Rgb([255u8, 0, 0]));
        image.save(&source).unwrap();

        write_scaled_jpeg(&source, &destination, 320, 180).unwrap();

        let preview = image::open(&destination).unwrap();
        assert_eq!(preview.width(), 2);
        assert_eq!(preview.height(), 2);
    }

    #[test]
    fn a_png_is_not_a_renderable_jpeg() {
        let temp = tempfile::tempdir().unwrap();
        let png_path = temp.path().join("test.jpg");
        fs::write(&png_path, red_png()).unwrap();

        assert!(!is_renderable_jpeg(&png_path));
    }

    #[test]
    fn jpeg_path_for_changes_extension() {
        let path = Path::new("/cache/abc123.webp");
        assert_eq!(jpeg_path_for(path), Path::new("/cache/abc123.jpg"));

        let path = Path::new("/cache/abc123.png");
        assert_eq!(jpeg_path_for(path), Path::new("/cache/abc123.jpg"));

        let path = Path::new("/cache/abc123.jpg");
        assert_eq!(jpeg_path_for(path), Path::new("/cache/abc123.jpg"));
    }
}
