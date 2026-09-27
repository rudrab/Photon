//! Thumbnail and preview generation.
//!
//! Cache files are keyed by the image's **content hash**, never its DB id: ids
//! are reused when a library is recreated, which made stale thumbnails of other
//! photos show up. Layout: `<cache>/<size>/<hash[..2]>/<hash>.jpg`.
//!
//! Decode strategy, fastest first:
//!   1. JPEG: `jpeg-decoder` IDCT downscaling (decode at 1/2, 1/4 or 1/8 size)
//!   2. RAW:  largest embedded JPEG preview (e.g. the Olympus makernote preview),
//!      then the camera JPEG next to it, then the small EXIF thumbnail
//!   3. Resize with `fast_image_resize` (SIMD), then apply EXIF orientation.

use crate::metadata;
use anyhow::{Context, Result};
use exif::{In, Tag};
use fast_image_resize::images::Image as FirImage;
use fast_image_resize::{PixelType, ResizeOptions, Resizer};
use image::codecs::jpeg::JpegEncoder;
use image::{DynamicImage, ImageBuffer, RgbImage};
use photon_core::models::{Image, ImageFormat};
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Cursor, Read};
use std::path::{Path, PathBuf};

/// The sizes kept in the cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThumbSize {
    /// Grid cards and drill-down covers. Big enough for 400px cards on 2× displays
    /// once the cover crop is taken into account.
    Grid,
    /// The single-photo viewer. Generated on demand.
    Large,
}

impl ThumbSize {
    fn long_edge(self) -> u32 {
        match self {
            Self::Grid => 640,
            Self::Large => 2560,
        }
    }

    fn quality(self) -> u8 {
        match self {
            Self::Grid => 82,
            Self::Large => 88,
        }
    }

    fn dir(self) -> &'static str {
        match self {
            Self::Grid => "grid",
            Self::Large => "large",
        }
    }
}

/// Where the cached `size` rendition of the image with content `hash` lives.
pub fn thumb_path(cache_root: &Path, size: ThumbSize, hash: &str) -> PathBuf {
    let shard = hash.get(..2).unwrap_or("00");
    cache_root.join(size.dir()).join(shard).join(format!("{hash}.jpg"))
}

#[derive(Clone)]
pub struct ThumbnailGenerator {
    cache_root: PathBuf,
}

impl ThumbnailGenerator {
    pub fn new(cache_root: PathBuf) -> Self {
        Self { cache_root }
    }

    pub fn cache_root(&self) -> &Path {
        &self.cache_root
    }

    /// Delete the old id-keyed cache (`small/`, `medium/`). Its files belong to
    /// whatever photo had that id in some earlier library, so they are wrong.
    pub fn remove_legacy_cache(&self) {
        for dir in ["small", "medium"] {
            let path = self.cache_root.join(dir);
            if path.is_dir() {
                match fs::remove_dir_all(&path) {
                    Ok(()) => log::info!("Removed legacy id-keyed thumbnail cache {}", path.display()),
                    Err(e) => log::warn!("Could not remove {}: {e}", path.display()),
                }
            }
        }
    }

    pub fn exists(&self, image: &Image, size: ThumbSize) -> bool {
        thumb_path(&self.cache_root, size, &image.hash).exists()
    }

    /// Return the cached rendition of `image`, generating it first if needed.
    pub fn ensure(&self, image: &Image, size: ThumbSize) -> Result<PathBuf> {
        let out = thumb_path(&self.cache_root, size, &image.hash);
        if out.exists() {
            return Ok(out);
        }

        // Rows imported by older versions have no stored orientation.
        let orientation = image
            .orientation
            .or_else(|| metadata::extract(&image.path).ok()?.orientation)
            .unwrap_or(1);

        let rgb = load_smart(&image.path, size.long_edge())?.into_rgb8();
        let resized = resize(rgb, size.long_edge())?;
        let oriented = apply_orientation(resized, orientation);

        // Write to a unique temp name, then rename: the UI never sees a partial
        // file, and concurrent generators of the same image can't clash.
        let dir = out.parent().expect("thumb path has a parent");
        fs::create_dir_all(dir)?;
        let tmp = dir.join(format!(".{}.{}.tmp", image.hash, uuid::Uuid::new_v4().simple()));
        let result = (|| -> Result<()> {
            let mut writer = BufWriter::new(File::create(&tmp)?);
            JpegEncoder::new_with_quality(&mut writer, size.quality()).encode_image(&oriented)?;
            writer.into_inner().map_err(|e| e.into_error())?;
            fs::rename(&tmp, &out)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result.map(|()| out)
    }
}

fn resize(src: RgbImage, long_edge: u32) -> Result<RgbImage> {
    let (w, h) = src.dimensions();
    if w == 0 || h == 0 {
        anyhow::bail!("Zero-dimension image");
    }

    let scale = (long_edge as f64 / w.max(h) as f64).min(1.0);
    let dst_w = ((w as f64 * scale).round() as u32).max(1);
    let dst_h = ((h as f64 * scale).round() as u32).max(1);
    if (dst_w, dst_h) == (w, h) {
        return Ok(src);
    }

    let src = FirImage::from_vec_u8(w, h, src.into_raw(), PixelType::U8x3)?;
    let mut dst = FirImage::new(dst_w, dst_h, PixelType::U8x3);
    Resizer::new().resize(&src, &mut dst, &ResizeOptions::default())?;

    RgbImage::from_raw(dst_w, dst_h, dst.into_vec()).context("resize buffer mismatch")
}

/// Apply EXIF orientation so that the pixels are displayed upright.
fn apply_orientation(img: RgbImage, orientation: u16) -> RgbImage {
    use image::imageops::{flip_horizontal, flip_vertical, rotate180, rotate270, rotate90};
    match orientation {
        2 => flip_horizontal(&img),
        3 => rotate180(&img),
        4 => flip_vertical(&img),
        5 => flip_horizontal(&rotate90(&img)),
        6 => rotate90(&img),
        7 => flip_horizontal(&rotate270(&img)),
        8 => rotate270(&img),
        _ => img,
    }
}

/// Pick the fastest decode path that still yields at least `long_edge` pixels
/// where the source allows it.
fn load_smart(path: &Path, long_edge: u32) -> Result<DynamicImage> {
    let format = metadata::format_of(path);

    if format.is_raw() {
        return load_raw_preview(path, long_edge);
    }
    if format == ImageFormat::Jpeg {
        if let Ok(img) = decode_jpeg_scaled(File::open(path)?, long_edge) {
            return Ok(img);
        }
    }

    image::io::Reader::open(path)?
        .with_guessed_format()?
        .decode()
        .context("Failed to decode image")
}

/// RAW files can't be decoded here; use the best JPEG the camera stored.
fn load_raw_preview(path: &Path, long_edge: u32) -> Result<DynamicImage> {
    let data = fs::read(path)?;
    let embedded = largest_embedded_jpeg(&data);

    if let Some((start, w, h)) = embedded {
        if w.max(h) >= long_edge {
            return decode_jpeg_scaled(Cursor::new(&data[start..]), long_edge);
        }
    }
    // The embedded preview is smaller than wanted: the camera JPEG is better, if present.
    if let Some(jpeg) = find_camera_jpeg(path) {
        if let Ok(img) = decode_jpeg_scaled(File::open(jpeg)?, long_edge) {
            return Ok(img);
        }
    }
    if let Some((start, _, _)) = embedded {
        return decode_jpeg_scaled(Cursor::new(&data[start..]), long_edge);
    }
    exif_thumbnail(path, &data, long_edge)
}

/// Find every embedded JPEG (SOI marker followed by a parseable header) and
/// return the offset and size of the largest one.
///
/// This works across vendors without parsing each maker-note format: Olympus,
/// Canon, Nikon, Sony, Fuji and Panasonic all embed a baseline JPEG preview.
fn largest_embedded_jpeg(data: &[u8]) -> Option<(usize, u32, u32)> {
    let mut best: Option<(usize, u32, u32)> = None;
    let mut pos = 0;
    while let Some(off) = find_soi(&data[pos..]) {
        let start = pos + off;
        pos = start + 3;

        let mut decoder = jpeg_decoder::Decoder::new(Cursor::new(&data[start..]));
        if decoder.read_info().is_err() {
            continue;
        }
        let Some(info) = decoder.info() else { continue };
        let (w, h) = (info.width as u32, info.height as u32);
        if best.is_none_or(|(_, bw, bh)| w * h > bw * bh) {
            best = Some((start, w, h));
        }
    }
    best
}

/// Offset of the next `FF D8 FF` (JPEG SOI + start of the first marker).
fn find_soi(data: &[u8]) -> Option<usize> {
    data.windows(3).position(|w| w == [0xFF, 0xD8, 0xFF])
}

/// Last resort: the small EXIF (IFD1) thumbnail.
fn exif_thumbnail(path: &Path, data: &[u8], long_edge: u32) -> Result<DynamicImage> {
    let exif = metadata::read_exif(path)?;
    let field = |tag| {
        exif.get_field(tag, In::THUMBNAIL)
            .or_else(|| exif.get_field(tag, In::PRIMARY))
            .and_then(|f| f.value.get_uint(0))
    };
    let (Some(offset), Some(length)) = (
        field(Tag::JPEGInterchangeFormat),
        field(Tag::JPEGInterchangeFormatLength),
    ) else {
        anyhow::bail!("No embedded preview found");
    };
    let bytes = data
        .get(offset as usize..(offset as usize).saturating_add(length as usize))
        .context("EXIF thumbnail out of bounds")?;
    decode_jpeg_scaled(Cursor::new(bytes), long_edge)
}

/// Decode a JPEG at the smallest IDCT scale that still covers `long_edge`.
fn decode_jpeg_scaled<R: Read>(reader: R, long_edge: u32) -> Result<DynamicImage> {
    let mut decoder = jpeg_decoder::Decoder::new(BufReader::new(reader));
    decoder.read_info()?;
    let info = decoder.info().context("No JPEG metadata")?;

    // `scale` guarantees at least the requested size in *both* dimensions, so
    // request the short-edge equivalent of our long-edge target.
    let (w, h) = (info.width as u32, info.height as u32);
    let factor = long_edge as f64 / w.max(h).max(1) as f64;
    let req_w = ((w as f64 * factor).ceil() as u32).clamp(1, u16::MAX as u32) as u16;
    let req_h = ((h as f64 * factor).ceil() as u32).clamp(1, u16::MAX as u32) as u16;
    decoder.scale(req_w, req_h)?;

    let pixels = decoder.decode()?;
    let info = decoder.info().context("No JPEG metadata")?;
    let (w, h) = (info.width as u32, info.height as u32);

    match info.pixel_format {
        jpeg_decoder::PixelFormat::RGB24 => ImageBuffer::from_raw(w, h, pixels)
            .map(DynamicImage::ImageRgb8)
            .context("RGB buffer mismatch"),
        jpeg_decoder::PixelFormat::L8 => ImageBuffer::from_raw(w, h, pixels)
            .map(DynamicImage::ImageLuma8)
            .context("Luma buffer mismatch"),
        other => anyhow::bail!("Unsupported JPEG pixel format {other:?}"),
    }
}

/// The out-of-camera JPEG shot alongside a RAW file (never an edited variant:
/// an edit is not what the RAW looks like).
fn find_camera_jpeg(raw: &Path) -> Option<PathBuf> {
    ["JPG", "jpg", "JPEG", "jpeg"]
        .iter()
        .map(|ext| raw.with_extension(ext))
        .find(|p| p.exists())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{write_jpeg, Spec};

    fn image_at(path: &Path, orientation: Option<u16>) -> Image {
        let mut img = Image::new(path.to_path_buf(), crate::dedup::blake3_hash_file(path).unwrap(), 0);
        img.orientation = orientation;
        img
    }

    #[test]
    fn orientation_6_turns_a_landscape_into_portrait() {
        // 8x4 landscape stored sideways: EXIF 6 means "rotate 90° CW to display".
        let img = RgbImage::from_fn(8, 4, |x, y| image::Rgb([x as u8, y as u8, 0]));
        let out = apply_orientation(img.clone(), 6);
        assert_eq!(out.dimensions(), (4, 8));
        // Top-left of the source lands at top-right after a clockwise turn.
        assert_eq!(out.get_pixel(3, 0), img.get_pixel(0, 0));

        for o in [5, 7] {
            assert_eq!(apply_orientation(img.clone(), o).dimensions(), (4, 8));
        }
        for o in [1, 2, 3, 4] {
            assert_eq!(apply_orientation(img.clone(), o).dimensions(), (8, 4));
        }
    }

    #[test]
    fn cache_is_keyed_by_content_not_id() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.jpg");
        let b = dir.path().join("b.jpg");
        write_jpeg(&a, &Spec { seed: 10, width: 64, height: 32, ..Default::default() });
        write_jpeg(&b, &Spec { seed: 200, width: 64, height: 32, ..Default::default() });

        let gen = ThumbnailGenerator::new(dir.path().join("cache"));
        // Same DB id, different photos (as after a library rebuild).
        let mut img_a = image_at(&a, Some(1));
        let mut img_b = image_at(&b, Some(1));
        img_a.id = Some(1);
        img_b.id = Some(1);

        let pa = gen.ensure(&img_a, ThumbSize::Grid).unwrap();
        let pb = gen.ensure(&img_b, ThumbSize::Grid).unwrap();
        assert_ne!(pa, pb);
        assert_ne!(std::fs::read(pa).unwrap(), std::fs::read(pb).unwrap());
    }

    #[test]
    fn generates_upright_downscaled_thumbnail_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("wide.jpg");
        write_jpeg(&src, &Spec { width: 2000, height: 1000, ..Default::default() });

        let gen = ThumbnailGenerator::new(dir.path().join("cache"));
        // No stored orientation: it's read from the file's EXIF (6).
        let path = gen.ensure(&image_at(&src, None), ThumbSize::Grid).unwrap();

        let thumb = image::open(&path).unwrap();
        assert_eq!((thumb.width(), thumb.height()), (320, 640)); // rotated, 640px long edge
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn picks_the_largest_embedded_jpeg() {
        let dir = tempfile::tempdir().unwrap();
        let small = dir.path().join("s.jpg");
        let big = dir.path().join("b.jpg");
        write_jpeg(&small, &Spec { width: 160, height: 120, exif: false, ..Default::default() });
        write_jpeg(&big, &Spec { width: 1600, height: 1200, exif: false, ..Default::default() });

        // Fake RAW: junk, a tiny thumbnail, more junk, the real preview.
        let mut raw = b"IIRO\x08\0\0\0junk".to_vec();
        raw.extend(std::fs::read(&small).unwrap());
        raw.extend([0u8; 100]);
        let big_at = raw.len();
        raw.extend(std::fs::read(&big).unwrap());
        raw.extend([0xFF, 0xD8, 0xFF, 0x00, 0x01]); // false positive

        assert_eq!(largest_embedded_jpeg(&raw), Some((big_at, 1600, 1200)));

        let orf = dir.path().join("P1.ORF");
        std::fs::write(&orf, &raw).unwrap();
        let preview = load_raw_preview(&orf, 640).unwrap();
        assert_eq!(preview.width().max(preview.height()), 800); // 1/2 IDCT scale, ≥ 640
    }
}
