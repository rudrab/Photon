//! Zoom to faces: where the faces are in a photo, found by the face model
//! (`photon_ai::faces`, YuNet) on its Large preview when first asked for,
//! and kept for the session.
//!
//! Faces are numbered left to right, so in a burst "face 2" is the same
//! person in every frame: stepping through faces in Survey or Compare shows
//! one person across all the photos at a time.

use photon_core::models::Image;
use photon_import::thumbnails::{ThumbSize, ThumbnailGenerator};
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Mutex;

/// A face, in fractions of the upright photo's width and height.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FaceBox {
    pub center: (f64, f64),
    pub size: (f64, f64),
}

/// Faces smaller than this, relative to the largest face, are left out:
/// passers-by in the background, not who the photo is of.
const MIN_RELATIVE_HEIGHT: f32 = 0.4;

/// Shown when there is no face model to find faces with.
pub const NO_MODEL: &str = "Zoom to faces needs the face model: download it in Preferences → AI";

thread_local! {
    static FOUND: RefCell<HashMap<String, Rc<Vec<FaceBox>>>> = Default::default();
}

/// Loaded on first use and kept: loading compiles the model for the device.
static DETECTOR: Mutex<Option<photon_ai::faces::FaceDetector>> = Mutex::new(None);

/// Whether the face model is there to find faces with. Cheap enough to ask
/// on every key press.
pub fn model_ready() -> bool {
    photon_ai::faces::model_ready(&photon_ai::store::ModelStore::default_store())
}

/// The faces in `image`, left to right. `Err` says why they couldn't be looked for.
pub async fn faces_of(image: &Image, cache_dir: &Path) -> Result<Rc<Vec<FaceBox>>, String> {
    if let Some(found) = FOUND.with(|f| f.borrow().get(&image.hash).cloned()) {
        return Ok(found);
    }
    let (img, cache): (Image, PathBuf) = (image.clone(), cache_dir.to_path_buf());
    let found = gtk4::gio::spawn_blocking(move || detect(&img, &cache))
        .await
        .map_err(|_| "the face detector crashed".to_string())??;
    let found = Rc::new(found);
    FOUND.with(|f| f.borrow_mut().insert(image.hash.clone(), found.clone()));
    Ok(found)
}

/// Forget the faces of a photo that has changed how it looks (rotated).
pub fn invalidate(hash: &str) {
    FOUND.with(|f| f.borrow_mut().remove(hash));
}

fn detect(image: &Image, cache_dir: &Path) -> Result<Vec<FaceBox>, String> {
    let preview = ThumbnailGenerator::new(cache_dir.to_path_buf())
        .ensure(image, ThumbSize::Large)
        .map_err(|e| format!("no preview: {e:#}"))?;
    let rgb = image::open(&preview).map_err(|e| format!("reading {}: {e}", preview.display()))?.to_rgb8();

    let mut detector = DETECTOR.lock().unwrap_or_else(|e| e.into_inner());
    if detector.is_none() {
        let backend = photon_ai::openvino::OpenVinoBackend::new();
        match photon_ai::faces::load_detector(&photon_ai::store::ModelStore::default_store(), &backend) {
            Ok(Some((loaded, device))) => {
                log::info!("Face detector for zoom to faces on {device}");
                *detector = Some(loaded);
            }
            Ok(None) => return Err(NO_MODEL.to_string()),
            Err(e) => return Err(format!("the face model could not be loaded: {e:#}")),
        }
    }
    let Some(detector) = detector.as_mut() else { return Err(NO_MODEL.to_string()) };
    let faces = detector
        .detect(&rgb, photon_ai::faces::MIN_SCORE)
        .map_err(|e| format!("face detection failed: {e:#}"))?;
    Ok(arrange(&faces, rgb.width(), rgb.height()))
}

/// `faces` found in a `w`×`h` image as fractions of it: the ones that matter
/// (see [`MIN_RELATIVE_HEIGHT`]), left to right.
fn arrange(faces: &[photon_ai::faces::Face], w: u32, h: u32) -> Vec<FaceBox> {
    let (w, h) = (w.max(1) as f64, h.max(1) as f64);
    let tallest = faces.iter().map(|f| f.h).fold(0.0f32, f32::max);
    let mut boxes: Vec<FaceBox> = faces
        .iter()
        .filter(|f| f.h >= tallest * MIN_RELATIVE_HEIGHT)
        .map(|f| FaceBox {
            center: (((f.x + f.w / 2.0) as f64 / w).clamp(0.0, 1.0), ((f.y + f.h / 2.0) as f64 / h).clamp(0.0, 1.0)),
            size: (f.w as f64 / w, f.h as f64 / h),
        })
        .collect();
    boxes.sort_by(|a, b| a.center.0.total_cmp(&b.center.0));
    boxes
}

/// Face number `step` of `faces` (counting round), for stepping through faces.
pub fn pick(faces: &[FaceBox], step: isize) -> Option<FaceBox> {
    let n = faces.len() as isize;
    (n > 0).then(|| faces[step.rem_euclid(n) as usize])
}

#[cfg(test)]
mod tests {
    use super::*;
    use photon_ai::faces::Face;

    fn face(x: f32, h: f32) -> Face {
        Face { x, y: 100.0, w: h, h, score: 0.9, landmarks: [(0.0, 0.0); 5] }
    }

    #[test]
    fn faces_run_left_to_right_without_the_background() {
        // Largest first (as detected), a small face far behind, one more.
        let boxes = arrange(&[face(600.0, 200.0), face(50.0, 30.0), face(100.0, 150.0)], 1000, 500);
        let xs: Vec<f64> = boxes.iter().map(|b| b.center.0).collect();
        assert_eq!(xs, [0.175, 0.7]);
        assert_eq!(boxes[1].size, (0.2, 0.4));
        assert_eq!(boxes[1].center.1, 0.4);
    }

    #[test]
    fn stepping_counts_round() {
        let boxes = arrange(&[face(0.0, 100.0), face(300.0, 100.0), face(600.0, 100.0)], 1000, 500);
        assert_eq!(pick(&boxes, 4), Some(boxes[1]));
        assert_eq!(pick(&boxes, -1), Some(boxes[2]));
        assert_eq!(pick(&[], 0), None);
    }
}
