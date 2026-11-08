//! Development tool: print the aesthetic score of photos.
//! `cargo run -p photon-ai --release --example aesthetic -- <clip.onnx> <head.pth> [cpu|gpu] <photo>...`
//! With `CROPS=1`, also the best square crop and how much it gains; with
//! `FACES=<yunet.onnx>` too, only crops that keep the largest face (as Photon does).

use anyhow::{Context, Result};
use photon_ai::aesthetic::{AestheticHead, AestheticScorer, SIZE};
use photon_ai::backend::{Device, InferenceBackend};
use photon_ai::openvino::OpenVinoBackend;
use std::path::Path;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let usage = "usage: aesthetic <clip.onnx> <head.pth> [cpu|gpu] <photo>...";
    let (clip, head) = (args.first().context(usage)?, args.get(1).context(usage)?);
    let (device, photos) = match args.get(2).map(String::as_str) {
        Some("gpu") => (Device::Gpu, &args[3..]),
        Some("cpu") => (Device::Cpu, &args[3..]),
        _ => (Device::Cpu, &args[2..]),
    };
    let started = std::time::Instant::now();
    let model = OpenVinoBackend::new().load_with_input_shape(Path::new(clip), device, &[1, 3, SIZE, SIZE])?;
    let mut scorer = AestheticScorer::new(model, AestheticHead::from_pth(Path::new(head))?)?;
    println!("loaded on {device} in {:.1} s", started.elapsed().as_secs_f32());
    let mut detector = match std::env::var("FACES") {
        Ok(path) => Some(photon_ai::faces::FaceDetector::new(OpenVinoBackend::new().load(Path::new(&path), Device::Cpu)?)?),
        Err(_) => None,
    };
    for photo in photos {
        let img = image::open(photo)?.to_rgb8();
        let t = std::time::Instant::now();
        // Photon scores the Large preview (2560 px long edge).
        let img = if img.width().max(img.height()) > 2560 {
            let s = 2560.0 / img.width().max(img.height()) as f64;
            image::imageops::resize(&img, (img.width() as f64 * s) as u32, (img.height() as f64 * s) as u32, image::imageops::FilterType::Triangle)
        } else {
            img
        };
        let score = scorer.score(&img)?;
        let name = Path::new(photo).file_name().unwrap_or_default().to_string_lossy();
        if std::env::var("CROPS").is_ok() {
            // With FACES=<yunet.onnx>, crops must keep the largest face, as in Photon.
            let keep = match &mut detector {
                Some(d) => photon_ai::faces::largest_face(d, &img)?.map(|f| {
                    let (w, h) = (img.width() as f64, img.height() as f64);
                    let (x0, y0) = ((f.x as f64 / w).clamp(0.0, 1.0), (f.y as f64 / h).clamp(0.0, 1.0));
                    let (x1, y1) = (((f.x + f.w) as f64 / w).clamp(0.0, 1.0), ((f.y + f.h) as f64 / h).clamp(0.0, 1.0));
                    photon_ai::aesthetic::CropRect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 }
                }),
                None => None,
            };
            let (c, best) = scorer
                .best_crop(&img, keep.as_ref())?
                .unwrap_or((photon_ai::aesthetic::CropRect { x: 0.0, y: 0.0, w: 1.0, h: 1.0 }, score));
            println!(
                "{name:<28} {score:.2} best-crop {best:.2} gain {:+.2} x {:.3} y {:.3} w {:.3} h {:.3}  {}   ({} ms)",
                best - score, c.x, c.y, c.w, c.h, c.describe(), t.elapsed().as_millis()
            );
        } else {
            println!("{name:<28} {score:.2}   ({} ms)", t.elapsed().as_millis());
        }
    }
    Ok(())
}
