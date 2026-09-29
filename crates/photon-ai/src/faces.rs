//! Face detection with YuNet (OpenCV Zoo, MIT), and how sharp a face is.
//!
//! YuNet 2023mar takes a 1×3×640×640 BGR image (0–255, no normalisation) and
//! returns, for strides 8, 16 and 32, one row per grid cell: `cls_*` and
//! `obj_*` scores, `bbox_*` (centre offset in cells, log size in cells) and
//! `kps_*` (5 landmarks in cells: two eyes, nose tip, two mouth corners).
//! The decoding follows OpenCV's `FaceDetectorYN`.

use crate::backend::{Device, InferenceBackend, LoadedModel, Tensor};
use crate::manifest::ModelManifest;
use crate::store::{ModelStatus, ModelStore};
use anyhow::{bail, Context, Result};
use image::{imageops::FilterType, GrayImage, RgbImage};

/// The model's input size.
const SIZE: usize = 640;
const STRIDES: [usize; 3] = [8, 16, 32];
/// Faces smaller than this (in the 640 px input) are too small to judge.
const MIN_FACE: f32 = 24.0;
const NMS_IOU: f32 = 0.3;
/// Detections below this confidence are ignored (OpenCV's default is 0.9;
/// babies and turned heads score lower, and 0.6 kept every real face in
/// testing without false ones).
pub const MIN_SCORE: f32 = 0.6;
/// The eye region is scaled to this width before measuring, so a face's
/// size in the frame doesn't change its score.
const EYE_REGION_WIDTH: u32 = 160;

/// A face, in the pixels of the image given to [`FaceDetector::detect`].
#[derive(Debug, Clone, PartialEq)]
pub struct Face {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub score: f32,
    /// Eye, eye, nose tip, mouth corner, mouth corner.
    pub landmarks: [(f32, f32); 5],
}

pub struct FaceDetector {
    model: Box<dyn LoadedModel>,
    /// Index of each `cls_s, obj_s, bbox_s, kps_s` output, per stride.
    outputs: [[usize; 4]; 3],
}

impl FaceDetector {
    /// Wrap a loaded YuNet model, checking it is one.
    pub fn new(model: Box<dyn LoadedModel>) -> Result<Self> {
        if model.input_shapes().first() != Some(&vec![1, 3, SIZE, SIZE]) {
            bail!("not a YuNet 640×640 model (input {:?})", model.input_shapes());
        }
        let names = model.output_names();
        let find = |name: String| names.iter().position(|n| *n == name).with_context(|| format!("no output {name}"));
        let mut outputs = [[0; 4]; 3];
        for (i, stride) in STRIDES.iter().enumerate() {
            for (j, kind) in ["cls", "obj", "bbox", "kps"].iter().enumerate() {
                outputs[i][j] = find(format!("{kind}_{stride}"))?;
            }
        }
        Ok(Self { model, outputs })
    }

    /// Faces in `img` scoring at least `min_score` (0–1), best first.
    pub fn detect(&mut self, img: &RgbImage, min_score: f32) -> Result<Vec<Face>> {
        // Letterbox: scale the long edge to 640, pad the rest with black.
        let scale = SIZE as f32 / img.width().max(img.height()) as f32;
        let (w, h) = ((img.width() as f32 * scale).round() as u32, (img.height() as f32 * scale).round() as u32);
        let resized = image::imageops::resize(img, w.max(1), h.max(1), FilterType::Triangle);
        let mut data = vec![0.0f32; 3 * SIZE * SIZE];
        for (x, y, px) in resized.enumerate_pixels() {
            let i = y as usize * SIZE + x as usize;
            // BGR, as OpenCV feeds it.
            data[i] = px[2] as f32;
            data[SIZE * SIZE + i] = px[1] as f32;
            data[2 * SIZE * SIZE + i] = px[0] as f32;
        }
        let out = self.model.run(&[Tensor::new(vec![1, 3, SIZE, SIZE], data)])?;

        let mut faces = Vec::new();
        for (level, &stride) in STRIDES.iter().enumerate() {
            let [cls, obj, bbox, kps] = self.outputs[level].map(|i| &out[i].data);
            let cols = SIZE / stride;
            for idx in 0..cols * cols {
                let score = (cls[idx].clamp(0.0, 1.0) * obj[idx].clamp(0.0, 1.0)).sqrt();
                if score < min_score {
                    continue;
                }
                let (col, row, s) = ((idx % cols) as f32, (idx / cols) as f32, stride as f32);
                let cx = (col + bbox[idx * 4]) * s;
                let cy = (row + bbox[idx * 4 + 1]) * s;
                let (bw, bh) = (bbox[idx * 4 + 2].exp() * s, bbox[idx * 4 + 3].exp() * s);
                if bw < MIN_FACE || bh < MIN_FACE {
                    continue;
                }
                let lm = |k: usize| ((col + kps[idx * 10 + 2 * k]) * s / scale, (row + kps[idx * 10 + 2 * k + 1]) * s / scale);
                faces.push(Face {
                    x: (cx - bw / 2.0) / scale,
                    y: (cy - bh / 2.0) / scale,
                    w: bw / scale,
                    h: bh / scale,
                    score,
                    landmarks: [lm(0), lm(1), lm(2), lm(3), lm(4)],
                });
            }
        }
        Ok(non_max_suppression(faces))
    }
}

/// The face model's entry in the manifest.
pub fn face_spec() -> Option<crate::manifest::ModelSpec> {
    ModelManifest::load_embedded().models.into_iter().find(|m| m.task == "face-detection")
}

/// Whether the face model is downloaded (and intact). Cheap: hashes a
/// quarter-megabyte file.
pub fn model_ready(store: &ModelStore) -> bool {
    face_spec().is_some_and(|spec| matches!(store.status(&spec), ModelStatus::Ready { .. }))
}

/// The face detector from `store`, on the first of the model's preferred
/// devices that `backend` can use. `Ok(None)` when the model isn't downloaded.
pub fn load_detector(store: &ModelStore, backend: &dyn InferenceBackend) -> Result<Option<(FaceDetector, Device)>> {
    let Some(spec) = face_spec() else { return Ok(None) };
    if !matches!(store.status(&spec), ModelStatus::Ready { .. }) {
        return Ok(None);
    }
    let path = store.verify(&spec)?;
    let usable: Vec<Device> = backend.devices().into_iter().filter(|d| d.available).map(|d| d.device).collect();
    let device = spec
        .preferred_devices
        .iter()
        .filter_map(|name| match name.as_str() {
            "cpu" => Some(Device::Cpu),
            "gpu" => Some(Device::Gpu),
            "npu" => Some(Device::Npu),
            _ => None,
        })
        .find(|d| usable.contains(d))
        .context("no usable device for the face model")?;
    Ok(Some((FaceDetector::new(backend.load(&path, device)?)?, device)))
}

/// The eye sharpness of the largest face in `img` that can be judged, and
/// how many faces there are.
pub fn largest_face_eyes(detector: &mut FaceDetector, img: &RgbImage) -> Result<(usize, Option<f64>)> {
    let mut faces = detector.detect(img, MIN_SCORE)?;
    faces.sort_by(|a, b| (b.w * b.h).total_cmp(&(a.w * a.h)));
    let eyes = faces.iter().find_map(|f| eye_sharpness(img, f));
    Ok((faces.len(), eyes))
}

fn iou(a: &Face, b: &Face) -> f32 {
    let (x1, y1) = (a.x.max(b.x), a.y.max(b.y));
    let (x2, y2) = ((a.x + a.w).min(b.x + b.w), (a.y + a.h).min(b.y + b.h));
    let inter = (x2 - x1).max(0.0) * (y2 - y1).max(0.0);
    inter / (a.w * a.h + b.w * b.h - inter).max(f32::EPSILON)
}

/// Keep the best of overlapping detections.
fn non_max_suppression(mut faces: Vec<Face>) -> Vec<Face> {
    faces.sort_by(|a, b| b.score.total_cmp(&a.score));
    let mut kept: Vec<Face> = Vec::new();
    for face in faces {
        if kept.iter().all(|k| iou(k, &face) < NMS_IOU) {
            kept.push(face);
        }
    }
    kept
}

/// How sharp the eyes of `face` are in `img`: the variance of the Laplacian
/// over the region around both eyes, scaled to a fixed width first. `None`
/// when the eyes are too close together (a tiny or side-on face) to judge.
///
/// The eyes, not the whole face: skin (a baby's especially) has little
/// texture even when perfectly sharp, while lashes, lids and catchlights
/// are the finest detail on a face — and where people look for focus.
pub fn eye_sharpness(img: &RgbImage, face: &Face) -> Option<f64> {
    let [(x1, y1), (x2, y2), ..] = face.landmarks;
    let d = ((x2 - x1).powi(2) + (y2 - y1).powi(2)).sqrt();
    if d < 12.0 {
        return None;
    }
    let left = (x1.min(x2) - 0.5 * d).max(0.0);
    let right = (x1.max(x2) + 0.5 * d).min(img.width() as f32);
    let top = (y1.min(y2) - 0.4 * d).max(0.0);
    let bottom = (y1.max(y2) + 0.4 * d).min(img.height() as f32);
    let (w, h) = ((right - left) as u32, (bottom - top) as u32);
    if w < 8 || h < 4 {
        return None;
    }
    let crop = image::imageops::crop_imm(img, left as u32, top as u32, w, h).to_image();
    let out_h = ((h as f32 / w as f32) * EYE_REGION_WIDTH as f32).round().max(3.0) as u32;
    let gray: GrayImage = image::imageops::grayscale(&image::imageops::resize(&crop, EYE_REGION_WIDTH, out_h, FilterType::Triangle));
    Some(laplacian_variance(&gray))
}

fn laplacian_variance(img: &GrayImage) -> f64 {
    let (w, h) = img.dimensions();
    let v = |x: u32, y: u32| img.get_pixel(x, y)[0] as f64;
    let (mut sum, mut sum_sq, mut n) = (0.0, 0.0, 0.0);
    for y in 1..h.saturating_sub(1) {
        for x in 1..w.saturating_sub(1) {
            let l = v(x - 1, y) + v(x + 1, y) + v(x, y - 1) + v(x, y + 1) - 4.0 * v(x, y);
            sum += l;
            sum_sq += l * l;
            n += 1.0;
        }
    }
    if n < 2.0 {
        return 0.0;
    }
    (sum_sq / n - (sum / n).powi(2)).max(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn face(x: f32, score: f32) -> Face {
        Face { x, y: 0.0, w: 100.0, h: 100.0, score, landmarks: [(0.0, 0.0); 5] }
    }

    #[test]
    fn overlapping_detections_keep_the_best() {
        let kept = non_max_suppression(vec![face(0.0, 0.7), face(10.0, 0.9), face(300.0, 0.8)]);
        let scores: Vec<f32> = kept.iter().map(|f| f.score).collect();
        assert_eq!(scores, [0.9, 0.8]);
    }

    #[test]
    fn sharp_eyes_score_higher_than_blurred_ones() {
        // Fine stripes around the "eyes" vs. the same, box-blurred.
        let sharp = RgbImage::from_fn(400, 300, |x, _| if (x / 2) % 2 == 0 { image::Rgb([230; 3]) } else { image::Rgb([30; 3]) });
        let blurred = image::imageops::blur(&sharp, 3.0);
        let mut f = face(100.0, 0.9);
        f.landmarks[0] = (150.0, 150.0);
        f.landmarks[1] = (250.0, 150.0);
        let (s, b) = (eye_sharpness(&sharp, &f).unwrap(), eye_sharpness(&blurred, &f).unwrap());
        assert!(b < s * 0.2, "sharp {s}, blurred {b}");

        // Eyes 5 px apart: too small to judge.
        f.landmarks[1] = (155.0, 150.0);
        assert_eq!(eye_sharpness(&sharp, &f), None);
    }
}
