//! Batch export.
//!
//! Each photo is decoded upright at full size (RAW files through darktable,
//! with their edits, or Photon's own renderer) in the chosen colour space
//! (sRGB, Adobe RGB, Display P3), then resized, sharpened and watermarked,
//! and encoded as JPEG, PNG, WebP or 8/16-bit TIFF. The file is tagged with
//! its colour profile and, if asked, the camera's metadata and the library's
//! title, description, keywords and rating, before it gets its final name.
//! Photos are exported in parallel (Rayon).

use crate::icc::{ColorSpace, Rgb16Image};
use crate::{metadata, raw, sidecar, thumbnails};
use anyhow::{bail, Context, Result};
use fast_image_resize::images::Image as FirImage;
use fast_image_resize::{PixelType, ResizeOptions, Resizer};
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::{ColorType, DynamicImage, ImageEncoder, Rgb, RgbImage, RgbaImage};
use photon_core::models::Image;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExportFormat {
    Jpeg { quality: u8 },
    Png,
    Webp { quality: u8 },
    /// `bit_depth` 8 or 16.
    Tiff { bit_depth: u8 },
}

impl Default for ExportFormat {
    fn default() -> Self {
        Self::Jpeg { quality: 90 }
    }
}

impl ExportFormat {
    pub fn extension(self) -> &'static str {
        match self {
            ExportFormat::Jpeg { .. } => "jpg",
            ExportFormat::Png => "png",
            ExportFormat::Webp { .. } => "webp",
            ExportFormat::Tiff { .. } => "tif",
        }
    }

    fn sixteen_bit(self) -> bool {
        matches!(self, ExportFormat::Tiff { bit_depth: 16 })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Sharpening {
    #[default]
    None,
    Screen,
    Print,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum WatermarkPosition {
    TopLeft,
    TopRight,
    BottomLeft,
    #[default]
    BottomRight,
    Center,
}

impl WatermarkPosition {
    pub const ALL: [WatermarkPosition; 5] = [
        WatermarkPosition::TopLeft,
        WatermarkPosition::TopRight,
        WatermarkPosition::BottomLeft,
        WatermarkPosition::BottomRight,
        WatermarkPosition::Center,
    ];

    pub fn label(self) -> &'static str {
        match self {
            WatermarkPosition::TopLeft => "Top left",
            WatermarkPosition::TopRight => "Top right",
            WatermarkPosition::BottomLeft => "Bottom left",
            WatermarkPosition::BottomRight => "Bottom right",
            WatermarkPosition::Center => "Center",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WatermarkConfig {
    /// Drawn in white with a soft shadow. Used when there is no `image_path`.
    pub text: Option<String>,
    /// An image such as a logo (PNG with transparency).
    pub image_path: Option<PathBuf>,
    /// 0–1.
    pub opacity: f32,
    /// The watermark's width as a fraction of the photo's width.
    pub scale: f32,
    pub position: WatermarkPosition,
}

impl Default for WatermarkConfig {
    fn default() -> Self {
        Self {
            text: None,
            image_path: None,
            opacity: 0.7,
            scale: 0.2,
            position: WatermarkPosition::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExportResize {
    Original,
    FitLongEdge(u32),
    FitBoundingBox(u32, u32),
}

impl Default for ExportResize {
    fn default() -> Self {
        Self::Original
    }
}

/// Everything about how photos are exported. Saved as JSON in export presets,
/// so every field has a default.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ExportConfig {
    pub destination_dir: PathBuf,
    pub format: ExportFormat,
    pub resize: ExportResize,
    pub prefix: String,
    pub suffix: String,
    /// Embed metadata: the camera's EXIF, IPTC and XMP from the source file,
    /// and the library's title, description, keywords and rating. Off, an
    /// export carries no metadata at all, only its colour profile.
    pub preserve_metadata: bool,
    /// With `preserve_metadata`: leave out the GPS location.
    pub strip_gps: bool,
    pub filename_template: Option<String>,
    pub raw_renderer: RawRenderer,
    pub sharpening: Sharpening,
    pub watermark: Option<WatermarkConfig>,
    pub color_space: ColorSpace,
}

/// How RAW files are turned into pixels for export.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum RawRenderer {
    /// darktable, with the photo's edits from its XMP sidecar; the built-in
    /// renderer where darktable isn't installed or fails.
    #[default]
    Darktable,
    /// Photon's own neutral rendering (see [`raw::develop`]).
    Builtin,
}

impl Default for ExportConfig {
    fn default() -> Self {
        Self {
            destination_dir: dirs::picture_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("Photon_Export"),
            format: ExportFormat::default(),
            resize: ExportResize::default(),
            prefix: String::new(),
            suffix: String::new(),
            preserve_metadata: true,
            strip_gps: false,
            filename_template: None,
            raw_renderer: RawRenderer::default(),
            sharpening: Sharpening::None,
            watermark: None,
            color_space: ColorSpace::default(),
        }
    }
}

/// A photo to export, with what the library knows about it beyond the file.
#[derive(Debug, Clone, Default)]
pub struct ExportItem {
    pub image: Image,
    pub keywords: Vec<String>,
}

#[derive(Debug, Default)]
pub struct ExportReport {
    pub total: usize,
    pub exported: usize,
    pub failed: usize,
    pub errors: Vec<(String, String)>,
}

/// Export a single image according to `config`.
pub fn export_single(image: &Image, config: &ExportConfig) -> Result<PathBuf> {
    export_single_with_warning(image, &[], config).map(|(p, _)| p)
}

/// Export `image`, tagged with `keywords`, and return any non-fatal warning
/// (e.g. a RAW file that fell back to its embedded preview).
pub fn export_single_with_warning(
    image: &Image,
    keywords: &[String],
    config: &ExportConfig,
) -> Result<(PathBuf, Option<String>)> {
    fs::create_dir_all(&config.destination_dir)?;
    let mut warnings = Vec::new();

    // 1. Decode upright, at full resolution, in the output colour space.
    let (pixels, warning) = load_for_export(image, config)?;
    warnings.extend(warning);

    // 2. Resize, output sharpening, watermark.
    let mut pixels = pixels.resized(&config.resize)?;
    if config.sharpening != Sharpening::None {
        pixels = pixels.sharpened(config.sharpening);
    }
    if let Some(ref wm) = config.watermark {
        if let Err(e) = pixels.add_watermark(wm, config.color_space) {
            warnings.push(format!("Watermark not applied: {e:#}"));
        }
    }

    // 3. Where it goes: never over an existing file.
    let dest_path = destination_path(image, config);

    // 4. Encode to a temporary file, tag it, and only then give it its final name.
    let embed = Embed {
        source: &image.path,
        space: config.color_space,
        metadata: config.preserve_metadata,
        strip_gps: config.strip_gps,
        size: pixels.dimensions(),
        rating: image.rating.clamp(0, 5),
        title: image.title.as_deref().filter(|t| !t.is_empty()),
        description: image.description.as_deref().filter(|d| !d.is_empty()),
        keywords,
    };
    let tmp_path = dest_path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4().simple()));
    let result = (|| -> Result<Vec<String>> {
        encode(pixels, config.format, &tmp_path)?;
        let warnings = embed_metadata(&tmp_path, config.format, &embed)?;
        File::open(&tmp_path)?.sync_all()?;
        fs::rename(&tmp_path, &dest_path)?;
        Ok(warnings)
    })();
    match result {
        Ok(w) => warnings.extend(w),
        Err(e) => {
            let _ = fs::remove_file(&tmp_path);
            return Err(e);
        }
    }

    // 5. What couldn't go into the file goes into a sidecar next to it. (The
    //    source's own sidecar is never copied: its darktable history would be
    //    applied to the export a second time.)
    if config.preserve_metadata && !exiftool_installed() && !matches!(config.format, ExportFormat::Jpeg { .. }) {
        let update = sidecar::XmpUpdate {
            rating: (embed.rating > 0).then_some(embed.rating),
            title: embed.title,
            description: embed.description,
            keywords: (!keywords.is_empty()).then_some(sidecar::Keywords { tags: keywords, known: keywords }),
            ..Default::default()
        };
        if let Err(e) = sidecar::sync_xmp_metadata(&dest_path, &update) {
            warnings.push(format!("XMP sidecar not written: {e}"));
        }
    }

    let warning = (!warnings.is_empty()).then(|| warnings.join("; "));
    Ok((dest_path, warning))
}

/// Multithreaded batch export using Rayon.
pub fn batch_export(
    items: &[ExportItem],
    config: ExportConfig,
    cancel_flag: Arc<AtomicBool>,
    progress_callback: impl Fn(usize, usize) + Send + Sync + 'static,
) -> ExportReport {
    let total = items.len();
    let completed = AtomicUsize::new(0);
    let failed = AtomicUsize::new(0);
    let errors = std::sync::Mutex::new(Vec::new());

    progress_callback(0, total);

    items.par_iter().for_each(|item| {
        if cancel_flag.load(Ordering::Relaxed) {
            return;
        }

        match export_single_with_warning(&item.image, &item.keywords, &config) {
            Ok((_path, warn_opt)) => {
                let done = completed.fetch_add(1, Ordering::SeqCst) + 1;
                progress_callback(done, total);
                if let Some(warn) = warn_opt {
                    if let Ok(mut errs) = errors.lock() {
                        errs.push((item.image.filename.clone(), warn));
                    }
                }
            }
            Err(e) => {
                failed.fetch_add(1, Ordering::SeqCst);
                if let Ok(mut errs) = errors.lock() {
                    errs.push((item.image.filename.clone(), format!("{e:#}")));
                }
            }
        }
    });

    ExportReport {
        total,
        exported: completed.load(Ordering::SeqCst),
        failed: failed.load(Ordering::SeqCst),
        errors: errors.into_inner().unwrap_or_default(),
    }
}

// ── Pixels ───────────────────────────────────────────────

/// A photo being exported: 8 bits per channel, or 16 for a 16-bit TIFF.
enum Pixels {
    Eight(RgbImage),
    Sixteen(Rgb16Image),
}

impl Pixels {
    fn from_dynamic(img: DynamicImage, sixteen_bit: bool) -> Self {
        if sixteen_bit {
            Pixels::Sixteen(img.into_rgb16())
        } else {
            Pixels::Eight(img.into_rgb8())
        }
    }

    fn dimensions(&self) -> (u32, u32) {
        match self {
            Pixels::Eight(img) => img.dimensions(),
            Pixels::Sixteen(img) => img.dimensions(),
        }
    }

    fn resized(self, resize: &ExportResize) -> Result<Self> {
        let (w, h) = self.dimensions();
        let Some((dw, dh)) = target_size(w, h, resize)? else { return Ok(self) };
        Ok(match self {
            Pixels::Eight(img) => Pixels::Eight(
                RgbImage::from_raw(dw, dh, resize_samples(img.as_raw(), (w, h), (dw, dh))?).context("buffer mismatch")?,
            ),
            Pixels::Sixteen(img) => Pixels::Sixteen(
                Rgb16Image::from_raw(dw, dh, resize_samples(img.as_raw(), (w, h), (dw, dh))?).context("buffer mismatch")?,
            ),
        })
    }

    fn sharpened(self, mode: Sharpening) -> Self {
        match self {
            Pixels::Eight(img) => Pixels::Eight(apply_sharpening(img, mode)),
            Pixels::Sixteen(img) => {
                let (w, h) = img.dimensions();
                Pixels::Sixteen(Rgb16Image::from_raw(w, h, unsharp(img.as_raw(), (w, h), mode)).expect("same size"))
            }
        }
    }

    fn add_watermark(&mut self, wm: &WatermarkConfig, space: ColorSpace) -> Result<()> {
        let (w, h) = self.dimensions();
        match self {
            Pixels::Eight(img) => apply_watermark_in(img, (w, h), wm, space),
            Pixels::Sixteen(img) => apply_watermark_in(img, (w, h), wm, space),
        }
    }

    fn into_rgb8(self) -> RgbImage {
        match self {
            Pixels::Eight(img) => img,
            Pixels::Sixteen(img) => {
                let (w, h) = img.dimensions();
                let bytes = img.into_raw().into_iter().map(|v| ((v as u32 * 255 + 32767) / 65535) as u8).collect();
                RgbImage::from_raw(w, h, bytes).expect("same size")
            }
        }
    }
}

/// A colour channel an export is processed in: 8 or 16 bits.
pub trait Channel: bytemuck::Pod + Default + Send + Sync + 'static {
    const PIXEL_TYPE: PixelType;
    const MAX: f32;
    fn to_f32(self) -> f32;
    /// Rounded and clamped to the channel's range.
    fn from_f32(v: f32) -> Self;
}

impl Channel for u8 {
    const PIXEL_TYPE: PixelType = PixelType::U8x3;
    const MAX: f32 = 255.0;
    fn to_f32(self) -> f32 {
        self as f32
    }
    fn from_f32(v: f32) -> Self {
        v.round().clamp(0.0, 255.0) as u8
    }
}

impl Channel for u16 {
    const PIXEL_TYPE: PixelType = PixelType::U16x3;
    const MAX: f32 = 65535.0;
    fn to_f32(self) -> f32 {
        self as f32
    }
    fn from_f32(v: f32) -> Self {
        v.round().clamp(0.0, 65535.0) as u16
    }
}

/// The size `w`×`h` shrinks to under `resize`, or `None` if it already fits
/// (exports are never enlarged).
fn target_size(w: u32, h: u32, resize: &ExportResize) -> Result<Option<(u32, u32)>> {
    if w == 0 || h == 0 {
        bail!("Zero dimensions");
    }
    let scale = match *resize {
        ExportResize::Original => return Ok(None),
        ExportResize::FitLongEdge(max) => max as f64 / w.max(h) as f64,
        ExportResize::FitBoundingBox(max_w, max_h) => (max_w as f64 / w as f64).min(max_h as f64 / h as f64),
    };
    if scale >= 1.0 {
        return Ok(None);
    }
    let dw = ((w as f64 * scale).round() as u32).max(1);
    let dh = ((h as f64 * scale).round() as u32).max(1);
    Ok(Some((dw, dh)))
}

/// Interleaved RGB `samples` of size `from` resampled to size `to`.
fn resize_samples<C: Channel>(samples: &[C], from: (u32, u32), to: (u32, u32)) -> Result<Vec<C>> {
    let bytes: Vec<u8> = bytemuck::cast_slice(samples).to_vec();
    let src_fir = FirImage::from_vec_u8(from.0, from.1, bytes, C::PIXEL_TYPE)?;
    let mut dst_fir = FirImage::new(to.0, to.1, C::PIXEL_TYPE);
    Resizer::new().resize(&src_fir, &mut dst_fir, &ResizeOptions::default())?;
    Ok(dst_fir
        .buffer()
        .chunks_exact(std::mem::size_of::<C>())
        .map(bytemuck::pod_read_unaligned)
        .collect())
}

/// Apply unsharp mask sharpening (Screen or Print).
pub fn apply_sharpening(src: RgbImage, mode: Sharpening) -> RgbImage {
    let (w, h) = src.dimensions();
    RgbImage::from_raw(w, h, unsharp(src.as_raw(), (w, h), mode)).expect("same size")
}

/// Interleaved RGB `src` of size `(w, h)` with an unsharp mask applied.
fn unsharp<C: Channel>(src: &[C], (w, h): (u32, u32), mode: Sharpening) -> Vec<C> {
    let amount = match mode {
        Sharpening::None => return src.to_vec(),
        Sharpening::Screen => 0.5f32,
        Sharpening::Print => 1.0f32,
    };
    if w < 3 || h < 3 {
        return src.to_vec();
    }
    let (w, h) = (w as usize, h as usize);
    let at = |x: usize, y: usize, c: usize| (y * w + x) * 3 + c;

    // Horizontal pass: 1 2 1
    let mut temp = vec![0f32; w * h * 3];
    for y in 0..h {
        for x in 0..w {
            let (x_prev, x_next) = (x.saturating_sub(1), (x + 1).min(w - 1));
            for c in 0..3 {
                let p0 = src[at(x_prev, y, c)].to_f32();
                let p1 = src[at(x, y, c)].to_f32();
                let p2 = src[at(x_next, y, c)].to_f32();
                temp[at(x, y, c)] = (p0 + 2.0 * p1 + p2) * 0.25;
            }
        }
    }

    // Vertical pass: 1 2 1, then the unsharp mask: orig + amount * (orig - blur)
    let mut out = vec![C::default(); src.len()];
    for y in 0..h {
        let (y_prev, y_next) = (y.saturating_sub(1), (y + 1).min(h - 1));
        for x in 0..w {
            for c in 0..3 {
                let blur = (temp[at(x, y_prev, c)] + 2.0 * temp[at(x, y, c)] + temp[at(x, y_next, c)]) * 0.25;
                let orig = src[at(x, y, c)].to_f32();
                out[at(x, y, c)] = C::from_f32(orig + amount * (orig - blur));
            }
        }
    }
    out
}

/// Apply `wm` to an sRGB photo.
pub fn apply_watermark(img: &mut RgbImage, wm: &WatermarkConfig) -> Result<()> {
    let size = img.dimensions();
    apply_watermark_in(img, size, wm, ColorSpace::Srgb)
}

/// Apply `wm` to interleaved RGB `samples` of size `(w, h)` in `space`.
fn apply_watermark_in<C: Channel>(samples: &mut [C], (w, h): (u32, u32), wm: &WatermarkConfig, space: ColorSpace) -> Result<()> {
    let target_w = ((w as f32 * wm.scale.clamp(0.05, 1.0)).round() as u32).max(1);

    let mark = if let Some(path) = &wm.image_path {
        image::open(path).with_context(|| format!("Reading {}", path.display()))?.into_rgba8()
    } else if let Some(text) = wm.text.as_deref().filter(|t| !t.trim().is_empty()) {
        render_text(text.trim(), target_w)?
    } else {
        bail!("no watermark text or image");
    };
    let (mw, mh) = mark.dimensions();
    let target_h = ((target_w as f32 / mw as f32) * mh as f32).round().max(1.0) as u32;
    let mut mark = image::imageops::resize(&mark, target_w, target_h, image::imageops::FilterType::Lanczos3);

    // The watermark is drawn in sRGB: bring it into the photo's space.
    if space != ColorSpace::Srgb {
        let mut rgb = RgbImage::from_fn(target_w, target_h, |x, y| {
            let p = mark.get_pixel(x, y);
            Rgb([p[0], p[1], p[2]])
        });
        crate::icc::convert_rgb8(&mut rgb, &lcms2::Profile::new_srgb(), &space.profile());
        for (x, y, p) in rgb.enumerate_pixels() {
            let m = mark.get_pixel_mut(x, y);
            m[0] = p[0];
            m[1] = p[1];
            m[2] = p[2];
        }
    }

    let margin = (w.min(h) as f32 * 0.02).round() as u32;
    let (x0, y0) = match wm.position {
        WatermarkPosition::TopLeft => (margin, margin),
        WatermarkPosition::TopRight => (w.saturating_sub(target_w + margin), margin),
        WatermarkPosition::BottomLeft => (margin, h.saturating_sub(target_h + margin)),
        WatermarkPosition::BottomRight => (w.saturating_sub(target_w + margin), h.saturating_sub(target_h + margin)),
        WatermarkPosition::Center => (w.saturating_sub(target_w) / 2, h.saturating_sub(target_h) / 2),
    };

    let opacity = wm.opacity.clamp(0.0, 1.0);
    for (x, y, p) in mark.enumerate_pixels() {
        let (dx, dy) = (x0 + x, y0 + y);
        if dx >= w || dy >= h {
            continue;
        }
        let alpha = p[3] as f32 / 255.0 * opacity;
        if alpha <= 0.0 {
            continue;
        }
        let i = (dy as usize * w as usize + dx as usize) * 3;
        for c in 0..3 {
            let fg = p[c] as f32 / 255.0 * C::MAX;
            samples[i + c] = C::from_f32(fg * alpha + samples[i + c].to_f32() * (1.0 - alpha));
        }
    }
    Ok(())
}

/// `text` in bold white with a soft dark shadow (legible on light and dark
/// photos), about `width` pixels wide, on a transparent background.
fn render_text(text: &str, width: u32) -> Result<RgbaImage> {
    use cairo::{Context as Cairo, FontSlant, FontWeight, Format, ImageSurface};
    const PROBE_SIZE: f64 = 100.0;
    let set_font = |cr: &Cairo, size: f64| {
        cr.select_font_face("Sans", FontSlant::Normal, FontWeight::Bold);
        cr.set_font_size(size);
    };

    let probe = ImageSurface::create(Format::ARgb32, 1, 1)?;
    let cr = Cairo::new(&probe)?;
    set_font(&cr, PROBE_SIZE);
    let advance = cr.text_extents(text)?.x_advance();
    if advance <= 0.0 {
        bail!("watermark text has no visible characters");
    }
    let size = PROBE_SIZE * width as f64 / advance;
    set_font(&cr, size);
    let ext = cr.text_extents(text)?;
    drop(cr);

    let shadow = (size / 20.0).max(1.0);
    let pad = (shadow * 2.0).ceil();
    let (w, h) = ((ext.width() + 2.0 * pad).ceil() as i32, (ext.height() + 2.0 * pad).ceil() as i32);
    let mut surface = ImageSurface::create(Format::ARgb32, w.max(1), h.max(1))?;
    {
        let cr = Cairo::new(&surface)?;
        set_font(&cr, size);
        let (x, y) = (pad - ext.x_bearing(), pad - ext.y_bearing());
        cr.set_source_rgba(0.0, 0.0, 0.0, 0.55);
        cr.move_to(x + shadow, y + shadow);
        cr.show_text(text)?;
        cr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
        cr.move_to(x, y);
        cr.show_text(text)?;
    }
    surface.flush();

    // Cairo's ARGB32: native-endian words, premultiplied alpha.
    let (w, h) = (surface.width() as u32, surface.height() as u32);
    let stride = surface.stride() as usize;
    let data = surface.data()?;
    Ok(RgbaImage::from_fn(w, h, |x, y| {
        let i = y as usize * stride + x as usize * 4;
        let px = u32::from_ne_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]);
        let a = px >> 24;
        let unmultiply = |c: u32| if a == 0 { 0 } else { ((c * 255 + a / 2) / a).min(255) as u8 };
        image::Rgba([unmultiply((px >> 16) & 0xff), unmultiply((px >> 8) & 0xff), unmultiply(px & 0xff), a as u8])
    }))
}

/// `image` upright at full resolution in `config.color_space`, at 16 bits
/// for a 16-bit TIFF. RAW files go through darktable (with their edits) or
/// Photon's renderer; the embedded camera preview is only a last resort, for
/// cameras neither can decode.
fn load_for_export(image: &Image, config: &ExportConfig) -> Result<(Pixels, Option<String>)> {
    let sixteen = config.format.sixteen_bit();
    let space = config.color_space;
    let is_raw = metadata::format_of(&image.path).is_raw();

    if is_raw && config.raw_renderer == RawRenderer::Darktable && raw::darktable_cli().is_some() {
        let xmp = sidecar::find_xmp(&image.path);
        match raw::render_with_darktable(&image.path, xmp.as_deref(), space, sixteen) {
            // Upright and in `space` already.
            Ok(img) => return Ok((Pixels::from_dynamic(img, sixteen), None)),
            Err(e) => log::warn!("{}: {e:#}; using the built-in RAW renderer", image.path.display()),
        }
    }

    let (mut pixels, icc, warning) = if is_raw {
        let orientation = thumbnails::orientation_of(image);
        let developed = if sixteen {
            raw::develop16(&image.path).map(|img| Pixels::Sixteen(thumbnails::apply_orientation(img, orientation)))
        } else {
            raw::develop(&image.path).map(|img| Pixels::Eight(thumbnails::apply_orientation(img, orientation)))
        };
        match developed {
            Ok(pixels) => (pixels, None, None),
            Err(e) => {
                log::warn!("{}: {e:#}; exporting the camera's embedded preview", image.path.display());
                let preview = DynamicImage::ImageRgb8(thumbnails::embedded_preview_upright(image)?);
                let warning = Some("RAW fell back to embedded preview".to_string());
                (Pixels::from_dynamic(preview, sixteen), None, warning)
            }
        }
    } else {
        let (img, icc) = thumbnails::decode_upright(image)?;
        (Pixels::from_dynamic(img, sixteen), icc, None)
    };

    // Untagged pixels and the built-in RAW renderer's are sRGB.
    if icc.is_some() || space != ColorSpace::Srgb {
        let (from, to) = (crate::icc::source_profile(icc.as_deref()), space.profile());
        match &mut pixels {
            Pixels::Eight(img) => crate::icc::convert_rgb8(img, &from, &to),
            Pixels::Sixteen(img) => crate::icc::convert_rgb16(img, &from, &to),
        }
    }
    Ok((pixels, warning))
}

/// The export's path in `config.destination_dir`: named after the template,
/// or prefix + name + suffix, with `_2`, `_3`... added rather than
/// overwriting an existing file.
fn destination_path(image: &Image, config: &ExportConfig) -> PathBuf {
    let ext = config.format.extension();
    let original_stem = image
        .path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| image.filename.clone());

    let out_filename = if let Some(ref tmpl) = config.filename_template {
        apply_filename_template(tmpl, &original_stem, image.created_at, ext)
    } else {
        format!("{}{}{}.{}", config.prefix, original_stem, config.suffix, ext)
    };

    let dest_path = config.destination_dir.join(&out_filename);
    if !dest_path.exists() {
        return dest_path;
    }
    let stem = dest_path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or(original_stem);
    (2usize..)
        .map(|n| config.destination_dir.join(format!("{stem}_{n}.{ext}")))
        .find(|candidate| !candidate.exists())
        .expect("an unused name")
}

fn encode(pixels: Pixels, format: ExportFormat, path: &Path) -> Result<()> {
    let mut file = BufWriter::new(File::create(path)?);
    match (format, pixels) {
        (ExportFormat::Tiff { bit_depth: 16 }, Pixels::Sixteen(img)) => {
            let (w, h) = img.dimensions();
            image::codecs::tiff::TiffEncoder::new(&mut file).write_image(
                bytemuck::cast_slice(img.as_raw()),
                w,
                h,
                ColorType::Rgb16,
            )?;
        }
        (format, pixels) => {
            let img = pixels.into_rgb8();
            let (w, h) = img.dimensions();
            match format {
                ExportFormat::Jpeg { quality } => {
                    JpegEncoder::new_with_quality(&mut file, quality.clamp(1, 100)).encode_image(&img)?
                }
                ExportFormat::Png => PngEncoder::new(&mut file).write_image(img.as_raw(), w, h, ColorType::Rgb8)?,
                ExportFormat::Webp { quality } => {
                    file.write_all(&webp::Encoder::from_rgb(img.as_raw(), w, h).encode(quality as f32))?
                }
                ExportFormat::Tiff { .. } => {
                    image::codecs::tiff::TiffEncoder::new(&mut file).write_image(img.as_raw(), w, h, ColorType::Rgb8)?
                }
            }
        }
    }
    file.into_inner()?;
    Ok(())
}

// ── Metadata ─────────────────────────────────────────────

/// What goes into an export besides its pixels.
struct Embed<'a> {
    /// The photo exported, whose camera metadata is copied.
    source: &'a Path,
    /// The export's colour space, whose profile it is tagged with.
    space: ColorSpace,
    /// Metadata at all (otherwise only the colour profile).
    metadata: bool,
    strip_gps: bool,
    /// The export's pixel size.
    size: (u32, u32),
    /// From the library: stars (0 = none), title, description, keywords.
    rating: i32,
    title: Option<&'a str>,
    description: Option<&'a str>,
    keywords: &'a [String],
}

impl Embed<'_> {
    fn has_library_fields(&self) -> bool {
        self.rating > 0 || self.title.is_some() || self.description.is_some() || !self.keywords.is_empty()
    }
}

/// Tag `out`, a freshly encoded export, with the colour profile of its space
/// and, if asked, its metadata: the source's EXIF, IPTC and XMP — with
/// Orientation=1 (the pixels are upright), the new pixel size, and without
/// GPS if asked — and the library's title, description, keywords and rating.
///
/// exiftool copies from any source (RAW, HEIC, TIFF, ...) into any format.
/// Without it, only a JPEG export gets metadata (a JPEG source's, and the
/// library's fields); other formats get a sidecar. Returns what couldn't be
/// done, for the export report.
fn embed_metadata(out: &Path, format: ExportFormat, e: &Embed) -> Result<Vec<String>> {
    use img_parts::{DynImage, ImageICC};

    let mut warnings = Vec::new();
    let icc = e.space.icc_bytes();
    let is_tiff = matches!(format, ExportFormat::Tiff { .. });

    // img-parts can't write TIFF; exiftool tags those below.
    if !is_tiff {
        let mut img = DynImage::from_bytes(fs::read(out)?.into())?
            .context("export is not a JPEG, PNG or WebP")?;
        img.set_icc_profile(Some(icc.clone().into()));
        let mut bytes = Vec::new();
        img.encoder().write_to(&mut bytes)?;
        fs::write(out, bytes)?;
    }

    if !exiftool_installed() {
        if is_tiff {
            log::info!("{} has no ICC profile: exiftool is not installed", out.display());
        }
        if e.metadata {
            if matches!(format, ExportFormat::Jpeg { .. }) {
                if !embed_jpeg_metadata(out, e)? {
                    warnings.push("Camera metadata not embedded (install exiftool to copy it from this format)".into());
                }
            } else {
                warnings.push("Metadata written to a sidecar (install exiftool to embed it in this format)".into());
            }
        }
        return Ok(warnings);
    }
    if !(e.metadata || is_tiff) {
        return Ok(warnings);
    }

    let mut cmd = std::process::Command::new("exiftool");
    cmd.args(["-q", "-q", "-m", "-overwrite_original", "-charset", "iptc=UTF8"]);
    if e.metadata {
        cmd.arg("-TagsFromFile")
            .arg(e.source)
            .args(["-all:all", "--ICC_Profile:all", "--IFD1:all"]);
        if e.strip_gps {
            cmd.arg("--GPS*");
        }
        if !e.keywords.is_empty() {
            // Otherwise the library's keywords are added to the file's.
            cmd.args(["--XMP-dc:Subject", "--IPTC:Keywords"]);
        }
        cmd.args(["-Orientation#=1", &format!("-ExifImageWidth={}", e.size.0), &format!("-ExifImageHeight={}", e.size.1)]);
        if e.rating > 0 {
            cmd.arg(format!("-XMP-xmp:Rating={}", e.rating));
        }
        if let Some(title) = e.title {
            cmd.arg(format!("-XMP-dc:Title={title}")).arg(format!("-IPTC:ObjectName={title}"));
        }
        if let Some(desc) = e.description {
            cmd.arg(format!("-XMP-dc:Description={desc}"))
                .arg(format!("-IPTC:Caption-Abstract={desc}"))
                .arg(format!("-EXIF:ImageDescription={desc}"));
        }
        for keyword in e.keywords {
            cmd.arg(format!("-XMP-dc:Subject={keyword}")).arg(format!("-IPTC:Keywords={keyword}"));
        }
        if e.title.is_some() || e.description.is_some() || !e.keywords.is_empty() {
            cmd.arg("-IPTC:CodedCharacterSet=UTF8");
        }
    }
    let icc_path = out.with_extension("icc");
    if is_tiff {
        fs::write(&icc_path, &icc)?;
        cmd.arg(format!("-ICC_Profile<={}", icc_path.display()));
    }
    let output = cmd.arg(out).output();
    if is_tiff {
        let _ = fs::remove_file(&icc_path);
    }
    match output {
        Ok(o) if o.status.success() => {}
        Ok(o) => warnings.push(format!(
            "Metadata not embedded: exiftool: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        )),
        Err(err) => warnings.push(format!("Metadata not embedded: exiftool: {err}")),
    }
    Ok(warnings)
}

pub fn exiftool_installed() -> bool {
    static FOUND: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FOUND.get_or_init(|| {
        std::env::var_os("PATH").is_some_and(|path| {
            std::env::split_paths(&path).any(|dir| dir.join("exiftool").is_file())
        })
    })
}

/// Without exiftool: put the metadata into the JPEG export `out` with
/// img-parts. A JPEG source's EXIF (Orientation=1, the new size, GPS blanked
/// if asked), IPTC and XMP are copied; the library's fields are merged into
/// the XMP. Returns false if the source isn't a JPEG, so only the library's
/// fields could be written.
fn embed_jpeg_metadata(out: &Path, e: &Embed) -> Result<bool> {
    use img_parts::jpeg::{markers, Jpeg, JpegSegment};
    const XMP_ID: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";

    let source = fs::read(e.source).ok().and_then(|b| Jpeg::from_bytes(b.into()).ok());
    let (mut exif, mut xmp, mut iptc) = (None, None, None);
    for seg in source.iter().flat_map(|jpeg| jpeg.segments()) {
        let data = seg.contents();
        match seg.marker() {
            markers::APP1 if data.starts_with(b"Exif\0\0") => exif = Some(data.to_vec()),
            markers::APP1 if data.starts_with(XMP_ID) => xmp = String::from_utf8(data[XMP_ID.len()..].to_vec()).ok(),
            markers::APP13 => iptc = Some(data.to_vec()),
            _ => {}
        }
    }

    if let Some(exif) = &mut exif {
        patch_exif(&mut exif[6..], e.size, e.strip_gps);
    }
    if e.strip_gps && xmp.as_deref().is_some_and(|packet| packet.contains("GPS")) {
        xmp = None;
    }
    if e.has_library_fields() {
        // The source's IPTC would contradict the library's title and keywords.
        iptc = None;
        let update = sidecar::XmpUpdate {
            rating: (e.rating > 0).then_some(e.rating),
            title: e.title,
            description: e.description,
            keywords: (!e.keywords.is_empty()).then_some(sidecar::Keywords { tags: e.keywords, known: e.keywords }),
            orientation: xmp.is_some().then_some(1),
            ..Default::default()
        };
        xmp = sidecar::merged_xmp(xmp.as_deref(), &update);
    }
    // A JPEG segment holds at most 64 KiB; bigger XMP would need extended XMP.
    let xmp = xmp.filter(|packet| XMP_ID.len() + packet.len() <= 65533);

    let mut segments = Vec::new();
    if let Some(exif) = exif {
        segments.push(JpegSegment::new_with_contents(markers::APP1, exif.into()));
    }
    if let Some(packet) = xmp {
        let mut data = XMP_ID.to_vec();
        data.extend_from_slice(packet.as_bytes());
        segments.push(JpegSegment::new_with_contents(markers::APP1, data.into()));
    }
    if let Some(iptc) = iptc {
        segments.push(JpegSegment::new_with_contents(markers::APP13, iptc.into()));
    }

    let mut out_jpeg = Jpeg::from_bytes(fs::read(out)?.into())
        .map_err(|err| anyhow::anyhow!("export is not a valid JPEG: {err}"))?;
    let existing = out_jpeg.segments_mut();
    existing.retain(|seg| !matches!(seg.marker(), markers::APP1 | markers::APP13));
    let after_jfif = existing.iter().take_while(|seg| seg.marker() == markers::APP0).count();
    for (i, seg) in segments.into_iter().enumerate() {
        existing.insert(after_jfif + i, seg);
    }
    let mut bytes = Vec::new();
    out_jpeg.encoder().write_to(&mut bytes)?;
    fs::write(out, bytes)?;
    Ok(source.is_some())
}

/// Edit a TIFF-structured EXIF block in place: Orientation=1, the pixel size
/// (`PixelXDimension`/`PixelYDimension`) set to `size`, and with `strip_gps`
/// the GPS IFD emptied, its values zeroed. Malformed offsets are skipped.
fn patch_exif(tiff: &mut [u8], size: (u32, u32), strip_gps: bool) {
    let le = match tiff.get(0..2) {
        Some(b"II") => true,
        Some(b"MM") => false,
        _ => return,
    };
    let get16 = |t: &[u8], o: usize| {
        t.get(o..o + 2).map(|b| if le { u16::from_le_bytes([b[0], b[1]]) } else { u16::from_be_bytes([b[0], b[1]]) })
    };
    let get32 = |t: &[u8], o: usize| {
        t.get(o..o + 4).map(|b| {
            let b = [b[0], b[1], b[2], b[3]];
            if le { u32::from_le_bytes(b) } else { u32::from_be_bytes(b) }
        })
    };
    let put16 = |t: &mut [u8], o: usize, v: u16| t[o..o + 2].copy_from_slice(&if le { v.to_le_bytes() } else { v.to_be_bytes() });
    let put32 = |t: &mut [u8], o: usize, v: u32| t[o..o + 4].copy_from_slice(&if le { v.to_le_bytes() } else { v.to_be_bytes() });
    // (tag, offset) of each whole 12-byte entry of the IFD at `ifd`.
    let entries = |t: &[u8], ifd: usize| -> Vec<(u16, usize)> {
        let n = get16(t, ifd).unwrap_or(0) as usize;
        (0..n)
            .map(|i| ifd + 2 + 12 * i)
            .filter(|&e| e + 12 <= t.len())
            .map(|e| (get16(t, e).unwrap_or(0), e))
            .collect()
    };
    // Bytes per value of each TIFF type.
    let type_size = |ty: u16| match ty {
        1 | 2 | 6 | 7 => 1,
        3 | 8 => 2,
        4 | 9 | 11 => 4,
        5 | 10 | 12 => 8,
        _ => 0,
    };

    let Some(ifd0) = get32(tiff, 4) else { return };
    for (tag, e) in entries(tiff, ifd0 as usize) {
        match tag {
            0x0112 => {
                // SHORT, count 1, value 1
                put16(tiff, e + 2, 3);
                put32(tiff, e + 4, 1);
                put16(tiff, e + 8, 1);
                put16(tiff, e + 10, 0);
            }
            0x8769 => {
                let Some(exif_ifd) = get32(tiff, e + 8) else { continue };
                for (tag, e) in entries(tiff, exif_ifd as usize) {
                    let value = match tag {
                        0xA002 => size.0,
                        0xA003 => size.1,
                        _ => continue,
                    };
                    // A LONG holds any size, and one fits inside the entry.
                    put16(tiff, e + 2, 4);
                    put32(tiff, e + 4, 1);
                    put32(tiff, e + 8, value);
                }
            }
            0x8825 if strip_gps => {
                let Some(gps) = get32(tiff, e + 8).map(|o| o as usize) else { continue };
                for (_, e) in entries(tiff, gps) {
                    let len = type_size(get16(tiff, e + 2).unwrap_or(0)) * get32(tiff, e + 4).unwrap_or(0) as usize;
                    if len > 4 {
                        if let Some(off) = get32(tiff, e + 8).map(|o| o as usize) {
                            if let Some(value) = tiff.get_mut(off..off.saturating_add(len)) {
                                value.fill(0);
                            }
                        }
                    }
                    tiff[e..e + 12].fill(0);
                }
                if gps + 2 <= tiff.len() {
                    put16(tiff, gps, 0);
                }
            }
            _ => {}
        }
    }
}

pub fn apply_filename_template(
    template: &str,
    name: &str,
    created_at: Option<i64>,
    ext: &str,
) -> String {
    let mut res = template.to_string();
    res = res.replace("{name}", name);
    res = res.replace("{ext}", ext);
    if let Some(ts) = created_at {
        if let Some(dt) = chrono::DateTime::from_timestamp(ts, 0) {
            while let Some(start) = res.find("{date:") {
                if let Some(end) = res[start..].find('}') {
                    let fmt = &res[start + 6..start + end];
                    let formatted = dt.format(fmt).to_string();
                    res.replace_range(start..start + end + 1, &formatted);
                } else {
                    break;
                }
            }
            res = res.replace("{date}", &dt.format("%Y%m%d").to_string());
        }
    } else {
        while let Some(start) = res.find("{date:") {
            if let Some(end) = res[start..].find('}') {
                res.replace_range(start..start + end + 1, "");
            } else {
                break;
            }
        }
        res = res.replace("{date}", "");
    }
    if !res.ends_with(&format!(".{ext}")) {
        res.push('.');
        res.push_str(ext);
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exports_jpeg_and_applies_resizing() {
        let dir = tempfile::tempdir().unwrap();
        let src_path = dir.path().join("input.png");

        // Create a 400x200 test image
        let img = image::ImageBuffer::from_fn(400, 200, |x, y| {
            image::Rgb([(x % 255) as u8, (y % 255) as u8, 128])
        });
        img.save(&src_path).unwrap();

        let image_model = Image {
            id: Some(1),
            hash: "testhash".to_string(),
            path: src_path.clone(),
            filename: "input.png".to_string(),
            size_bytes: 1000,
            width: Some(400),
            height: Some(200),
            rating: 5,
            flagged: 1,
            title: Some("Test Title".to_string()),
            orientation: Some(1),
            ..Default::default()
        };

        let export_dir = dir.path().join("out");
        let config = ExportConfig {
            destination_dir: export_dir.clone(),
            format: ExportFormat::Jpeg { quality: 85 },
            resize: ExportResize::FitLongEdge(200),
            prefix: "Web_".to_string(),
            suffix: "_edited".to_string(),
            preserve_metadata: true,
            ..Default::default()
        };

        let out = export_single(&image_model, &config).unwrap();
        assert!(out.exists());
        assert_eq!(out.file_name().unwrap(), "Web_input_edited.jpg");

        // Verify dimensions
        let decoded = image::open(&out).unwrap();
        assert_eq!(decoded.width(), 200);
        assert_eq!(decoded.height(), 100);

        // The library's title is embedded in the file, with or without exiftool.
        let bytes = std::fs::read(&out).unwrap();
        assert!(bytes.windows(10).any(|w| w == b"Test Title"));
        assert!(!out.with_extension("jpg.xmp").exists());
    }

    #[test]
    fn exports_gif_to_jpeg() {
        let dir = tempfile::tempdir().unwrap();
        let src_path = dir.path().join("anim.gif");

        let mut out = Vec::new();
        {
            let mut encoder = image::codecs::gif::GifEncoder::new(&mut out);
            let frame = image::Frame::new(image::RgbaImage::new(100, 50));
            encoder.encode_frame(frame).unwrap();
        }
        std::fs::write(&src_path, &out).unwrap();

        let image_model = Image {
            id: Some(2),
            hash: "gifhash".to_string(),
            path: src_path.clone(),
            filename: "anim.gif".to_string(),
            size_bytes: out.len() as i64,
            width: Some(100),
            height: Some(50),
            orientation: Some(1),
            ..Default::default()
        };

        let export_dir = dir.path().join("out_gif");
        let config = ExportConfig {
            destination_dir: export_dir,
            format: ExportFormat::Jpeg { quality: 80 },
            resize: ExportResize::Original,
            prefix: "".to_string(),
            suffix: "".to_string(),
            preserve_metadata: false,
            ..Default::default()
        };

        let out = export_single(&image_model, &config).unwrap();
        assert!(out.exists());
        let decoded = image::open(&out).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (100, 50));
    }

    #[test]
    fn exports_avif_to_jpeg() {
        let fixture_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        let src_path = fixture_dir.join("sample.avif");
        assert!(src_path.exists());

        let dir = tempfile::tempdir().unwrap();
        let image_model = Image {
            id: Some(3),
            hash: "avifhash".to_string(),
            path: src_path.clone(),
            filename: "sample.avif".to_string(),
            size_bytes: std::fs::metadata(&src_path).unwrap().len() as i64,
            orientation: Some(1),
            ..Default::default()
        };

        let export_dir = dir.path().join("out_avif");
        let config = ExportConfig {
            destination_dir: export_dir,
            format: ExportFormat::Jpeg { quality: 85 },
            resize: ExportResize::FitLongEdge(200),
            prefix: "exp_".to_string(),
            suffix: "".to_string(),
            preserve_metadata: false,
            ..Default::default()
        };

        let out = export_single(&image_model, &config).unwrap();
        assert!(out.exists());
        let decoded = image::open(&out).unwrap();
        assert!(decoded.width() > 0 && decoded.height() > 0);
        assert!(decoded.width().max(decoded.height()) <= 200);
    }

    #[test]
    fn exports_heic_to_jpeg() {
        let fixture_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        let src_path = fixture_dir.join("sample.heic");
        assert!(src_path.exists());

        let dir = tempfile::tempdir().unwrap();
        let image_model = Image {
            id: Some(4),
            hash: "heichash".to_string(),
            path: src_path.clone(),
            filename: "sample.heic".to_string(),
            size_bytes: std::fs::metadata(&src_path).unwrap().len() as i64,
            orientation: Some(1),
            ..Default::default()
        };

        let export_dir = dir.path().join("out_heic");
        let config = ExportConfig {
            destination_dir: export_dir,
            format: ExportFormat::Jpeg { quality: 85 },
            resize: ExportResize::Original,
            prefix: "exp_".to_string(),
            suffix: "".to_string(),
            preserve_metadata: false,
            ..Default::default()
        };

        match export_single(&image_model, &config) {
            Ok(out) => {
                assert!(out.exists());
                let decoded = image::open(&out).unwrap();
                assert!(decoded.width() > 0 && decoded.height() > 0);
            }
            Err(e) => crate::testutil::assert_missing_hevc_decoder("exports_heic_to_jpeg", &format!("{e:#}")),
        }
    }

    #[test]
    fn filename_template_applies_variables() {
        let ts = 1718452800; // 2024-06-15 12:00:00 UTC
        let out = apply_filename_template("{date:%Y%m%d}_{name}.{ext}", "IMG_001", Some(ts), "jpg");
        assert_eq!(out, "20240615_IMG_001.jpg");
    }

    #[test]
    fn export_does_not_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let src_path = dir.path().join("test.png");
        let img = image::ImageBuffer::from_fn(10, 10, |_, _| image::Rgb([10u8, 20u8, 30u8]));
        img.save(&src_path).unwrap();

        let image_model = Image {
            id: Some(10),
            hash: "h10".to_string(),
            path: src_path.clone(),
            filename: "test.png".to_string(),
            size_bytes: 100,
            ..Default::default()
        };

        let config = ExportConfig {
            destination_dir: dir.path().join("exports"),
            format: ExportFormat::Jpeg { quality: 80 },
            resize: ExportResize::Original,
            prefix: "".to_string(),
            suffix: "".to_string(),
            preserve_metadata: false,
            ..Default::default()
        };

        let first = export_single(&image_model, &config).unwrap();
        let second = export_single(&image_model, &config).unwrap();
        assert_ne!(first, second);
        assert!(first.exists());
        assert!(second.exists());
        assert!(second.file_name().unwrap().to_string_lossy().contains("_2"));
    }

    #[test]
    fn webp_export_writes_valid_webp() {
        let dir = tempfile::tempdir().unwrap();
        let src_path = dir.path().join("test_webp.png");
        let img = image::ImageBuffer::from_fn(20, 20, |_, _| image::Rgb([50u8, 100u8, 150u8]));
        img.save(&src_path).unwrap();

        let image_model = Image {
            id: Some(11),
            hash: "h11".to_string(),
            path: src_path.clone(),
            filename: "test_webp.png".to_string(),
            size_bytes: 200,
            ..Default::default()
        };

        let config = ExportConfig {
            destination_dir: dir.path().join("exports_webp"),
            format: ExportFormat::Webp { quality: 80 },
            resize: ExportResize::Original,
            prefix: "".to_string(),
            suffix: "".to_string(),
            preserve_metadata: false,
            ..Default::default()
        };

        let out = export_single(&image_model, &config).unwrap();
        assert!(out.exists());
        let data = std::fs::read(&out).unwrap();
        // WebP RIFF header check: RIFF....WEBP
        assert!(data.len() > 12);
        assert_eq!(&data[0..4], b"RIFF");
        assert_eq!(&data[8..12], b"WEBP");
    }

    /// A TIFF-format EXIF block (as stored in JPEG APP1 / PNG eXIf) saying
    /// the camera is "TestCam", the photo is rotated (orientation 6) and was
    /// taken at 51°30' N.
    fn exif_block() -> Vec<u8> {
        use exif::{experimental::Writer, Field, In, Tag, Value};
        let orientation = Field { tag: Tag::Orientation, ifd_num: In::PRIMARY, value: Value::Short(vec![6]) };
        let model = Field { tag: Tag::Model, ifd_num: In::PRIMARY, value: Value::Ascii(vec![b"TestCam".to_vec()]) };
        let rational = |v: &[(u32, u32)]| Value::Rational(v.iter().map(|&(num, denom)| exif::Rational { num, denom }).collect());
        let lat_ref = Field { tag: Tag::GPSLatitudeRef, ifd_num: In::PRIMARY, value: Value::Ascii(vec![b"N".to_vec()]) };
        let lat = Field { tag: Tag::GPSLatitude, ifd_num: In::PRIMARY, value: rational(&[(51, 1), (30, 1), (0, 1)]) };
        let width = Field { tag: Tag::PixelXDimension, ifd_num: In::PRIMARY, value: Value::Short(vec![40]) };
        let height = Field { tag: Tag::PixelYDimension, ifd_num: In::PRIMARY, value: Value::Short(vec![20]) };
        let mut writer = Writer::new();
        writer.push_field(&width);
        writer.push_field(&height);
        writer.push_field(&orientation);
        writer.push_field(&model);
        writer.push_field(&lat_ref);
        writer.push_field(&lat);
        let mut out = std::io::Cursor::new(Vec::new());
        writer.write(&mut out, false).unwrap();
        out.into_inner()
    }

    /// A 40×20 photo at `path` (a .jpg or .png) carrying `exif_block()`.
    fn source_with_exif(path: &std::path::Path) -> Image {
        use img_parts::{DynImage, ImageEXIF};
        let img = image::ImageBuffer::from_fn(40, 20, |_, _| image::Rgb([200u8, 100u8, 50u8]));
        img.save(path).unwrap();
        let mut parts = DynImage::from_bytes(std::fs::read(path).unwrap().into()).unwrap().unwrap();
        parts.set_exif(Some(exif_block().into()));
        let mut bytes = Vec::new();
        parts.encoder().write_to(&mut bytes).unwrap();
        std::fs::write(path, bytes).unwrap();
        Image {
            id: Some(12),
            hash: "h12".to_string(),
            path: path.to_path_buf(),
            filename: path.file_name().unwrap().to_string_lossy().into_owned(),
            orientation: Some(6),
            ..Default::default()
        }
    }

    fn exif_of(path: &std::path::Path) -> Option<exif::Exif> {
        let mut reader = std::io::BufReader::new(std::fs::File::open(path).unwrap());
        exif::Reader::new().read_from_container(&mut reader).ok()
    }

    fn has_icc(path: &std::path::Path) -> bool {
        use img_parts::{DynImage, ImageICC};
        DynImage::from_bytes(std::fs::read(path).unwrap().into())
            .unwrap()
            .and_then(|img| img.icc_profile())
            .is_some()
    }

    #[test]
    fn jpeg_export_carries_source_exif_upright_with_srgb_profile() {
        let dir = tempfile::tempdir().unwrap();
        let src = source_with_exif(&dir.path().join("src.jpg"));
        let config = ExportConfig {
            destination_dir: dir.path().join("out"),
            format: ExportFormat::Jpeg { quality: 85 },
            preserve_metadata: true,
            ..Default::default()
        };

        let out = export_single(&src, &config).unwrap();

        let exif = exif_of(&out).expect("export has EXIF");
        let orientation = exif.get_field(exif::Tag::Orientation, exif::In::PRIMARY).unwrap();
        assert_eq!(orientation.value.get_uint(0), Some(1));
        let model = exif.get_field(exif::Tag::Model, exif::In::PRIMARY).unwrap();
        assert_eq!(model.display_value().to_string(), "\"TestCam\"");
        assert!(has_icc(&out));
        // Upright: the rotated 40×20 source comes out portrait.
        assert_eq!(image::image_dimensions(&out).unwrap(), (20, 40));
    }

    #[test]
    fn non_jpeg_source_metadata_is_copied_with_exiftool_or_reported() {
        let dir = tempfile::tempdir().unwrap();
        let src = source_with_exif(&dir.path().join("src.png"));
        let config = ExportConfig {
            destination_dir: dir.path().join("out"),
            format: ExportFormat::Jpeg { quality: 85 },
            preserve_metadata: true,
            ..Default::default()
        };

        let (out, warning) = export_single_with_warning(&src, &[], &config).unwrap();

        assert!(has_icc(&out));
        if exiftool_installed() {
            assert_eq!(warning, None);
            let exif = exif_of(&out).expect("export has EXIF");
            let model = exif.get_field(exif::Tag::Model, exif::In::PRIMARY).unwrap();
            assert_eq!(model.display_value().to_string(), "\"TestCam\"");
            let orientation = exif.get_field(exif::Tag::Orientation, exif::In::PRIMARY).unwrap();
            assert_eq!(orientation.value.get_uint(0), Some(1));
        } else {
            assert!(warning.unwrap().contains("exiftool"));
        }
    }

    fn embed_for<'a>(source: &'a std::path::Path, keywords: &'a [String]) -> Embed<'a> {
        Embed {
            source,
            space: ColorSpace::Srgb,
            metadata: true,
            strip_gps: false,
            size: (20, 40),
            rating: 0,
            title: None,
            description: None,
            keywords,
        }
    }

    #[test]
    fn jpeg_metadata_without_exiftool_is_upright_resized_and_keeps_the_profile() {
        let dir = tempfile::tempdir().unwrap();
        let src = source_with_exif(&dir.path().join("src.jpg"));
        let config = ExportConfig { destination_dir: dir.path().join("out"), preserve_metadata: false, ..Default::default() };
        let out = export_single(&src, &config).unwrap();
        assert!(exif_of(&out).is_none());

        let keywords = vec!["kw-one".to_string(), "kw-two".to_string()];
        let embed = Embed { title: Some("Fallback Title"), ..embed_for(&src.path, &keywords) };
        assert!(embed_jpeg_metadata(&out, &embed).unwrap());

        let exif = exif_of(&out).expect("export has EXIF");
        let uint = |tag| exif.get_field(tag, exif::In::PRIMARY).and_then(|f| f.value.get_uint(0));
        assert_eq!(uint(exif::Tag::Orientation), Some(1));
        assert_eq!(uint(exif::Tag::PixelXDimension), Some(20));
        assert_eq!(uint(exif::Tag::PixelYDimension), Some(40));
        assert!(has_icc(&out));
        let bytes = std::fs::read(&out).unwrap();
        for text in [&b"Fallback Title"[..], b"kw-one", b"kw-two"] {
            assert!(bytes.windows(text.len()).any(|w| w == text), "{}", String::from_utf8_lossy(text));
        }

        // From a source that isn't a JPEG, only the library's fields.
        let png = dir.path().join("not-a-jpeg.png");
        image::RgbImage::new(2, 2).save(&png).unwrap();
        assert!(!embed_jpeg_metadata(&out, &embed_for(&png, &[])).unwrap());
    }

    #[test]
    fn gps_is_stripped_with_and_without_exiftool() {
        let dir = tempfile::tempdir().unwrap();
        let src = source_with_exif(&dir.path().join("gps.jpg"));
        let lat = |path: &std::path::Path| {
            exif_of(path).and_then(|e| e.get_field(exif::Tag::GPSLatitude, exif::In::PRIMARY).map(|f| f.display_value().to_string()))
        };
        assert!(lat(&src.path).is_some(), "fixture has GPS");

        for strip_gps in [false, true] {
            let config = ExportConfig {
                destination_dir: dir.path().join(format!("out-{strip_gps}")),
                strip_gps,
                ..Default::default()
            };
            let out = export_single(&src, &config).unwrap();
            assert_eq!(lat(&out).is_some(), !strip_gps, "exiftool path, strip_gps={strip_gps}");
        }

        let config = ExportConfig { destination_dir: dir.path().join("bare"), preserve_metadata: false, ..Default::default() };
        let out = export_single(&src, &config).unwrap();
        let embed = Embed { strip_gps: true, ..embed_for(&src.path, &[]) };
        embed_jpeg_metadata(&out, &embed).unwrap();
        assert!(exif_of(&out).is_some());
        assert_eq!(lat(&out), None, "img-parts path");
        let model = exif_of(&out).unwrap().get_field(exif::Tag::Model, exif::In::PRIMARY).map(|f| f.display_value().to_string());
        assert_eq!(model.as_deref(), Some("\"TestCam\""), "the rest of the EXIF survives");
    }

    #[test]
    fn library_title_and_keywords_replace_the_files_own() {
        if !exiftool_installed() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let mut src = source_with_exif(&dir.path().join("kw.jpg"));
        src.title = Some("Library Title".into());
        src.rating = 4;
        let keywords = vec!["alpha".to_string(), "beta".to_string()];
        let config = ExportConfig { destination_dir: dir.path().join("out"), ..Default::default() };
        let (out, warning) = export_single_with_warning(&src, &keywords, &config).unwrap();
        assert_eq!(warning, None);

        let output = std::process::Command::new("exiftool")
            .args(["-s3", "-XMP-dc:Title", "-XMP-dc:Subject", "-IPTC:Keywords", "-XMP-xmp:Rating"])
            .arg(&out)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&output.stdout);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines, ["Library Title", "alpha, beta", "alpha, beta", "4"]);
    }

    #[test]
    fn srgb_profile_is_embedded_even_without_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let src = source_with_exif(&dir.path().join("src.jpg"));
        for format in [ExportFormat::Jpeg { quality: 85 }, ExportFormat::Png, ExportFormat::Webp { quality: 80 }] {
            let config = ExportConfig {
                destination_dir: dir.path().join("out"),
                format,
                preserve_metadata: false,
                ..Default::default()
            };
            let out = export_single(&src, &config).unwrap();
            assert!(has_icc(&out), "{format:?} export has no ICC profile");
            assert!(exif_of(&out).is_none(), "{format:?} export kept EXIF it was told to drop");
        }
    }

    #[test]
    fn tiff_export_writes_valid_tiff() {
        let dir = tempfile::tempdir().unwrap();
        let src_path = dir.path().join("test_tiff.png");
        let img = image::ImageBuffer::from_fn(16, 16, |_, _| image::Rgb([100u8, 150u8, 200u8]));
        img.save(&src_path).unwrap();

        let image_model = Image {
            id: Some(13),
            hash: "h13".to_string(),
            path: src_path.clone(),
            filename: "test_tiff.png".to_string(),
            size_bytes: 256,
            ..Default::default()
        };

        let config = ExportConfig {
            destination_dir: dir.path().join("exports_tiff"),
            format: ExportFormat::Tiff { bit_depth: 8 },
            resize: ExportResize::Original,
            prefix: "".to_string(),
            suffix: "".to_string(),
            preserve_metadata: false,
            ..Default::default()
        };

        let out = export_single(&image_model, &config).unwrap();
        assert!(out.exists());
        assert_eq!(out.extension().unwrap(), "tif");
        let data = std::fs::read(&out).unwrap();
        assert!(data.len() > 4);
        // TIFF magic: II*\0 or MM\0*
        let is_tiff = &data[0..4] == b"II*\0" || &data[0..4] == b"MM\0*";
        assert!(is_tiff, "Expected valid TIFF header");
    }

    #[test]
    fn sharpening_modifies_pixel_values_at_edges() {
        // Create an image with an edge: left half black, right half white
        let mut img = image::RgbImage::new(10, 10);
        for y in 0..10 {
            for x in 0..10 {
                let v = if x >= 5 { 200u8 } else { 50u8 };
                img.put_pixel(x, y, image::Rgb([v, v, v]));
            }
        }
        let sharpened = apply_sharpening(img.clone(), Sharpening::Screen);
        // The pixels near the edge should have accentuated contrast
        assert_ne!(img, sharpened);
    }

    #[test]
    fn watermark_blends_into_image() {
        let dir = tempfile::tempdir().unwrap();
        let wm_path = dir.path().join("wm.png");
        let wm_img = image::ImageBuffer::from_fn(10, 10, |_, _| image::Rgba([255u8, 0u8, 0u8, 255u8]));
        wm_img.save(&wm_path).unwrap();

        let mut base_img = image::ImageBuffer::from_fn(100, 100, |_, _| image::Rgb([0u8, 0u8, 0u8]));
        let config = WatermarkConfig {
            text: None,
            image_path: Some(wm_path),
            opacity: 1.0,
            scale: 0.1,
            ..Default::default()
        };
        apply_watermark(&mut base_img, &config).unwrap();
        // Base image was all black, watermark added red pixels
        let has_red = base_img.pixels().any(|p| p[0] > 0);
        assert!(has_red);
    }

    #[test]
    fn sixteen_bit_tiff_keeps_more_than_eight_bits() {
        let dir = tempfile::tempdir().unwrap();
        let src_path = dir.path().join("gradient16.png");
        let gradient = image::ImageBuffer::from_fn(256, 4, |x, _| image::Rgb([x as u16 * 256 + 77, 30000u16, 1000u16]));
        DynamicImage::ImageRgb16(gradient).save(&src_path).unwrap();
        let src = Image { path: src_path, filename: "gradient16.png".into(), ..Default::default() };

        let config = ExportConfig {
            destination_dir: dir.path().join("out"),
            format: ExportFormat::Tiff { bit_depth: 16 },
            preserve_metadata: false,
            ..Default::default()
        };
        let out = export_single(&src, &config).unwrap();

        let decoded = image::open(&out).unwrap();
        assert_eq!(decoded.color(), ColorType::Rgb16);
        let px = decoded.into_rgb16();
        // 8-bit data widened to 16 bits would only hold multiples of 257.
        assert!(px.pixels().any(|p| p[0] % 257 != 0));
        assert!(px.pixels().zip(px.pixels().skip(1)).take(255).all(|(a, b)| a[0] < b[0]));
    }

    #[test]
    fn exports_are_converted_to_and_tagged_with_the_chosen_space() {
        let dir = tempfile::tempdir().unwrap();
        let src_path = dir.path().join("red.png");
        image::RgbImage::from_pixel(32, 32, Rgb([255, 0, 0])).save(&src_path).unwrap();
        let src = Image { path: src_path, filename: "red.png".into(), ..Default::default() };

        for (space, expected) in [(ColorSpace::Srgb, [255, 0, 0]), (ColorSpace::AdobeRgb, [219, 0, 0]), (ColorSpace::DisplayP3, [234, 51, 35])] {
            let config = ExportConfig {
                destination_dir: dir.path().join(format!("{space:?}")),
                format: ExportFormat::Png,
                color_space: space,
                preserve_metadata: false,
                ..Default::default()
            };
            let out = export_single(&src, &config).unwrap();

            let icc = crate::icc::extract_png_icc(&out).expect("tagged");
            let profile = lcms2::Profile::new_icc(&icc).unwrap();
            let expected_desc = lcms2::Profile::new_icc(&space.icc_bytes()).unwrap()
                .info(lcms2::InfoType::Description, lcms2::Locale::none());
            assert_eq!(profile.info(lcms2::InfoType::Description, lcms2::Locale::none()), expected_desc);

            let px = image::open(&out).unwrap().into_rgb8().get_pixel(16, 16).0;
            for c in 0..3 {
                assert!(px[c].abs_diff(expected[c]) <= 6, "{space:?}: {px:?}, expected about {expected:?}");
            }
        }
    }

    #[test]
    fn text_watermark_lands_where_asked() {
        let bright = |img: &RgbImage, x0: u32, y0: u32| {
            (x0..x0 + 100).flat_map(|x| (y0..y0 + 50).map(move |y| (x, y))).filter(|&(x, y)| img.get_pixel(x, y)[0] > 128).count()
        };
        for (position, corner) in [(WatermarkPosition::BottomRight, (100, 50)), (WatermarkPosition::TopLeft, (0, 0))] {
            let mut img = RgbImage::new(200, 100);
            let wm = WatermarkConfig { text: Some("© Photon".into()), opacity: 1.0, scale: 0.4, position, ..Default::default() };
            apply_watermark(&mut img, &wm).unwrap();
            let opposite = (100 - corner.0, 50 - corner.1);
            assert!(bright(&img, corner.0, corner.1) > 50, "{position:?}: no text in its corner");
            assert_eq!(bright(&img, opposite.0, opposite.1), 0, "{position:?}: text in the opposite corner");
        }
    }

    #[test]
    fn config_without_new_fields_still_loads() {
        // A preset saved before colour spaces, GPS stripping and watermark positions.
        let json = r#"{"format":{"Jpeg":{"quality":85}},"resize":{"FitLongEdge":2048},"sharpening":"Screen",
                       "watermark":{"text":"x","opacity":0.5,"scale":0.1}}"#;
        let config: ExportConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.format, ExportFormat::Jpeg { quality: 85 });
        assert_eq!(config.color_space, ColorSpace::Srgb);
        assert!(config.preserve_metadata && !config.strip_gps);
        assert_eq!(config.watermark.unwrap().position, WatermarkPosition::BottomRight);
    }

    #[test]
    fn default_presets_parse() {
        let db = photon_core::db::Database::open_in_memory().unwrap();
        let conn = db.conn().unwrap();
        let presets = photon_core::db::queries::get_export_presets(&conn).unwrap();
        assert_eq!(presets.len(), 3);
        for (_, name, json) in presets {
            let config: ExportConfig = serde_json::from_str(&json).unwrap_or_else(|e| panic!("{name}: {e}"));
            if name == "Print TIFF" {
                assert_eq!(config.format, ExportFormat::Tiff { bit_depth: 16 });
                assert_eq!(config.color_space, ColorSpace::AdobeRgb);
            }
        }
    }
}
