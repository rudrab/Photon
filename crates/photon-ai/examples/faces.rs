//! Development tool: run the face detector on photos and print what it finds.
//! `cargo run -p photon-ai --release --example faces -- <model.onnx> <photo>...`

use anyhow::{Context, Result};
use photon_ai::backend::{Device, InferenceBackend};
use photon_ai::faces::{eye_sharpness, FaceDetector};
use photon_ai::openvino::OpenVinoBackend;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let model_path = args.first().context("usage: faces <model.onnx> <photo>...")?;
    let backend = OpenVinoBackend::new();
    let mut detector = FaceDetector::new(backend.load(std::path::Path::new(model_path), Device::Cpu)?)?;
    for photo in &args[1..] {
        let img = image::open(photo)?.to_rgb8();
        // Photon works on the Large preview (2560 px long edge).
        let img = image::imageops::resize(&img, 2560, 2560 * img.height() / img.width(), image::imageops::FilterType::Triangle);
        let faces = detector.detect(&img, 0.6)?;
        let name = std::path::Path::new(photo).file_name().unwrap_or_default().to_string_lossy();
        if faces.is_empty() {
            println!("{name:<14} no face");
        }
        for f in faces {
            let eyes = eye_sharpness(&img, &f).map_or("—".into(), |v| format!("{v:.0}"));
            if let Ok(dir) = std::env::var("FACES_DUMP") {
                // The eye region as measured, for checking by eye.
                let [(x1, y1), (x2, y2), ..] = f.landmarks;
                let d = ((x2 - x1).powi(2) + (y2 - y1).powi(2)).sqrt();
                let (l, t) = ((x1.min(x2) - 0.5 * d).max(0.0), (y1.min(y2) - 0.4 * d).max(0.0));
                let crop = image::imageops::crop_imm(&img, l as u32, t as u32, (x1.max(x2) - x1.min(x2) + d) as u32, (y1.max(y2) - y1.min(y2) + 0.8 * d) as u32).to_image();
                crop.save(format!("{dir}/{name}.eyes.png"))?;
            }
            println!("{name:<14} face {:>4.0}x{:<4.0} at ({:>4.0},{:>4.0}) score {:.2}  eye sharpness {eyes}", f.w, f.h, f.x, f.y, f.score);
        }
    }
    Ok(())
}
