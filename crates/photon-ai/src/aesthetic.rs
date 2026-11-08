//! Aesthetic score (AI-7, learned): how good a photo looks as a whole —
//! composition, light, subject, colour — on a 1–10 scale.
//!
//! Two models: the CLIP ViT-L/14 image encoder (OpenAI, MIT) turns the photo
//! into a 768-value embedding, and the LAION "improved aesthetic predictor"
//! (Apache-2.0), trained on human ratings of photos (AVA, SAC, LAION-Logos),
//! maps that embedding to a mean rating. The head is a PyTorch state dict of
//! five linear layers (no activations); it is read here and evaluated in
//! plain Rust, so it needs neither PyTorch nor a converted copy.
//!
//! The photo is seen as CLIP was trained: shortest edge scaled to 224 px and
//! the centre 224×224 cut out, so what lies near the long edges is not seen.

use crate::backend::{Device, InferenceBackend, LoadedModel, Tensor};
use crate::manifest::{ModelManifest, ModelSpec};
use crate::store::{ModelStatus, ModelStore};
use anyhow::{bail, ensure, Context, Result};
use image::{imageops::FilterType, RgbImage};
use std::path::Path;

/// CLIP's input size.
pub const SIZE: usize = 224;
/// Length of a CLIP ViT-L/14 image embedding.
pub const EMBEDDING_LEN: usize = 768;
const MEAN: [f32; 3] = [0.481_454_7, 0.457_827_5, 0.408_210_7];
const STD: [f32; 3] = [0.268_629_5, 0.261_302_6, 0.275_777_1];
/// The head's linear layers, `(inputs, outputs)`, in order.
const LAYERS: [(usize, usize); 5] = [(768, 1024), (1024, 128), (128, 64), (64, 16), (16, 1)];

/// The CLIP image encoder's entry in the manifest.
pub fn embedding_spec() -> Option<ModelSpec> {
    ModelManifest::load_embedded().models.into_iter().find(|m| m.task == "image-embedding")
}

/// The aesthetic head's entry in the manifest.
pub fn head_spec() -> Option<ModelSpec> {
    ModelManifest::load_embedded().models.into_iter().find(|m| m.task == "aesthetic-head")
}

/// Both files the aesthetic score needs, encoder first.
pub fn specs() -> Vec<ModelSpec> {
    embedding_spec().into_iter().chain(head_spec()).collect()
}

/// Whether both models are downloaded and intact. Hashes a file only the
/// first time after it changed (see `ModelStore::status`).
pub fn model_ready(store: &ModelStore) -> bool {
    let specs = specs();
    specs.len() == 2 && specs.iter().all(|s| matches!(store.status(s), ModelStatus::Ready { .. }))
}

/// `img` as CLIP's input: shortest edge to 224 (bicubic, as CLIP's own
/// preprocessing), centre crop, then normalised per channel, as 1×3×224×224.
pub fn preprocess(img: &RgbImage) -> Tensor {
    let (w, h) = img.dimensions();
    let short = w.min(h).max(1) as f32;
    let scale = SIZE as f32 / short;
    let (rw, rh) = (((w as f32 * scale).round() as u32).max(SIZE as u32), ((h as f32 * scale).round() as u32).max(SIZE as u32));
    let resized = image::imageops::resize(img, rw, rh, FilterType::CatmullRom);
    let (left, top) = ((rw - SIZE as u32) / 2, (rh - SIZE as u32) / 2);
    let plane = SIZE * SIZE;
    let mut data = vec![0.0f32; 3 * plane];
    for y in 0..SIZE {
        for x in 0..SIZE {
            let px = resized.get_pixel(left + x as u32, top + y as u32);
            for c in 0..3 {
                data[c * plane + y * SIZE + x] = (px[c] as f32 / 255.0 - MEAN[c]) / STD[c];
            }
        }
    }
    Tensor::new(vec![1, 3, SIZE, SIZE], data)
}

/// The LAION aesthetic head: embedding (L2-normalised) → mean rating.
#[derive(Debug, Clone)]
pub struct AestheticHead {
    /// Per layer: weights (`outputs × inputs`, row-major) and biases.
    layers: Vec<(Vec<f32>, Vec<f32>)>,
}

impl AestheticHead {
    /// The head from its layers' weights and biases, checking their sizes.
    pub fn from_layers(layers: Vec<(Vec<f32>, Vec<f32>)>) -> Result<Self> {
        ensure!(layers.len() == LAYERS.len(), "expected {} layers, found {}", LAYERS.len(), layers.len());
        for (i, ((w, b), &(inputs, outputs))) in layers.iter().zip(LAYERS.iter()).enumerate() {
            ensure!(w.len() == inputs * outputs, "layer {i}: {} weights, expected {}", w.len(), inputs * outputs);
            ensure!(b.len() == outputs, "layer {i}: {} biases, expected {outputs}", b.len());
        }
        Ok(Self { layers })
    }

    /// Read the head from the predictor's `.pth` file: a PyTorch zip archive
    /// (stored, not compressed) whose `data/0` … `data/9` are the weights and
    /// biases of `layers.0`, `.2`, `.4`, `.6` and `.7`, in that order — the
    /// state dict's order, in which PyTorch numbers its storages — as
    /// little-endian float32. The file is pinned by its hash in the manifest;
    /// the sizes are checked anyway.
    pub fn from_pth(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let entries = stored_zip_entries(&bytes).with_context(|| format!("reading the archive {}", path.display()))?;
        let pickle = entries
            .iter()
            .find(|(name, _)| name.ends_with("/data.pkl"))
            .map(|(_, data)| *data)
            .context("no data.pkl: not a PyTorch file")?;
        for key in ["layers.0.weight", "layers.7.bias"] {
            ensure!(pickle.windows(key.len()).any(|w| w == key.as_bytes()), "{key} missing: not the aesthetic predictor");
        }
        let storage = |k: usize| -> Result<Vec<f32>> {
            let suffix = format!("/data/{k}");
            let data = entries
                .iter()
                .find(|(name, _)| name.ends_with(&suffix))
                .map(|(_, data)| *data)
                .with_context(|| format!("no tensor data/{k}"))?;
            ensure!(data.len() % 4 == 0, "data/{k} isn't float32");
            Ok(data.as_chunks::<4>().0.iter().map(|&c| f32::from_le_bytes(c)).collect())
        };
        let layers = (0..LAYERS.len()).map(|i| Ok((storage(2 * i)?, storage(2 * i + 1)?))).collect::<Result<Vec<_>>>()?;
        Self::from_layers(layers)
    }

    /// The rating of a CLIP image embedding (normalised here, as in training).
    pub fn score(&self, embedding: &[f32]) -> f64 {
        let norm = embedding.iter().map(|&v| (v as f64).powi(2)).sum::<f64>().sqrt().max(f64::EPSILON);
        let mut x: Vec<f64> = embedding.iter().map(|&v| v as f64 / norm).collect();
        for (w, b) in &self.layers {
            let inputs = x.len();
            x = b
                .iter()
                .enumerate()
                .map(|(o, &bias)| {
                    let row = &w[o * inputs..(o + 1) * inputs];
                    row.iter().zip(&x).map(|(&wv, &xv)| wv as f64 * xv).sum::<f64>() + bias as f64
                })
                .collect();
        }
        x[0]
    }
}

/// The entries of a zip archive whose files are stored uncompressed, as
/// PyTorch writes them: `(name, data)`, from the central directory.
fn stored_zip_entries(bytes: &[u8]) -> Result<Vec<(String, &[u8])>> {
    let u16_at = |at: usize| -> Result<usize> {
        bytes.get(at..at + 2).map(|b| u16::from_le_bytes([b[0], b[1]]) as usize).context("truncated archive")
    };
    let u32_at = |at: usize| -> Result<usize> {
        bytes.get(at..at + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize).context("truncated archive")
    };
    // End of central directory: 22 bytes plus a comment of up to 64 KiB.
    let min_start = bytes.len().saturating_sub(22 + 0xFFFF);
    let eocd = (min_start..=bytes.len().saturating_sub(22))
        .rev()
        .find(|&i| bytes[i..i + 4] == [0x50, 0x4b, 0x05, 0x06])
        .context("not a zip archive")?;
    let count = u16_at(eocd + 10)?;
    let mut at = u32_at(eocd + 16)?;
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        ensure!(u32_at(at)? == 0x0201_4b50, "bad central directory entry");
        let method = u16_at(at + 10)?;
        let size = u32_at(at + 20)?;
        let (name_len, extra_len, comment_len) = (u16_at(at + 28)?, u16_at(at + 30)?, u16_at(at + 32)?);
        let local = u32_at(at + 42)?;
        let name = String::from_utf8_lossy(bytes.get(at + 46..at + 46 + name_len).context("truncated archive")?).into_owned();
        if method != 0 {
            bail!("{name} is compressed (method {method}); expected a stored PyTorch archive");
        }
        ensure!(u32_at(local)? == 0x0403_4b50, "bad local header for {name}");
        let start = local + 30 + u16_at(local + 26)? + u16_at(local + 28)?;
        let data = bytes.get(start..start + size).with_context(|| format!("{name} runs past the end of the archive"))?;
        entries.push((name, data));
        at += 46 + name_len + extra_len + comment_len;
    }
    Ok(entries)
}

/// A crop, as fractions (0–1) of the photo's width and height.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CropRect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl CropRect {
    /// Whether `other` lies wholly inside this crop.
    pub fn contains(&self, other: &CropRect) -> bool {
        const EPS: f64 = 1e-6;
        other.x >= self.x - EPS
            && other.y >= self.y - EPS
            && other.x + other.w <= self.x + self.w + EPS
            && other.y + other.h <= self.y + self.h + EPS
    }

    /// Where the crop sits and how much it keeps, for a person to redo it
    /// by hand: "a square from the upper left, keeping 45% of the frame".
    pub fn describe(&self) -> String {
        let (cx, cy) = (self.x + self.w / 2.0, self.y + self.h / 2.0);
        let horizontal = if cx < 0.4 { Some("left") } else if cx > 0.6 { Some("right") } else { None };
        let vertical = if cy < 0.4 { Some("upper") } else if cy > 0.6 { Some("lower") } else { None };
        let place = match (vertical, horizontal) {
            (Some(v), Some(h)) => format!("the {v} {h}"),
            (Some("upper"), None) => "the top".to_string(),
            (Some(_), None) => "the bottom".to_string(),
            (None, Some(h)) => format!("the {h}"),
            (None, None) => "the centre".to_string(),
        };
        format!("a square from {place}, keeping {:.0}% of the frame", self.w * self.h * 100.0)
    }
}

/// Sides of the square crops tried, as fractions of the photo's short edge.
/// 70 % of the short edge keeps a third of a 3:2 frame: tighter crops cost
/// too much resolution to count as "the same photo, cropped".
pub const CROP_SIDES: [f64; 3] = [1.0, 0.85, 0.7];

/// The crops [`AestheticScorer::best_crop`] tries: squares (CLIP sees only a
/// square; any other shape would be cut to one again) with sides
/// [`CROP_SIDES`], each at the 3×3 positions start / centre / end of the room
/// left on each axis. The centre square at full size is what [`preprocess`]
/// sees of the uncropped photo. Duplicates (no room on an axis) are dropped.
pub fn crop_candidates(width: u32, height: u32) -> Vec<CropRect> {
    let (w, h) = (width.max(1) as f64, height.max(1) as f64);
    let short = w.min(h);
    let mut out: Vec<CropRect> = Vec::new();
    for side in CROP_SIDES {
        let (cw, ch) = (side * short / w, side * short / h);
        for fy in [0.0, 0.5, 1.0] {
            for fx in [0.0, 0.5, 1.0] {
                let rect = CropRect { x: (1.0 - cw) * fx, y: (1.0 - ch) * fy, w: cw, h: ch };
                let same = |r: &CropRect| (r.x - rect.x).abs() < 1e-9 && (r.y - rect.y).abs() < 1e-9 && (r.w - rect.w).abs() < 1e-9;
                if !out.iter().any(same) {
                    out.push(rect);
                }
            }
        }
    }
    out
}

/// The pixels of `rect` in `img`.
pub fn crop_image(img: &RgbImage, rect: &CropRect) -> RgbImage {
    let (w, h) = (img.width() as f64, img.height() as f64);
    let x = (rect.x * w).round().clamp(0.0, w - 1.0) as u32;
    let y = (rect.y * h).round().clamp(0.0, h - 1.0) as u32;
    let cw = ((rect.w * w).round() as u32).clamp(1, img.width() - x);
    let ch = ((rect.h * h).round() as u32).clamp(1, img.height() - y);
    image::imageops::crop_imm(img, x, y, cw, ch).to_image()
}

/// CLIP and the head together: a photo in, its aesthetic score out.
pub struct AestheticScorer {
    clip: Box<dyn LoadedModel>,
    head: AestheticHead,
    /// Which of the encoder's outputs is the embedding.
    output: usize,
}

impl AestheticScorer {
    pub fn new(clip: Box<dyn LoadedModel>, head: AestheticHead) -> Result<Self> {
        let names = clip.output_names();
        let output = names.iter().position(|n| n == "image_embeds").unwrap_or(0);
        Ok(Self { clip, head, output })
    }

    /// The CLIP embedding of `img`.
    pub fn embed(&mut self, img: &RgbImage) -> Result<Vec<f32>> {
        let mut out = self.clip.run(&[preprocess(img)])?;
        ensure!(self.output < out.len(), "the image encoder returned {} outputs", out.len());
        let embedding = out.swap_remove(self.output).data;
        ensure!(embedding.len() == EMBEDDING_LEN, "embedding has {} values, expected {EMBEDDING_LEN}", embedding.len());
        Ok(embedding)
    }

    /// The aesthetic score of `img`, roughly 1–10 (most photos 4–6.5).
    pub fn score(&mut self, img: &RgbImage) -> Result<f64> {
        let embedding = self.embed(img)?;
        Ok(self.head.score(&embedding))
    }

    /// The best-scoring of the [`crop_candidates`] of `img` that keep all of
    /// `keep` (the main face: the score alone happily cuts a face in half),
    /// and its score: what the photo could be with a crop. `None` when no
    /// candidate keeps it. ~20 runs of CLIP, so meant for the photos that
    /// score weak as shot, not for every photo.
    ///
    /// Taking the best of ~20 tries flatters: on 40 photos of one library the
    /// best crop beat the frame by 0.19 at the median, so a gain only means
    /// something above that.
    pub fn best_crop(&mut self, img: &RgbImage, keep: Option<&CropRect>) -> Result<Option<(CropRect, f64)>> {
        let mut best: Option<(CropRect, f64)> = None;
        for rect in crop_candidates(img.width(), img.height()) {
            if keep.is_some_and(|k| !rect.contains(k)) {
                continue;
            }
            let score = self.score(&crop_image(img, &rect))?;
            if best.is_none_or(|(_, b)| score > b) {
                best = Some((rect, score));
            }
        }
        Ok(best)
    }
}

/// The aesthetic scorer from `store`, on the first of the encoder's preferred
/// devices that `backend` can use and compile it for (a GPU that fails falls
/// back to the CPU). `Ok(None)` when the models aren't downloaded.
pub fn load_scorer(store: &ModelStore, backend: &dyn InferenceBackend) -> Result<Option<(AestheticScorer, Device)>> {
    let (Some(encoder), Some(head)) = (embedding_spec(), head_spec()) else { return Ok(None) };
    if !model_ready(store) {
        return Ok(None);
    }
    let encoder_path = store.verify(&encoder)?;
    let head = AestheticHead::from_pth(&store.verify(&head)?)?;
    let input = encoder.inputs.first().cloned().unwrap_or_else(|| vec![1, 3, SIZE, SIZE]);
    let usable: Vec<Device> = backend.devices().into_iter().filter(|d| d.available).map(|d| d.device).collect();
    let mut last_error = None;
    for device in encoder.preferred_devices.iter().filter_map(|name| match name.as_str() {
        "cpu" => Some(Device::Cpu),
        "gpu" => Some(Device::Gpu),
        "npu" => Some(Device::Npu),
        _ => None,
    }) {
        if !usable.contains(&device) {
            continue;
        }
        match backend.load_with_input_shape(&encoder_path, device, &input) {
            Ok(model) => return Ok(Some((AestheticScorer::new(model, head)?, device))),
            Err(e) => {
                log::warn!("The image encoder couldn't be compiled for {device}: {e:#}");
                last_error = Some(e);
            }
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("no usable device for the image encoder")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stored (uncompressed) zip archive of `files`, as PyTorch writes one.
    fn stored_zip(files: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, data) in files {
            let offset = out.len() as u32;
            let header = |sig: u32, central: bool| {
                let mut h = Vec::new();
                h.extend_from_slice(&sig.to_le_bytes());
                if central {
                    h.extend_from_slice(&[20, 0]); // made by
                }
                h.extend_from_slice(&[20, 0, 0, 0, 0, 0, 0, 0, 0, 0]); // version, flags, method 0, time, date
                h.extend_from_slice(&0u32.to_le_bytes()); // crc (not checked)
                h.extend_from_slice(&(data.len() as u32).to_le_bytes());
                h.extend_from_slice(&(data.len() as u32).to_le_bytes());
                h.extend_from_slice(&(name.len() as u16).to_le_bytes());
                h.extend_from_slice(&0u16.to_le_bytes()); // extra
                if central {
                    h.extend_from_slice(&[0; 6]); // comment, disk, internal attrs
                    h.extend_from_slice(&0u32.to_le_bytes()); // external attrs
                    h.extend_from_slice(&offset.to_le_bytes());
                }
                h.extend_from_slice(name.as_bytes());
                h
            };
            out.extend(header(0x0403_4b50, false));
            out.extend_from_slice(data);
            central.extend(header(0x0201_4b50, true));
        }
        let cd_offset = out.len() as u32;
        out.extend_from_slice(&central);
        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&(files.len() as u16).to_le_bytes());
        out.extend_from_slice(&(files.len() as u16).to_le_bytes());
        out.extend_from_slice(&(central.len() as u32).to_le_bytes());
        out.extend_from_slice(&cd_offset.to_le_bytes());
        out.extend_from_slice(&[0, 0]);
        out
    }

    fn floats(values: &[f32]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    /// A head that sums its first input times `gain`, plus `bias`.
    fn head_files(gain: f32, bias: f32) -> Vec<(String, Vec<u8>)> {
        let mut files = vec![("archive/data.pkl".to_string(), b"..layers.0.weight..layers.7.bias..".to_vec())];
        for (i, &(inputs, outputs)) in LAYERS.iter().enumerate() {
            // Each layer passes its first input through to its first output.
            let mut w = vec![0.0f32; inputs * outputs];
            w[0] = if i == 0 { gain } else { 1.0 };
            let mut b = vec![0.0f32; outputs];
            if i == LAYERS.len() - 1 {
                b[0] = bias;
            }
            files.push((format!("archive/data/{}", 2 * i), floats(&w)));
            files.push((format!("archive/data/{}", 2 * i + 1), floats(&b)));
        }
        files
    }

    #[test]
    fn head_is_read_from_a_pytorch_archive_and_scores_the_normalised_embedding() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("head.pth");
        let files = head_files(2.0, 5.0);
        let refs: Vec<(&str, Vec<u8>)> = files.iter().map(|(n, d)| (n.as_str(), d.clone())).collect();
        std::fs::write(&path, stored_zip(&refs)).unwrap();
        let head = AestheticHead::from_pth(&path).unwrap();

        // [3, 4, 0…] normalises to [0.6, 0.8, 0…]: 2 × 0.6 + 5.
        let mut embedding = vec![0.0f32; EMBEDDING_LEN];
        embedding[0] = 3.0;
        embedding[1] = 4.0;
        assert!((head.score(&embedding) - 6.2).abs() < 1e-6);
        // Scale doesn't matter.
        embedding.iter_mut().for_each(|v| *v *= 10.0);
        assert!((head.score(&embedding) - 6.2).abs() < 1e-6);
    }

    #[test]
    fn wrong_archives_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("head.pth");

        // A layer of the wrong size.
        let mut files = head_files(1.0, 0.0);
        files[3].1 = floats(&[0.0; 10]);
        let refs: Vec<(&str, Vec<u8>)> = files.iter().map(|(n, d)| (n.as_str(), d.clone())).collect();
        std::fs::write(&path, stored_zip(&refs)).unwrap();
        assert!(AestheticHead::from_pth(&path).is_err());

        // Another model's state dict.
        let mut files = head_files(1.0, 0.0);
        files[0].1 = b"fc.weight".to_vec();
        let refs: Vec<(&str, Vec<u8>)> = files.iter().map(|(n, d)| (n.as_str(), d.clone())).collect();
        std::fs::write(&path, stored_zip(&refs)).unwrap();
        assert!(AestheticHead::from_pth(&path).is_err());

        // Not a zip at all.
        std::fs::write(&path, b"not an archive").unwrap();
        assert!(AestheticHead::from_pth(&path).is_err());
    }

    #[test]
    fn crop_candidates_are_squares_within_the_frame() {
        // 3:2 landscape: full-height squares at left/centre/right, then 9 each
        // for the two smaller sizes.
        let crops = crop_candidates(3000, 2000);
        assert_eq!(crops.len(), 3 + 9 + 9);
        for c in &crops {
            assert!(c.x >= 0.0 && c.y >= 0.0 && c.x + c.w <= 1.0 + 1e-9 && c.y + c.h <= 1.0 + 1e-9, "{c:?}");
            assert!((c.w * 3000.0 - c.h * 2000.0).abs() < 1e-6, "square: {c:?}");
        }
        // The uncropped view (the centre full-height square) is one of them.
        assert!(crops.iter().any(|c| (c.x - 1.0 / 6.0).abs() < 1e-9 && c.y == 0.0 && (c.h - 1.0).abs() < 1e-9));
        // A square photo: one full-size crop, then 9 + 9.
        assert_eq!(crop_candidates(1000, 1000).len(), 1 + 9 + 9);

        let img = RgbImage::new(300, 200);
        let right = crop_image(&img, &crops[2]);
        assert_eq!(right.dimensions(), (200, 200));

        // A face at the right edge: only crops reaching the right edge keep it.
        let face = CropRect { x: 0.85, y: 0.3, w: 0.1, h: 0.2 };
        let keeping: Vec<&CropRect> = crops.iter().filter(|c| c.contains(&face)).collect();
        assert!(!keeping.is_empty() && keeping.iter().all(|c| c.x + c.w > 0.95), "{keeping:?}");
        assert_eq!(crops[2].describe(), "a square from the right, keeping 67% of the frame");
        let corner = CropRect { x: 0.0, y: 0.0, w: 0.35, h: 0.5 };
        assert_eq!(corner.describe(), "a square from the upper left, keeping 18% of the frame");
    }

    #[test]
    fn preprocessing_crops_the_centre_and_normalises() {
        // 600×300: left third red, middle third green, right third blue.
        // Resized to 448×224 (thirds ~149 px), the centre crop spans x 112–336:
        // red up to 37, green to 187, blue after.
        let img = RgbImage::from_fn(600, 300, |x, _| match x / 200 {
            0 => image::Rgb([255, 0, 0]),
            1 => image::Rgb([0, 255, 0]),
            _ => image::Rgb([0, 0, 255]),
        });
        let t = preprocess(&img);
        assert_eq!(t.shape, vec![1, 3, SIZE, SIZE]);
        let plane = SIZE * SIZE;
        let at = |c: usize, x: usize, y: usize| t.data[c * plane + y * SIZE + x];
        let green = (1.0 - MEAN[1]) / STD[1];
        let none = (0.0 - MEAN[0]) / STD[0];
        assert!((at(1, SIZE / 2, SIZE / 2) - green).abs() < 1e-3);
        assert!((at(0, SIZE / 2, SIZE / 2) - none).abs() < 1e-3);
        assert!(at(0, 0, SIZE / 2) > 1.0, "left edge should be red");
        assert!(at(2, SIZE - 1, SIZE / 2) > 1.0, "right edge should be blue");
    }

    /// The real head, if downloaded (`PHOTON_AESTHETIC_HEAD=…/sac+logos+ava1-l14-linearMSE.pth`),
    /// against a score computed independently in Python with NumPy.
    #[test]
    #[ignore]
    fn real_head_matches_the_reference() {
        let path = std::env::var("PHOTON_AESTHETIC_HEAD").expect("set PHOTON_AESTHETIC_HEAD");
        let head = AestheticHead::from_pth(Path::new(&path)).unwrap();
        let embedding: Vec<f32> = (0..EMBEDDING_LEN).map(|i| (i as f32 * 0.37).sin()).collect();
        let score = head.score(&embedding);
        assert!((score - 5.657_232_16).abs() < 1e-4, "score {score}");
    }
}
