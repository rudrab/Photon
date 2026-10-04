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
use std::io::{BufReader, BufWriter, Cursor, Read, Seek, SeekFrom};
use rayon::prelude::*;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

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

/// Remove cached thumbnails (Grid and Large) for a given image hash.
pub fn invalidate_cache(cache_root: &Path, hash: &str) {
    for size in [ThumbSize::Grid, ThumbSize::Large] {
        let p = thumb_path(cache_root, size, hash);
        if p.exists() {
            let _ = fs::remove_file(p);
        }
    }
}

/// The ICC profile embedded in the file at `path`, if any (none = sRGB).
pub(crate) fn extract_icc_profile(path: &Path, format: ImageFormat) -> Option<Vec<u8>> {
    match format {
        ImageFormat::Jpeg => crate::icc::extract_jpeg_icc(path),
        ImageFormat::Png => crate::icc::extract_png_icc(path),
        ImageFormat::Webp => crate::icc::extract_webp_icc(path),
        ImageFormat::Heif | ImageFormat::Avif => crate::icc::extract_heif_icc(path),
        ImageFormat::Tiff | ImageFormat::RawOrf | ImageFormat::RawCr2 | ImageFormat::RawCr3
        | ImageFormat::RawNef | ImageFormat::RawArw | ImageFormat::RawDng | ImageFormat::RawRaf
        | ImageFormat::RawRw2 => crate::icc::extract_tiff_icc(path),
        _ => None,
    }
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

    pub fn invalidate(&self, hash: &str) {
        invalidate_cache(&self.cache_root, hash);
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

        let format = metadata::format_of(&image.path);
        let rgb = load_smart(&image.path, size.long_edge())?.into_rgb8();
        let rgb = crate::icc::to_srgb(rgb, extract_icc_profile(&image.path, format));
        let resized = resize(rgb, size.long_edge())?;
        let oriented = if matches!(format, ImageFormat::Heif | ImageFormat::Avif) && orientation == 1 {
            resized
        } else {
            apply_orientation(resized, orientation)
        };

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

    /// Return the grid rendition of `image`, computing its ThumbHash in the same pass.
    pub fn ensure_grid(&self, image: &Image) -> Result<(PathBuf, Option<Vec<u8>>)> {
        let out = thumb_path(&self.cache_root, ThumbSize::Grid, &image.hash);
        if out.exists() {
            let th = if image.thumbhash.is_some() {
                image.thumbhash.clone()
            } else {
                self.thumbhash_from_disk(&image.hash)
            };
            return Ok((out, th));
        }

        let orientation = image
            .orientation
            .or_else(|| metadata::extract(&image.path).ok()?.orientation)
            .unwrap_or(1);

        let format = metadata::format_of(&image.path);
        let rgb = load_smart(&image.path, ThumbSize::Grid.long_edge())?.into_rgb8();
        let rgb = crate::icc::to_srgb(rgb, extract_icc_profile(&image.path, format));
        let resized = resize(rgb, ThumbSize::Grid.long_edge())?;
        let oriented = if matches!(format, ImageFormat::Heif | ImageFormat::Avif) && orientation == 1 {
            resized
        } else {
            apply_orientation(resized, orientation)
        };

        let th = compute_thumbhash(&oriented);

        let dir = out.parent().expect("thumb path has a parent");
        fs::create_dir_all(dir)?;
        let tmp = dir.join(format!(".{}.{}.tmp", image.hash, uuid::Uuid::new_v4().simple()));
        let result = (|| -> Result<()> {
            let mut writer = BufWriter::new(File::create(&tmp)?);
            JpegEncoder::new_with_quality(&mut writer, ThumbSize::Grid.quality()).encode_image(&oriented)?;
            writer.into_inner().map_err(|e| e.into_error())?;
            fs::rename(&tmp, &out)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result.map(|()| (out, th))
    }

    /// Compute ThumbHash from an existing cached grid thumbnail on disk.
    pub fn thumbhash_from_disk(&self, hash: &str) -> Option<Vec<u8>> {
        let path = thumb_path(&self.cache_root, ThumbSize::Grid, hash);
        if path.exists() {
            if let Ok(img) = image::open(&path) {
                return compute_thumbhash(&img.to_rgb8());
            }
        }
        None
    }
}

/// Compute ThumbHash from an in-memory upright RGB image.
pub fn compute_thumbhash(img: &RgbImage) -> Option<Vec<u8>> {
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        return None;
    }
    let max_dim = w.max(h);
    let (tw, th) = if max_dim > 100 {
        let scale = 100.0 / max_dim as f64;
        (
            ((w as f64 * scale).round() as u32).max(1),
            ((h as f64 * scale).round() as u32).max(1),
        )
    } else {
        (w, h)
    };

    let small = if (tw, th) != (w, h) {
        resize(img.clone(), tw.max(th)).ok()?
    } else {
        img.clone()
    };

    let rgba = DynamicImage::ImageRgb8(small).to_rgba8();
    Some(thumbhash::rgba_to_thumb_hash(
        rgba.width() as usize,
        rgba.height() as usize,
        rgba.as_raw(),
    ))
}

#[derive(Debug, Clone)]
pub struct HistogramData {
    pub r: [u32; 256],
    pub g: [u32; 256],
    pub b: [u32; 256],
    pub lum: [u32; 256],
    pub max_val: u32,
    pub shadow_clip: bool,
    pub highlight_clip: bool,
}

impl Default for HistogramData {
    fn default() -> Self {
        Self {
            r: [0u32; 256],
            g: [0u32; 256],
            b: [0u32; 256],
            lum: [0u32; 256],
            max_val: 0,
            shadow_clip: false,
            highlight_clip: false,
        }
    }
}

impl HistogramData {
    pub fn compute(img: &RgbImage) -> Self {
        let mut r = [0u32; 256];
        let mut g = [0u32; 256];
        let mut b = [0u32; 256];
        let mut lum = [0u32; 256];

        for pixel in img.pixels() {
            let pr = pixel[0] as usize;
            let pg = pixel[1] as usize;
            let pb = pixel[2] as usize;
            let plum = ((0.2126 * pixel[0] as f64 + 0.7152 * pixel[1] as f64 + 0.0722 * pixel[2] as f64)
                .round() as usize)
                .clamp(0, 255);

            r[pr] += 1;
            g[pg] += 1;
            b[pb] += 1;
            lum[plum] += 1;
        }

        let mut max_val = 1u32;
        for i in 1..255 {
            max_val = max_val.max(r[i]).max(g[i]).max(b[i]).max(lum[i]);
        }

        let total_pixels = (img.width() * img.height()) as f64;
        let shadow_clip = (lum[0] as f64 / total_pixels) > 0.01;
        let highlight_clip = (lum[255] as f64 / total_pixels) > 0.01;

        Self {
            r,
            g,
            b,
            lum,
            max_val,
            shadow_clip,
            highlight_clip,
        }
    }
}

/// Compute histogram from an image file (e.g. cached thumbnail or source).
pub fn compute_histogram(path: &Path) -> Option<HistogramData> {
    let img = image::open(path).ok()?.to_rgb8();
    Some(HistogramData::compute(&img))
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

/// `image` upright at full resolution, for 1:1 viewing: RAW files developed
/// from their sensor data (see [`raw::develop`](crate::raw::develop)), the
/// rest decoded in full. In sRGB.
pub fn full_resolution(image: &Image) -> Result<RgbImage> {
    let format = metadata::format_of(&image.path);
    if format.is_raw() {
        let rgb = crate::raw::develop(&image.path)?;
        return Ok(apply_orientation(rgb, orientation_of(image)));
    }
    let (img, icc) = decode_upright(image)?;
    Ok(crate::icc::to_srgb(img.into_rgb8(), icc))
}

/// `image`'s stored orientation, or, for rows imported by older versions, the
/// file's.
pub(crate) fn orientation_of(image: &Image) -> u16 {
    image
        .orientation
        .or_else(|| metadata::extract(&image.path).ok()?.orientation)
        .unwrap_or(1)
}

/// `image` (anything but a RAW file) decoded upright at full resolution, at
/// its own bit depth and in its own colour space, with that space's ICC
/// profile (`None` = sRGB).
pub(crate) fn decode_upright(image: &Image) -> Result<(DynamicImage, Option<Vec<u8>>)> {
    let orientation = orientation_of(image);
    let format = metadata::format_of(&image.path);
    let img = if format.is_video() {
        load_video_frame(&image.path, 2560)?
    } else if matches!(format, ImageFormat::Heif | ImageFormat::Avif) {
        // libheif has already applied the file's rotation (irot/imir); only
        // a rotation made in Photon since import is left.
        decode_heif_or_avif(&image.path)?
    } else if format == ImageFormat::Gif {
        decode_gif_first_frame(&image.path)?
    } else {
        image::io::Reader::open(&image.path)?
            .with_guessed_format()?
            .decode()
            .context("Failed to decode image")?
    };
    let icc = if format.is_video() || format == ImageFormat::Gif {
        None
    } else {
        extract_icc_profile(&image.path, format)
    };
    Ok((orient_dynamic(img, orientation), icc))
}

/// [`apply_orientation`] for any pixel type.
fn orient_dynamic(img: DynamicImage, orientation: u16) -> DynamicImage {
    match orientation {
        2 => img.fliph(),
        3 => img.rotate180(),
        4 => img.flipv(),
        5 => img.rotate90().fliph(),
        6 => img.rotate90(),
        7 => img.rotate270().fliph(),
        8 => img.rotate270(),
        _ => img,
    }
}

/// The largest preview the camera embedded in RAW `image`, upright.
pub(crate) fn embedded_preview_upright(image: &Image) -> Result<RgbImage> {
    let orientation = image
        .orientation
        .or_else(|| metadata::extract(&image.path).ok()?.orientation)
        .unwrap_or(1);
    let preview = load_raw_preview(&image.path, u32::MAX)?.into_rgb8();
    Ok(apply_orientation(preview, orientation))
}

/// Mean brightness (sRGB-encoded luminance, 0–1) of the camera's embedded
/// preview in RAW file `path`: what the camera meant the photo to look like.
pub(crate) fn embedded_preview_luma(path: &Path) -> Option<f32> {
    let data = fs::read(path).ok()?;
    let (start, _, _) = largest_embedded_jpeg(&data)?;
    let preview = decode_jpeg_scaled(Cursor::new(&data[start..]), 512).ok()?.into_rgb8();
    let decode = |v: u8| {
        let v = v as f32 / 255.0;
        if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
    };
    let encode = |y: f32| if y <= 0.003_130_8 { 12.92 * y } else { 1.055 * y.powf(1.0 / 2.4) - 0.055 };
    let n = preview.pixels().len().max(1) as f32;
    let sum: f32 = preview
        .pixels()
        .map(|p| encode(0.2126 * decode(p[0]) + 0.7152 * decode(p[1]) + 0.0722 * decode(p[2])))
        .sum();
    Some(sum / n)
}

/// Apply EXIF orientation so that the pixels are displayed upright.
pub(crate) fn apply_orientation<P>(img: ImageBuffer<P, Vec<P::Subpixel>>, orientation: u16) -> ImageBuffer<P, Vec<P::Subpixel>>
where
    P: image::Pixel + 'static,
{
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

    if format.is_video() {
        return load_video_frame(path, long_edge);
    }
    if matches!(format, ImageFormat::Heif | ImageFormat::Avif) {
        return decode_heif_or_avif(path);
    }
    if format == ImageFormat::Gif {
        return decode_gif_first_frame(path);
    }
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

/// Decode HEIF or AVIF file via libheif-rs into DynamicImage.
/// Note that libheif automatically applies `irot` and `imir` transformations on decode.
pub fn decode_heif_or_avif(path: &Path) -> Result<DynamicImage> {
    use libheif_rs::{ColorSpace, HeifContext, LibHeif, RgbChroma};
    let path_str = path.to_str().context("invalid UTF-8 path")?;
    let ctx = HeifContext::read_from_file(path_str)?;
    let handle = ctx.primary_image_handle()?;
    let lib_heif = LibHeif::new();
    let img = lib_heif.decode(&handle, ColorSpace::Rgb(RgbChroma::Rgb), None)?;
    let planes = img.planes();
    let plane = planes.interleaved.context("libheif decoded plane is not interleaved")?;
    let w = plane.width;
    let h = plane.height;
    let row_size = (w * 3) as usize;
    if row_size > plane.stride {
        anyhow::bail!("Row size exceeds stride in libheif plane");
    }
    let mut buf = Vec::with_capacity(row_size * h as usize);
    for row in plane.data.chunks_exact(plane.stride).take(h as usize) {
        buf.extend_from_slice(&row[..row_size]);
    }
    let rgb = RgbImage::from_raw(w, h, buf).context("mismatched buffer size in decoded HEIF")?;
    Ok(DynamicImage::ImageRgb8(rgb))
}

/// Decode the first frame of a GIF file.
pub fn decode_gif_first_frame(path: &Path) -> Result<DynamicImage> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let decoder = image::codecs::gif::GifDecoder::new(reader)?;
    use image::AnimationDecoder;
    let frames = decoder.into_frames();
    let mut frames_iter = frames.into_iter();
    if let Some(first_frame) = frames_iter.next() {
        let frame = first_frame?;
        let buffer = frame.into_buffer();
        Ok(DynamicImage::ImageRgba8(buffer))
    } else {
        anyhow::bail!("GIF has no frames: {}", path.display());
    }
}

/// Extract a video frame at approximately 1 second using installed video thumbnailer.
/// Tries `gst-video-thumbnailer`, then `ffmpegthumbnailer`, then `gst-launch-1.0`.
pub fn load_video_frame(path: &Path, long_edge: u32) -> Result<DynamicImage> {
    let tmp = tempfile::Builder::new()
        .prefix("photon-vid-thumb-")
        .suffix(".png")
        .tempfile()?;
    let tmp_path = tmp.path().to_path_buf();

    // 1. Try gst-video-thumbnailer (standard on GNOME / Fedora)
    let gst_status = std::process::Command::new("gst-video-thumbnailer")
        .arg("--input-path")
        .arg(path)
        .arg("--output")
        .arg(&tmp_path)
        .arg("--size")
        .arg(long_edge.to_string())
        .status();

    if gst_status.is_ok_and(|s| s.success()) && tmp_path.exists() && fs::metadata(&tmp_path).map_or(false, |m| m.len() > 0) {
        let bytes = fs::read(&tmp_path)?;
        return Ok(image::load_from_memory(&bytes)?);
    }

    // 2. Try ffmpegthumbnailer
    let ffmpeg_status = std::process::Command::new("ffmpegthumbnailer")
        .arg("-i")
        .arg(path)
        .arg("-o")
        .arg(&tmp_path)
        .arg("-s")
        .arg(long_edge.to_string())
        .arg("-t")
        .arg("1")
        .status();

    if ffmpeg_status.is_ok_and(|s| s.success()) && tmp_path.exists() && fs::metadata(&tmp_path).map_or(false, |m| m.len() > 0) {
        let bytes = fs::read(&tmp_path)?;
        return Ok(image::load_from_memory(&bytes)?);
    }

    // 3. Fallback to gst-launch-1.0
    let uri = format!("file://{}", path.canonicalize().unwrap_or_else(|_| path.to_path_buf()).display());
    let gst_launch = std::process::Command::new("gst-launch-1.0")
        .arg("-q")
        .arg("playbin")
        .arg(format!("uri={uri}"))
        .arg(format!("video-sink=videoconvert ! pngenc ! filesink location={}", tmp_path.display()))
        .status();

    if gst_launch.is_ok_and(|s| s.success()) && tmp_path.exists() && fs::metadata(&tmp_path).map_or(false, |m| m.len() > 0) {
        let bytes = fs::read(&tmp_path)?;
        return Ok(image::load_from_memory(&bytes)?);
    }

    anyhow::bail!("Failed to generate video thumbnail for {}", path.display());
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
    embedded_jpegs(data).into_iter().max_by_key(|&(_, w, h)| w as u64 * h as u64)
}

/// Every embedded JPEG in `data`: offset and dimensions.
fn embedded_jpegs(data: &[u8]) -> Vec<(usize, u32, u32)> {
    let mut found = Vec::new();
    let mut pos = 0;
    while let Some(off) = find_soi(&data[pos..]) {
        let start = pos + off;
        pos = start + 3;

        let mut decoder = jpeg_decoder::Decoder::new(Cursor::new(&data[start..]));
        if decoder.read_info().is_err() {
            continue;
        }
        if let Some(info) = decoder.info() {
            found.push((start, info.width as u32, info.height as u32));
        }
    }
    found
}

/// Offset of the next `FF D8 FF` (JPEG SOI + start of the first marker).
fn find_soi(data: &[u8]) -> Option<usize> {
    data.windows(3).position(|w| w == [0xFF, 0xD8, 0xFF])
}

/// Last resort: the small EXIF (IFD1) thumbnail.
fn exif_thumbnail(path: &Path, data: &[u8], long_edge: u32) -> Result<DynamicImage> {
    let exif = metadata::read_exif(path)?;
    let (offset, length) = exif_thumbnail_range(&exif).context("No embedded preview found")?;
    let bytes = data
        .get(offset..offset.saturating_add(length))
        .context("EXIF thumbnail out of bounds")?;
    decode_jpeg_scaled(Cursor::new(bytes), long_edge)
}

/// Offset and length of the EXIF (IFD1) JPEG thumbnail.
fn exif_thumbnail_range(exif: &exif::Exif) -> Option<(usize, usize)> {
    let field = |tag| {
        exif.get_field(tag, In::THUMBNAIL)
            .or_else(|| exif.get_field(tag, In::PRIMARY))
            .and_then(|f| f.value.get_uint(0))
    };
    let offset = field(Tag::JPEGInterchangeFormat)? as usize;
    let length = field(Tag::JPEGInterchangeFormatLength)? as usize;
    (length > 0).then_some((offset, length))
}

/// Files up to this size may be decoded for a quick preview when they carry
/// no EXIF thumbnail (screenshots, exports). Bigger ones get none.
const QUICK_DECODE_MAX_BYTES: u64 = 12 * 1024 * 1024;
/// How much of a RAW file to search for a small embedded JPEG.
const RAW_PREVIEW_SCAN_BYTES: u64 = 128 * 1024;

/// Raw RGB8 pixels of a quick preview.
pub struct Preview {
    pub width: u32,
    pub height: u32,
    /// Tightly packed RGB, `width * 3` bytes per row.
    pub rgb: Vec<u8>,
}

/// [`quick_preview`] for many files, a few at a time (the same bounded I/O
/// parallelism as imports, which suits card readers). `on_preview(index, …)`
/// is called from worker threads as each finishes; files without a preview
/// are skipped. Stops early once `cancelled` is set.
pub fn quick_previews(
    paths: &[PathBuf],
    long_edge: u32,
    cancelled: &AtomicBool,
    on_preview: impl Fn(usize, Preview) + Sync,
) -> Result<()> {
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(crate::device::PARALLEL_IO_THREADS)
        .thread_name(|i| format!("photon-preview-{i}"))
        .build()?;
    pool.install(|| {
        paths.par_iter().enumerate().for_each(|(i, path)| {
            if cancelled.load(Ordering::Relaxed) {
                return;
            }
            if let Ok(img) = quick_preview(path, long_edge) {
                let (width, height) = img.dimensions();
                on_preview(i, Preview { width, height, rgb: img.into_raw() });
            }
        })
    });
    Ok(())
}

/// A small, upright preview for the pre-import review, reading as little of
/// the file as possible: the EXIF thumbnail sits in the header (a few KB, even
/// for 50 MB RAW files). Files without one are decoded only if small.
pub fn quick_preview(path: &Path, long_edge: u32) -> Result<RgbImage> {
    let format = metadata::format_of(path);
    let exif = metadata::read_exif(path).ok();
    let orientation = exif
        .as_ref()
        .and_then(|e| e.get_field(Tag::Orientation, In::PRIMARY)?.value.get_uint(0))
        .filter(|o| (1..=8).contains(o))
        .unwrap_or(1) as u16;

    let embedded = exif.as_ref().and_then(|exif| {
        let (offset, length) = exif_thumbnail_range(exif)?;
        let bytes = if metadata::is_tiff_family(format) {
            // Offsets are file-relative, and the parse buffer beyond the
            // header read is zero padding: fetch just this range.
            let mut file = File::open(path).ok()?;
            file.seek(SeekFrom::Start(offset as u64)).ok()?;
            let mut buf = vec![0u8; length];
            file.read_exact(&mut buf).ok()?;
            buf
        } else {
            exif.buf().get(offset..offset.checked_add(length)?)?.to_vec()
        };
        decode_jpeg_scaled(Cursor::new(bytes), long_edge).ok()
    });

    // RAW without an IFD1 thumbnail (e.g. Olympus keeps it in the maker
    // note): the largest JPEG that is complete within the first bytes.
    let embedded = embedded.or_else(|| {
        if !format.is_raw() {
            return None;
        }
        let mut prefix = Vec::new();
        File::open(path).ok()?.take(RAW_PREVIEW_SCAN_BYTES).read_to_end(&mut prefix).ok()?;
        // Smallest candidate that is big enough first (cheapest to decode),
        // then larger ones, then any. A preview that runs past the prefix
        // fails to decode and is skipped.
        let mut candidates = embedded_jpegs(&prefix);
        candidates.sort_by_key(|&(_, w, h)| (w.max(h) < long_edge, w as u64 * h as u64));
        candidates
            .into_iter()
            .find_map(|(start, _, _)| decode_jpeg_scaled(Cursor::new(&prefix[start..]), long_edge).ok())
    });

    let image = match embedded {
        Some(img) => img,
        None if !format.is_raw() && fs::metadata(path)?.len() <= QUICK_DECODE_MAX_BYTES => {
            load_smart(path, long_edge)?
        }
        None => anyhow::bail!("no quick preview available"),
    };
    Ok(apply_orientation(resize(image.into_rgb8(), long_edge)?, orientation))
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
    fn quick_preview_is_small_and_upright() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("big.jpg");
        // EXIF orientation 6, no EXIF thumbnail: falls back to a scaled decode.
        write_jpeg(&src, &Spec { width: 1200, height: 600, ..Default::default() });
        let preview = quick_preview(&src, 160).unwrap();
        assert_eq!(preview.dimensions(), (80, 160));
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

    #[test]
    fn invalidate_cache_removes_both_sizes() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("test.jpg");
        write_jpeg(&src, &Spec { width: 1000, height: 1000, ..Default::default() });

        let cache = dir.path().join("cache");
        let gen = ThumbnailGenerator::new(cache.clone());
        let img = image_at(&src, None);
        let grid_path = gen.ensure_grid(&img).unwrap().0;
        let large_path = gen.ensure(&img, ThumbSize::Large).unwrap();

        assert!(grid_path.exists());
        assert!(large_path.exists());

        invalidate_cache(&cache, &img.hash);
        assert!(!grid_path.exists());
        assert!(!large_path.exists());
    }

    #[test]
    fn decodes_heic_and_avif_fixtures() {
        let fixtures_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        let avif = fixtures_dir.join("sample.avif");
        assert!(avif.exists(), "sample.avif fixture must exist in tests/fixtures");

        let dyn_img = decode_heif_or_avif(&avif).expect("AVIF decode must succeed");
        assert!(dyn_img.width() > 0 && dyn_img.height() > 0);

        let img_model = Image {
            id: Some(10),
            hash: "avif_sample_hash".to_string(),
            path: avif.clone(),
            filename: "sample.avif".to_string(),
            format: Some(ImageFormat::Avif),
            orientation: Some(1),
            ..Default::default()
        };

        // Full resolution decode
        let full_res = full_resolution(&img_model).expect("AVIF full resolution decode must succeed");
        assert!(full_res.width() > 0 && full_res.height() > 0);

        // Thumbnail generator ensure_grid
        let cache_dir = tempfile::tempdir().unwrap();
        let gen = ThumbnailGenerator::new(cache_dir.path().to_path_buf());
        let (thumb_path, _) = gen.ensure_grid(&img_model).expect("AVIF thumbnail generation must succeed");
        assert!(thumb_path.exists());
        assert!(fs::metadata(&thumb_path).unwrap().len() > 0);

        let heic = fixtures_dir.join("sample.heic");
        assert!(heic.exists(), "sample.heic fixture must exist in tests/fixtures");
        match decode_heif_or_avif(&heic) {
            Ok(dyn_img) => {
                assert!(dyn_img.width() > 0 && dyn_img.height() > 0);
                let heic_model = Image {
                    id: Some(11),
                    hash: "heic_sample_hash".to_string(),
                    path: heic.clone(),
                    filename: "sample.heic".to_string(),
                    format: Some(ImageFormat::Heif),
                    orientation: Some(1),
                    ..Default::default()
                };
                let full = full_resolution(&heic_model).unwrap();
                assert!(full.width() > 0 && full.height() > 0);
            }
            Err(e) => crate::testutil::assert_missing_hevc_decoder("decodes_heic_and_avif_fixtures", &e.to_string()),
        }
    }

    #[test]
    fn decodes_gif_first_frame_correctly() {
        let fixtures_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        let gif_path = fixtures_dir.join("sample.gif");
        assert!(gif_path.exists(), "sample.gif fixture must exist");

        let decoded = decode_gif_first_frame(&gif_path).expect("GIF decode first frame must succeed");
        assert_eq!((decoded.width(), decoded.height()), (16, 16));

        let img_model = Image {
            id: Some(12),
            hash: "gif_sample_hash".to_string(),
            path: gif_path.clone(),
            filename: "sample.gif".to_string(),
            format: Some(ImageFormat::Gif),
            orientation: Some(1),
            ..Default::default()
        };

        let full = full_resolution(&img_model).expect("GIF full resolution must succeed");
        assert_eq!((full.width(), full.height()), (16, 16));

        let cache_dir = tempfile::tempdir().unwrap();
        let gen = ThumbnailGenerator::new(cache_dir.path().to_path_buf());
        let (thumb_path, _) = gen.ensure_grid(&img_model).expect("GIF thumbnail generation must succeed");
        assert!(thumb_path.exists());
    }

    #[test]
    fn video_thumbnail_graceful_degradation_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let dummy_vid = dir.path().join("corrupted.mp4");
        std::fs::write(&dummy_vid, b"not a real video file").unwrap();

        let img = Image {
            id: Some(13),
            hash: "corrupt_vid_hash".to_string(),
            path: dummy_vid.clone(),
            filename: "corrupted.mp4".to_string(),
            format: Some(ImageFormat::VideoMp4),
            ..Default::default()
        };

        let cache_dir = tempfile::tempdir().unwrap();
        let gen = ThumbnailGenerator::new(cache_dir.path().to_path_buf());

        // Generation should fail gracefully without crashing
        let res = gen.ensure_grid(&img);
        assert!(res.is_err(), "Invalid video thumbnail should return Err, not panic");
    }
}

