//! Image quality scoring and burst analysis (AI-7).
//!
//! Classical quality analysis computes sharpness, exposure clipping, and mean luminance
//! from a photo's Large preview (downscaled to a fixed 1024 px long edge).
//!
//! Burst detection groups consecutive photos (<= 2s gap, same camera model).
//! Suggest rejects identifies blurry frames within bursts and heavily clipped/unsharp images.

use fast_image_resize::images::Image as FirImage;
use fast_image_resize::{PixelType, ResizeOptions, Resizer};
use image::{ImageBuffer, RgbImage};
use photon_core::models::ImageQuality;
use std::collections::HashMap;

/// Standard long edge for quality analysis (ensures resolution independence).
pub const QUALITY_LONG_EDGE: u32 = 1024;

/// Version of the classical quality algorithm. Bump it whenever scores
/// change meaning: every photo is then re-scored.
/// 2: highlights = luma only (a saturated colour isn't clipping); scores
///    only from the Large preview (1 also used thumbnails and originals).
pub const QUALITY_VERSION: i32 = 2;

/// Ratio threshold in a burst: frames scoring less than 60% of the sharpest in the burst
/// are suggested as rejects.
pub const BURST_SHARPNESS_REJECT_RATIO: f64 = 0.60;

/// A photo outside a burst is suggested as blurred below this fraction of
/// the library's median sharpness. Tuned 2026-09-29 on a 611-photo library
/// (Olympus E-M10 II, JPG+ORF): median 1348; the clearly motion-blurred
/// shots scored 22–34, the next ones up 73 and 119. 0.08 → ~108.
pub const BLUR_RATIO_OF_LIBRARY_MEDIAN: f64 = 0.08;

/// A shot whose largest face's eyes are below this fraction of its
/// session's reference eye sharpness is suggested as blurred. Tuned
/// 2026-09-29 on 8 shots of one session (a baby, E-M10 II): eye scores
/// 32/48/49/55 were the blurred or soft ones, 79–129 the sharp ones;
/// reference (80th percentile) 116 → threshold 58.
pub const EYE_BLUR_RATIO: f64 = 0.5;
/// A session: the same camera, within this many seconds.
pub const SESSION_SECONDS: i64 = 30 * 60;
/// Fewer face shots than this in a session: compare with the library instead.
pub const MIN_SESSION_FACES: usize = 3;

/// The same, in absolute terms, while there is no library median (nothing
/// scored yet).
pub const BLUR_SHARPNESS_FLOOR: f64 = 60.0;

/// Highlight clipping threshold: photos with > 25% blown pixels are flagged.
pub const HIGHLIGHT_CLIP_REJECT_THRESHOLD: f64 = 0.25;

/// Shadow clipping threshold: photos with > 35% deep shadow pixels are flagged.
pub const SHADOW_CLIP_REJECT_THRESHOLD: f64 = 0.35;

/// Scores computed for an image.
#[derive(Debug, Clone, PartialEq)]
pub struct QualityScore {
    /// Maximum sharpness among 4x4 tiles (variance of Laplacian).
    pub sharpness: f64,
    /// Global sharpness across the entire image.
    pub sharpness_global: f64,
    /// Fraction of pixels with luma <= 2.0.
    pub clip_shadows: f64,
    /// Fraction of pixels with luma >= 253.0 or any RGB channel == 255.
    pub clip_highlights: f64,
    /// Mean luma across the image (0..255).
    pub mean_luma: f64,
    pub quality_version: i32,
}

impl QualityScore {
    pub fn to_model(&self, image_id: i64, computed_at: i64) -> ImageQuality {
        ImageQuality {
            image_id,
            sharpness: self.sharpness,
            sharpness_global: self.sharpness_global,
            clip_shadows: self.clip_shadows,
            clip_highlights: self.clip_highlights,
            mean_luma: self.mean_luma,
            quality_version: self.quality_version,
            computed_at,
            faces: None,
            eye_sharpness: None,
        }
    }
}

/// Downscale an RGB image to a fixed 1024 px long edge if necessary, preserving aspect ratio.
pub fn downscale_to_quality_size(img: &RgbImage) -> RgbImage {
    let (w, h) = img.dimensions();
    let long_edge = w.max(h);
    if long_edge <= QUALITY_LONG_EDGE {
        return img.clone();
    }

    let scale = QUALITY_LONG_EDGE as f64 / long_edge as f64;
    let target_w = ((w as f64 * scale).round() as u32).max(1);
    let target_h = ((h as f64 * scale).round() as u32).max(1);

    // Can't fail for a valid RgbImage; if it ever does, a plain (slower)
    // resize beats a panic in the backfill worker.
    let fallback = || image::imageops::resize(img, target_w, target_h, image::imageops::FilterType::Triangle);
    let Ok(src) = FirImage::from_vec_u8(w, h, img.as_raw().clone(), PixelType::U8x3) else {
        return fallback();
    };
    let mut dst = FirImage::new(target_w, target_h, PixelType::U8x3);
    if Resizer::new().resize(&src, &mut dst, &ResizeOptions::new()).is_err() {
        return fallback();
    }
    ImageBuffer::from_raw(target_w, target_h, dst.into_vec()).unwrap_or_else(fallback)
}

/// Compute quality scores for an RGB image (downscales to 1024 px long edge if needed).
pub fn compute_quality(img: &RgbImage) -> QualityScore {
    let resized = downscale_to_quality_size(img);
    let (w, h) = resized.dimensions();
    let total_pixels = (w * h) as f64;
    if total_pixels == 0.0 {
        return QualityScore {
            sharpness: 0.0,
            sharpness_global: 0.0,
            clip_shadows: 0.0,
            clip_highlights: 0.0,
            mean_luma: 0.0,
            quality_version: QUALITY_VERSION,
        };
    }

    // 1. Compute luma buffer Y = 0.299*R + 0.587*G + 0.114*B and statistics
    let raw = resized.as_raw();
    let mut luma = Vec::with_capacity((w * h) as usize);
    let mut sum_luma = 0.0;
    let mut shadow_pixels = 0usize;
    let mut highlight_pixels = 0usize;

    for chunk in raw.chunks_exact(3) {
        let r = chunk[0] as f64;
        let g = chunk[1] as f64;
        let b = chunk[2] as f64;
        let y = 0.299 * r + 0.587 * g + 0.114 * b;
        luma.push(y);
        sum_luma += y;

        if y <= 2.0 {
            shadow_pixels += 1;
        }
        // Luma, not any channel: a saturated red or blue has a channel at
        // 255 without being blown out.
        if y >= 253.0 {
            highlight_pixels += 1;
        }
    }

    let mean_luma = sum_luma / total_pixels;
    let clip_shadows = shadow_pixels as f64 / total_pixels;
    let clip_highlights = highlight_pixels as f64 / total_pixels;

    if w < 3 || h < 3 {
        return QualityScore {
            sharpness: 0.0,
            sharpness_global: 0.0,
            clip_shadows,
            clip_highlights,
            mean_luma,
            quality_version: QUALITY_VERSION,
        };
    }

    // 2. Compute Laplacian on interior pixels: lap = Y(x+1, y) + Y(x-1, y) + Y(x, y+1) + Y(x, y-1) - 4*Y(x, y)
    // and compute variance in 4x4 tiles as well as globally.
    const GRID: usize = 4;
    let tile_w = w as f64 / GRID as f64;
    let tile_h = h as f64 / GRID as f64;

    #[derive(Clone, Copy)]
    struct Accum {
        sum: f64,
        sum_sq: f64,
        count: usize,
    }

    impl Accum {
        fn new() -> Self {
            Self { sum: 0.0, sum_sq: 0.0, count: 0 }
        }
        fn add(&mut self, val: f64) {
            self.sum += val;
            self.sum_sq += val * val;
            self.count += 1;
        }
        fn variance(&self) -> f64 {
            if self.count < 2 {
                0.0
            } else {
                let n = self.count as f64;
                let mean = self.sum / n;
                ((self.sum_sq / n) - (mean * mean)).max(0.0)
            }
        }
    }

    let mut tile_acc = vec![vec![Accum::new(); GRID]; GRID];
    let mut global_acc = Accum::new();

    let stride = w as usize;
    for y in 1..(h - 1) as usize {
        let row_curr = y * stride;
        let row_prev = (y - 1) * stride;
        let row_next = (y + 1) * stride;

        let ty = ((y as f64 / tile_h).floor() as usize).min(GRID - 1);

        for x in 1..(w - 1) as usize {
            let tx = ((x as f64 / tile_w).floor() as usize).min(GRID - 1);

            let center = luma[row_curr + x];
            let left = luma[row_curr + x - 1];
            let right = luma[row_curr + x + 1];
            let up = luma[row_prev + x];
            let down = luma[row_next + x];

            let lap = left + right + up + down - 4.0 * center;

            global_acc.add(lap);
            tile_acc[ty][tx].add(lap);
        }
    }

    let sharpness_global = global_acc.variance();
    let mut max_tile_sharpness = 0.0f64;
    for row in 0..GRID {
        for col in 0..GRID {
            let var = tile_acc[row][col].variance();
            if var > max_tile_sharpness {
                max_tile_sharpness = var;
            }
        }
    }

    QualityScore {
        sharpness: max_tile_sharpness,
        sharpness_global,
        clip_shadows,
        clip_highlights,
        mean_luma,
        quality_version: QUALITY_VERSION,
    }
}

// ---------------------------------------------------------------------------
// Bursts and Rejection Suggestions (§4.4)
// ---------------------------------------------------------------------------

/// Trait representing an item that can be checked for burst grouping.
pub trait BurstItem {
    fn item_id(&self) -> i64;
    fn created_at(&self) -> Option<i64>;
    fn camera_model(&self) -> Option<&str>;
    fn flagged(&self) -> i32;
    /// Files of one shot (RAW + JPG, edits) share this; they are one frame,
    /// not a burst of two.
    fn shot_key(&self) -> Option<&str> {
        None
    }
}

impl BurstItem for photon_core::models::Image {
    fn item_id(&self) -> i64 {
        self.id.unwrap_or(0)
    }
    fn created_at(&self) -> Option<i64> {
        self.created_at
    }
    fn camera_model(&self) -> Option<&str> {
        self.camera_model.as_deref()
    }
    fn flagged(&self) -> i32 {
        self.flagged
    }
    fn shot_key(&self) -> Option<&str> {
        self.group_hash.as_deref()
    }
}

impl BurstItem for photon_core::models::TimelineItem {
    fn item_id(&self) -> i64 {
        self.id
    }
    fn created_at(&self) -> Option<i64> {
        self.created_at
    }
    fn camera_model(&self) -> Option<&str> {
        None
    }
    fn flagged(&self) -> i32 {
        self.flagged
    }
}

fn same_shot<T: BurstItem>(a: &T, b: &T) -> bool {
    matches!((a.shot_key(), b.shot_key()), (Some(x), Some(y)) if x == y)
}

/// A burst of consecutive photos.
#[derive(Debug, Clone, PartialEq)]
pub struct Burst<T> {
    /// Every file of every shot in the burst.
    pub items: Vec<T>,
}

/// Group photos into bursts: consecutive shots (by `created_at`) from the
/// same camera with ≤ 2 s between neighbours, at least two *shots* — the
/// files of one shot (same [`BurstItem::shot_key`]) count once.
pub fn find_bursts<T: BurstItem + Clone>(items: &[T]) -> Vec<Burst<T>> {
    let mut sorted: Vec<T> = items.iter().filter(|i| i.created_at().is_some()).cloned().collect();
    sorted.sort_by_key(|i| (i.created_at(), i.item_id()));

    let mut bursts = Vec::new();
    let mut current: Vec<T> = Vec::new();
    let mut shots = 0;
    let mut flush = |current: &mut Vec<T>, shots: usize| {
        if shots >= 2 {
            bursts.push(Burst { items: std::mem::take(current) });
        } else {
            current.clear();
        }
    };
    for item in sorted {
        match current.last() {
            None => shots = 1,
            Some(last) if current.iter().any(|c| same_shot(c, &item)) || same_shot(last, &item) => {}
            Some(last) => {
                let dt = item.created_at().unwrap_or(0) - last.created_at().unwrap_or(0);
                if (0..=2).contains(&dt) && item.camera_model() == last.camera_model() {
                    shots += 1;
                } else {
                    flush(&mut current, shots);
                    shots = 1;
                }
            }
        }
        current.push(item);
    }
    flush(&mut current, shots);
    bursts
}

/// A rejection suggestion for an image.
#[derive(Debug, Clone, PartialEq)]
pub struct RejectSuggestion {
    pub image_id: i64,
    pub burst_id: Option<usize>,
    pub is_burst_best: bool,
    pub burst_relative_percent: Option<f64>,
    pub reason: String,
}

/// The files of `items` grouped by shot, in order.
fn shots<T: BurstItem>(items: &[T]) -> Vec<Vec<&T>> {
    let mut groups: Vec<Vec<&T>> = Vec::new();
    for item in items {
        match groups.iter_mut().find(|g| same_shot(g[0], item)) {
            Some(group) => group.push(item),
            None => groups.push(vec![item]),
        }
    }
    groups
}

/// What "sharp" means in this library, for photos without enough context
/// of their own (standalone, or a small session).
#[derive(Debug, Clone, Copy, Default)]
pub struct LibraryReference {
    /// Median whole-frame sharpness.
    pub sharpness_median: Option<f64>,
    /// 80th percentile of eye sharpness over photos with faces.
    pub eye_p80: Option<f64>,
}

/// The library's reference values, from the scores in the database. Errors
/// are logged and leave that value unknown (the rules then fall back).
pub fn library_reference(conn: &rusqlite::Connection) -> LibraryReference {
    use photon_core::db::queries;
    LibraryReference {
        sharpness_median: queries::library_sharpness_median(conn, QUALITY_VERSION).unwrap_or_else(|e| {
            log::warn!("Library sharpness median: {e}");
            None
        }),
        eye_p80: queries::library_eye_sharpness(conn, QUALITY_VERSION)
            .map(|values| percentile(values, 0.8))
            .unwrap_or_else(|e| {
                log::warn!("Library eye sharpness: {e}");
                None
            }),
    }
}

/// How sharp a photo is compared with what is sharp for it, in percent.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RelativeSharpness {
    pub percent: f64,
    /// Measured on the eyes (a face was found), else on the whole frame.
    pub by_eyes: bool,
    /// Compared with its own session's sharp shots, else the library's.
    pub in_session: bool,
}

/// A quality grade for display: green, yellow, orange, red.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grade {
    Good,
    Fair,
    Poor,
    /// Would be suggested for rejection.
    Bad,
}

/// The session's reference eye sharpness (80th percentile of its face
/// shots), when it has enough of them.
pub fn session_eye_reference(values: Vec<f64>) -> Option<f64> {
    (values.len() >= MIN_SESSION_FACES).then(|| percentile(values, 0.8)).flatten()
}

/// `q` against the references the suggestions use: eyes against the
/// session's (or the library's) sharp portraits; otherwise the whole frame
/// against the library's median. `None` when there is nothing to compare with.
pub fn relative_sharpness(
    q: &ImageQuality,
    session_eye_reference: Option<f64>,
    library: &LibraryReference,
) -> Option<RelativeSharpness> {
    if let Some(eyes) = q.eye_sharpness {
        let reference = session_eye_reference.or(library.eye_p80).filter(|&r| r > 0.0);
        if let Some(reference) = reference {
            return Some(RelativeSharpness {
                percent: eyes / reference * 100.0,
                by_eyes: true,
                in_session: session_eye_reference.is_some(),
            });
        }
    }
    let median = library.sharpness_median.filter(|&m| m > 0.0)?;
    Some(RelativeSharpness { percent: q.sharpness / median * 100.0, by_eyes: false, in_session: false })
}

/// The grade of a relative sharpness. Red starts exactly where a reject is
/// suggested ([`EYE_BLUR_RATIO`], [`BLUR_RATIO_OF_LIBRARY_MEDIAN`]), so the
/// colour and the Photo Quality results agree; the steps above it are
/// spread over the range each measure really takes (whole-frame sharpness
/// varies far more between good photos than eye sharpness does).
pub fn grade(s: &RelativeSharpness) -> Grade {
    let bad = if s.by_eyes { EYE_BLUR_RATIO } else { BLUR_RATIO_OF_LIBRARY_MEDIAN } * 100.0;
    let (poor, fair) = if s.by_eyes { (65.0, 80.0) } else { (20.0, 50.0) };
    match s.percent {
        p if p < bad => Grade::Bad,
        p if p < poor => Grade::Poor,
        p if p < fair => Grade::Fair,
        _ => Grade::Good,
    }
}

/// The `p`-th percentile (0–1) of `values`.
pub fn percentile(mut values: Vec<f64>, p: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let idx = ((values.len() - 1) as f64 * p).round() as usize;
    Some(values[idx.min(values.len() - 1)])
}

/// Suggest photos to reject. Judged per **shot**: a shot's score is its best
/// file's (the RAW and the JPG of one shot score alike), and a suggestion
/// covers every file of the shot, so no blurred copy is left behind.
///
/// - Never a shot with a pick, nor one already rejected.
/// - In a burst: shots below 60 % of the burst's sharpest; clipped shots,
///   except the burst's best (in a bright scene every frame clips).
/// - Otherwise: below [`BLUR_RATIO_OF_LIBRARY_MEDIAN`] × `library_median`
///   (the median sharpness of the whole library, not of the photos looked at:
///   a small selection has no useful distribution of its own), or clipped.
pub fn suggest_rejects<T: BurstItem + Clone>(
    items: &[T],
    scores: &HashMap<i64, ImageQuality>,
    library: LibraryReference,
) -> Vec<RejectSuggestion> {
    let library_median = library.sharpness_median;
    let score_of = |shot: &[&T]| -> Option<ImageQuality> {
        shot.iter()
            .filter_map(|i| scores.get(&i.item_id()))
            .max_by(|a, b| a.sharpness.total_cmp(&b.sharpness))
            .cloned()
    };
    let skip = |shot: &[&T]| shot.iter().any(|i| i.flagged() != 0);
    let mut suggestions = Vec::new();
    let mut push = |shot: &[&T], burst_id: Option<usize>, pct: Option<f64>, reason: String| {
        for item in shot {
            suggestions.push(RejectSuggestion {
                image_id: item.item_id(),
                burst_id,
                is_burst_best: false,
                burst_relative_percent: pct,
                reason: reason.clone(),
            });
        }
    };

    let mut in_burst = std::collections::HashSet::new();
    for (burst_idx, burst) in find_bursts(items).iter().enumerate() {
        in_burst.extend(burst.items.iter().map(|i| i.item_id()));
        let burst_shots = shots(&burst.items);
        let scored: Vec<(&Vec<&T>, ImageQuality)> =
            burst_shots.iter().filter_map(|s| score_of(s).map(|q| (s, q))).collect();
        // A burst of the same face is judged by the eyes: the background's
        // detail says nothing about whether the subject moved.
        let by_eyes = !scored.is_empty() && scored.iter().all(|(_, q)| q.eye_sharpness.is_some());
        let value = |q: &ImageQuality| if by_eyes { q.eye_sharpness.unwrap_or(0.0) } else { q.sharpness };
        let Some(best) = scored.iter().map(|(_, q)| value(q)).max_by(f64::total_cmp) else { continue };
        for (shot, q) in &scored {
            if skip(shot) {
                continue;
            }
            let is_best = value(q) >= best;
            let pct = if best > 0.0 { Some(value(q) / best * 100.0) } else { None };
            if !is_best && best > 0.0 && value(q) < BURST_SHARPNESS_REJECT_RATIO * best {
                let what = if by_eyes { "eyes blurred" } else { "blurred" };
                push(shot, Some(burst_idx), pct, format!("{what} — {:.0}% of the sharpest in its burst", pct.unwrap_or(0.0)));
            } else if !is_best && q.clip_highlights > HIGHLIGHT_CLIP_REJECT_THRESHOLD {
                push(shot, Some(burst_idx), pct, format!("highlights clipped ({:.0}%)", q.clip_highlights * 100.0));
            }
        }
    }

    // Eye sharpness of every shot with a face, for session references.
    let face_shots: Vec<(i64, Option<&str>, f64)> = shots(items)
        .iter()
        .filter_map(|shot| {
            let eyes = score_of(shot)?.eye_sharpness?;
            Some((shot[0].created_at()?, shot[0].camera_model(), eyes))
        })
        .collect();
    let eye_reference = |shot: &[&T]| -> Option<(f64, bool)> {
        let (t, cam) = (shot[0].created_at()?, shot[0].camera_model());
        let session: Vec<f64> = face_shots
            .iter()
            .filter(|(ft, fcam, _)| *fcam == cam && (ft - t).abs() <= SESSION_SECONDS)
            .map(|&(_, _, e)| e)
            .collect();
        if session.len() >= MIN_SESSION_FACES {
            percentile(session, 0.8).map(|r| (r, true))
        } else {
            library.eye_p80.map(|r| (r, false))
        }
    };

    let blur_below = library_median.map_or(BLUR_SHARPNESS_FLOOR, |m| m * BLUR_RATIO_OF_LIBRARY_MEDIAN);
    let outside: Vec<T> = items.iter().filter(|i| !in_burst.contains(&i.item_id())).cloned().collect();
    for shot in shots(&outside) {
        if skip(&shot) {
            continue;
        }
        let Some(q) = score_of(&shot) else { continue };
        let eyes_blurred = q
            .eye_sharpness
            .zip(eye_reference(&shot))
            .filter(|&(eyes, (reference, _))| reference > 0.0 && eyes < EYE_BLUR_RATIO * reference);
        if let Some((eyes, (reference, in_session))) = eyes_blurred {
            let of = if in_session { "the sharp ones of this session" } else { "your sharp portraits" };
            push(&shot, None, None, format!("eyes blurred — {:.0}% of {of}", eyes / reference * 100.0));
        } else if q.sharpness < blur_below {
            let reason = match library_median {
                Some(m) if m > 0.0 => {
                    format!("blurred — sharpness {:.0}% of your typical photo", q.sharpness / m * 100.0)
                }
                _ => format!("blurred (sharpness {:.0})", q.sharpness),
            };
            push(&shot, None, None, reason);
        } else if q.clip_highlights > HIGHLIGHT_CLIP_REJECT_THRESHOLD {
            push(&shot, None, None, format!("highlights clipped ({:.0}%)", q.clip_highlights * 100.0));
        } else if q.clip_shadows > SHADOW_CLIP_REJECT_THRESHOLD {
            push(&shot, None, None, format!("shadows clipped ({:.0}%)", q.clip_shadows * 100.0));
        }
    }
    suggestions
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgb;

    #[test]
    fn checkerboard_scores_higher_than_blurred() {
        let size = 256;
        let mut sharp_img = RgbImage::new(size, size);
        for y in 0..size {
            for x in 0..size {
                let v = if ((x / 16) + (y / 16)) % 2 == 0 { 240 } else { 20 };
                sharp_img.put_pixel(x, y, Rgb([v, v, v]));
            }
        }

        let sharp_score = compute_quality(&sharp_img);

        // Create blurred version via box blur
        let mut blurred_img = RgbImage::new(size, size);
        for y in 2..size - 2 {
            for x in 2..size - 2 {
                let mut sum = 0u32;
                let mut count = 0u32;
                for dy in -2..=2 {
                    for dx in -2..=2 {
                        sum += sharp_img.get_pixel((x as i32 + dx) as u32, (y as i32 + dy) as u32)[0] as u32;
                        count += 1;
                    }
                }
                let avg = (sum / count) as u8;
                blurred_img.put_pixel(x, y, Rgb([avg, avg, avg]));
            }
        }

        let blur_score = compute_quality(&blurred_img);

        assert!(sharp_score.sharpness > 100.0);
        assert!(blur_score.sharpness < sharp_score.sharpness * 0.2);
    }

    #[test]
    fn pure_black_and_white_clipping() {
        let size = 128;
        let mut black_img = RgbImage::new(size, size);
        for p in black_img.pixels_mut() {
            *p = Rgb([0, 0, 0]);
        }
        let black_score = compute_quality(&black_img);
        assert!((black_score.clip_shadows - 1.0).abs() < 1e-6);
        assert_eq!(black_score.clip_highlights, 0.0);
        assert_eq!(black_score.mean_luma, 0.0);

        let mut white_img = RgbImage::new(size, size);
        for p in white_img.pixels_mut() {
            *p = Rgb([255, 255, 255]);
        }
        let white_score = compute_quality(&white_img);
        assert!((white_score.clip_highlights - 1.0).abs() < 1e-6);
        assert_eq!(white_score.clip_shadows, 0.0);
        assert!((white_score.mean_luma - 255.0).abs() < 1e-6);
    }

    #[test]
    fn single_tile_sharpness_stays_high() {
        let size = 256;
        let mut img = RgbImage::new(size, size);
        // Fill with uniform gray 128
        for p in img.pixels_mut() {
            *p = Rgb([128, 128, 128]);
        }
        // Place checkerboard in one tile only (e.g. tile row 1, col 1: x 64..128, y 64..128)
        for y in 64..128 {
            for x in 64..128 {
                let v = if ((x / 4) + (y / 4)) % 2 == 0 { 240 } else { 20 };
                img.put_pixel(x, y, Rgb([v, v, v]));
            }
        }

        let score = compute_quality(&img);
        // Max tile sharpness must be high, while global sharpness is much lower
        assert!(score.sharpness > 500.0);
        assert!(score.sharpness_global < score.sharpness * 0.3);
    }

    #[derive(Clone, Debug)]
    struct DummyItem {
        id: i64,
        created: Option<i64>,
        cam: Option<String>,
        flag: i32,
    }

    impl BurstItem for DummyItem {
        fn item_id(&self) -> i64 { self.id }
        fn created_at(&self) -> Option<i64> { self.created }
        fn camera_model(&self) -> Option<&str> { self.cam.as_deref() }
        fn flagged(&self) -> i32 { self.flag }
    }

    /// Files of shots: the RAW and JPG of one shot share `shot`.
    #[derive(Clone)]
    struct File {
        id: i64,
        created: i64,
        shot: &'static str,
    }

    impl BurstItem for File {
        fn item_id(&self) -> i64 { self.id }
        fn created_at(&self) -> Option<i64> { Some(self.created) }
        fn camera_model(&self) -> Option<&str> { Some("E-M10MarkII") }
        fn flagged(&self) -> i32 { 0 }
        fn shot_key(&self) -> Option<&str> { Some(self.shot) }
    }

    #[test]
    fn a_raw_jpg_pair_is_one_shot_not_a_burst() {
        // The real case: P1010646.JPG/.ORF, motion-blurred, among sharp shots.
        let files = [
            File { id: 113, created: 1704122012, shot: "b9df" }, // blurred JPG
            File { id: 105, created: 1704122012, shot: "b9df" }, // its ORF
            File { id: 119, created: 1704122077, shot: "b3ab" },
            File { id: 121, created: 1704122077, shot: "b3ab" },
        ];
        assert!(find_bursts(&files).is_empty(), "no burst: one shot per timestamp");

        let scores: HashMap<i64, ImageQuality> =
            [scored(113, 23.0, 0.0), scored(105, 23.8, 0.0), scored(119, 814.0, 0.0), scored(121, 845.0, 0.0)]
                .into_iter()
                .map(|q| (q.image_id, q))
                .collect();
        let mut ids: Vec<i64> = suggest_rejects(&files, &scores, LibraryReference { sharpness_median: Some(1348.0), eye_p80: None }).iter().map(|s| s.image_id).collect();
        ids.sort();
        assert_eq!(ids, [105, 113], "both files of the blurred shot, nothing else");

        // Two shots 1 s apart are a burst, whatever their files.
        let burst = [
            File { id: 1, created: 100, shot: "a" },
            File { id: 2, created: 100, shot: "a" },
            File { id: 3, created: 101, shot: "b" },
        ];
        assert_eq!(find_bursts(&burst).len(), 1);
        assert_eq!(find_bursts(&burst)[0].items.len(), 3);
    }

    #[test]
    fn burst_detection_and_edge_cases() {
        let items = vec![
            // Burst 1: items 1, 2, 3 (same cam, dt <= 2)
            DummyItem { id: 1, created: Some(1000), cam: Some("E-M1MarkIII".into()), flag: 0 },
            DummyItem { id: 2, created: Some(1001), cam: Some("E-M1MarkIII".into()), flag: 0 },
            DummyItem { id: 3, created: Some(1002), cam: Some("E-M1MarkIII".into()), flag: 0 },
            // Gap > 2s
            DummyItem { id: 4, created: Some(1010), cam: Some("E-M1MarkIII".into()), flag: 0 },
            // Different cam
            DummyItem { id: 5, created: Some(1011), cam: Some("GR III".into()), flag: 0 },
            // Equal timestamp burst with same cam
            DummyItem { id: 6, created: Some(1020), cam: Some("E-M1MarkIII".into()), flag: 0 },
            DummyItem { id: 7, created: Some(1020), cam: Some("E-M1MarkIII".into()), flag: 0 },
            // Missing date
            DummyItem { id: 8, created: None, cam: Some("E-M1MarkIII".into()), flag: 0 },
        ];

        let bursts = find_bursts(&items);
        assert_eq!(bursts.len(), 2);
        assert_eq!(bursts[0].items.len(), 3);
        assert_eq!(bursts[0].items[0].id, 1);
        assert_eq!(bursts[0].items[2].id, 3);

        assert_eq!(bursts[1].items.len(), 2);
        assert_eq!(bursts[1].items[0].id, 6);
        assert_eq!(bursts[1].items[1].id, 7);
    }

    #[test]
    fn suggest_rejects_in_burst_and_standalone() {
        let items = vec![
            DummyItem { id: 1, created: Some(1000), cam: Some("E-M1".into()), flag: 0 }, // sharpest
            DummyItem { id: 2, created: Some(1001), cam: Some("E-M1".into()), flag: 0 }, // blurry (30%)
            DummyItem { id: 3, created: Some(1002), cam: Some("E-M1".into()), flag: 1 }, // blurry but Pick!
            DummyItem { id: 4, created: Some(2000), cam: Some("E-M1".into()), flag: 0 }, // standalone clipped
        ];

        let mut scores = HashMap::new();
        scores.insert(1, ImageQuality {
            image_id: 1, sharpness: 100.0, sharpness_global: 80.0, clip_shadows: 0.0, clip_highlights: 0.0,
            mean_luma: 120.0, quality_version: 1, computed_at: 0, faces: None, eye_sharpness: None
        });
        scores.insert(2, ImageQuality {
            image_id: 2, sharpness: 30.0, sharpness_global: 20.0, clip_shadows: 0.0, clip_highlights: 0.0,
            mean_luma: 120.0, quality_version: 1, computed_at: 0, faces: None, eye_sharpness: None
        });
        scores.insert(3, ImageQuality {
            image_id: 3, sharpness: 25.0, sharpness_global: 15.0, clip_shadows: 0.0, clip_highlights: 0.0,
            mean_luma: 120.0, quality_version: 1, computed_at: 0, faces: None, eye_sharpness: None
        });
        scores.insert(4, ImageQuality {
            image_id: 4, sharpness: 80.0, sharpness_global: 60.0, clip_shadows: 0.0, clip_highlights: 0.35,
            mean_luma: 200.0, quality_version: 1, computed_at: 0, faces: None, eye_sharpness: None
        });

        let suggestions = suggest_rejects(&items, &scores, LibraryReference::default());
        let suggested_ids: Vec<i64> = suggestions.iter().map(|s| s.image_id).collect();

        assert!(suggested_ids.contains(&2));
        assert!(!suggested_ids.contains(&1)); // best frame
        assert!(!suggested_ids.contains(&3)); // pick is never rejected
        assert!(suggested_ids.contains(&4)); // highlight clipped
    }

    fn scored(id: i64, sharpness: f64, clip_highlights: f64) -> ImageQuality {
        ImageQuality {
            image_id: id, sharpness, sharpness_global: sharpness, clip_shadows: 0.0, clip_highlights,
            mean_luma: 120.0, quality_version: QUALITY_VERSION, computed_at: 0, faces: None, eye_sharpness: None,
        }
    }

    #[test]
    fn saturated_colour_is_not_clipping() {
        let red = RgbImage::from_pixel(64, 64, Rgb([255, 30, 30]));
        assert_eq!(compute_quality(&red).clip_highlights, 0.0);
        let white = RgbImage::from_pixel(64, 64, Rgb([255, 255, 255]));
        assert_eq!(compute_quality(&white).clip_highlights, 1.0);
    }

    #[test]
    fn rejected_photos_and_the_best_clipped_frame_are_not_suggested() {
        let cam = || Some("E-M1".to_string());
        // A snow burst: every frame clipped; frame 2 already rejected, and blurred.
        let items = vec![
            DummyItem { id: 1, created: Some(1000), cam: cam(), flag: 0 },
            DummyItem { id: 2, created: Some(1001), cam: cam(), flag: -1 },
            DummyItem { id: 3, created: Some(1002), cam: cam(), flag: 0 },
        ];
        let scores: HashMap<i64, ImageQuality> =
            [scored(1, 100.0, 0.4), scored(2, 10.0, 0.4), scored(3, 90.0, 0.4)].into_iter().map(|q| (q.image_id, q)).collect();

        let ids: Vec<i64> = suggest_rejects(&items, &scores, LibraryReference::default()).iter().map(|s| s.image_id).collect();
        assert_eq!(ids, [3], "the other clipped frame only: not the best, not the rejected one");
    }

    #[test]
    fn blurred_eyes_are_found_against_a_sharp_background() {
        // 2024-01-01, one session: whole-frame sharpness is high everywhere
        // (a patterned bedsheet), only the eyes tell the blurred shots apart.
        let session = [
            (646, 1704122012, 32.0),
            (647, 1704122077, 129.0),
            (648, 1704122579, 49.0),
            (649, 1704122585, 100.0),
            (650, 1704122640, 48.0),
            (651, 1704122643, 55.0),
            (652, 1704122646, 116.0),
            (653, 1704122650, 79.0),
        ];
        let items: Vec<DummyItem> = session
            .iter()
            .map(|&(id, t, _)| DummyItem { id, created: Some(t), cam: Some("E-M10MarkII".into()), flag: 0 })
            .collect();
        let scores: HashMap<i64, ImageQuality> = session
            .iter()
            .map(|&(id, _, eyes)| {
                let mut q = scored(id, 2000.0, 0.0);
                q.faces = Some(1);
                q.eye_sharpness = Some(eyes);
                (id, q)
            })
            .collect();
        let reference = LibraryReference { sharpness_median: Some(1348.0), eye_p80: None };

        let mut ids: Vec<i64> = suggest_rejects(&items, &scores, reference).iter().map(|s| s.image_id).collect();
        ids.sort();
        assert_eq!(ids, [646, 648, 650, 651]);

        // Alone (no session), the library's portraits are the reference.
        let one = &items[5..6];
        assert!(suggest_rejects(one, &scores, reference).is_empty(), "no reference: nothing to compare with");
        let with_library = LibraryReference { eye_p80: Some(116.0), ..reference };
        assert_eq!(suggest_rejects(one, &scores, with_library).len(), 1);
    }

    #[test]
    fn grades_follow_the_reject_thresholds() {
        let library = LibraryReference { sharpness_median: Some(1348.0), eye_p80: Some(116.0) };
        let eyes = |e: f64, session: Option<f64>| {
            let mut q = scored(1, 2000.0, 0.0);
            q.eye_sharpness = Some(e);
            grade(&relative_sharpness(&q, session, &library).unwrap())
        };
        // The 2024-01-01 session (reference 116): 651 is red, 653 orange...
        assert_eq!(eyes(55.0, Some(116.0)), Grade::Bad);
        assert_eq!(eyes(79.0, Some(116.0)), Grade::Fair);
        assert_eq!(eyes(70.0, Some(116.0)), Grade::Poor);
        assert_eq!(eyes(129.0, Some(116.0)), Grade::Good);
        // ...and 10% vs 50% are different colours.
        assert_ne!(eyes(11.6, Some(116.0)), eyes(58.0, Some(116.0)));

        // No face: the whole frame against the library's median.
        let frame = |s: f64| grade(&relative_sharpness(&scored(1, s, 0.0), None, &library).unwrap());
        assert_eq!(frame(23.0), Grade::Bad, "P1010646");
        assert_eq!(frame(200.0), Grade::Poor);
        assert_eq!(frame(400.0), Grade::Fair);
        assert_eq!(frame(2390.0), Grade::Good);

        assert_eq!(session_eye_reference(vec![10.0, 20.0]), None, "too few face shots");
        assert!(relative_sharpness(&scored(1, 5.0, 0.0), None, &LibraryReference::default()).is_none());
    }
}
