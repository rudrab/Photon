use anyhow::{bail, Context, Result};
use photon_ai::backend::{InferenceBackend, Tensor};
use photon_ai::manifest::ModelManifest;
use photon_ai::openvino::OpenVinoBackend;
use photon_ai::store::ModelStore;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::Instant;

fn get_peak_rss_mb() -> f64 {
    if let Ok(file) = File::open("/proc/self/status") {
        let reader = BufReader::new(file);
        for line in reader.lines().flatten() {
            if line.starts_with("VmHWM:") {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 {
                    if let Ok(kb) = parts[1].parse::<f64>() {
                        return kb / 1024.0;
                    }
                }
            }
        }
    }
    0.0
}

fn load_image_tensor(image_path: &Path, width: u32, height: u32) -> Result<Tensor> {
    let img = image::open(image_path)
        .with_context(|| format!("Failed to open image at {}", image_path.display()))?;
    let resized = img.resize_exact(width, height, image::imageops::FilterType::Triangle);
    let rgb = resized.to_rgb8();

    let mut data = Vec::with_capacity((3 * width * height) as usize);
    // NCHW format: [1, 3, H, W] in float32 [0.0, 255.0]
    for c in 0..3 {
        for y in 0..height {
            for x in 0..width {
                let pixel = rgb.get_pixel(x, y);
                data.push(pixel[c] as f32);
            }
        }
    }

    Ok(Tensor::new(vec![1, 3, height as usize, width as usize], data))
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let mut model_arg: Option<String> = None;
    let mut image_arg: Option<String> = None;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--model" => {
                if i + 1 < args.len() {
                    model_arg = Some(args[i + 1].clone());
                    i += 2;
                } else {
                    bail!("--model requires an argument");
                }
            }
            "--image" => {
                if i + 1 < args.len() {
                    image_arg = Some(args[i + 1].clone());
                    i += 2;
                } else {
                    bail!("--image requires an argument");
                }
            }
            _ => {
                i += 1;
            }
        }
    }

    let model_id_or_path = model_arg.unwrap_or_else(|| "yunet-2023mar".to_string());
    println!("=== Photon AI Benchmark ===");
    println!("Model: {}", model_id_or_path);

    // Resolve model path
    let model_path = if Path::new(&model_id_or_path).exists() {
        PathBuf::from(&model_id_or_path)
    } else {
        let manifest = ModelManifest::load_embedded();
        let store = ModelStore::default_store();
        if let Some(spec) = manifest.get_by_id(&model_id_or_path) {
            match store.verify(spec) {
                Ok(p) => p,
                Err(_) => {
                    // Fallback check in /tmp for testing
                    let tmp_p = PathBuf::from("/tmp").join(format!("{}.onnx", model_id_or_path));
                    let tmp_yunet = PathBuf::from("/tmp/face_detection_yunet_2023mar.onnx");
                    if tmp_p.exists() {
                        tmp_p
                    } else if tmp_yunet.exists() && model_id_or_path.contains("yunet") {
                        tmp_yunet
                    } else {
                        bail!(
                            "Model {} not found in store, current dir, or /tmp. Download it first or provide path.",
                            model_id_or_path
                        );
                    }
                }
            }
        } else {
            bail!("Model {} not found in manifest", model_id_or_path);
        }
    };
    println!("Model file: {}", model_path.display());

    // The input is built per device from the model's own input shape.
    if let Some(ref img_p) = image_arg {
        println!("Input image: {img_p}");
    } else {
        println!("No input image given: using a flat grey input");
    }
    let make_input = |shape: &[usize]| -> Result<Tensor> {
        let [1, 3, h, w] = shape else {
            bail!("unsupported model input shape {shape:?} (expected [1, 3, H, W])");
        };
        match image_arg {
            Some(ref img_p) => load_image_tensor(Path::new(img_p), *w as u32, *h as u32),
            None => Ok(Tensor::new(shape.to_vec(), vec![128.0; 3 * h * w])),
        }
    };

    let backend = OpenVinoBackend::new();
    let devices = backend.devices();

    println!("\nDetected Devices:");
    for dev in &devices {
        println!(
            "  - {:<4} | {:<25} | Available: {:<5} {}",
            dev.device.as_str(),
            dev.name,
            dev.available,
            dev.reason.as_deref().unwrap_or("")
        );
    }

    println!("\n{:-<92}", "");
    println!(
        "{:<6} | {:>13} | {:>12} | {:>10} | {:>10} | {:>16} | input",
        "Device", "Compile (ms)", "Cached (ms)", "Median ms", "Max ms", "Process peak MB"
    );
    println!("{:-<92}", "");

    for dev_info in devices {
        if !dev_info.available {
            println!("{:<6} | unavailable: {}", dev_info.device.as_str(), dev_info.reason.unwrap_or_default());
            continue;
        }
        let device = dev_info.device;
        match bench_device(&backend, &model_path, device, &make_input) {
            Ok(r) => println!(
                "{:<6} | {:>13.1} | {:>12.1} | {:>10.2} | {:>10.2} | {:>16.1} | {:?}",
                device.as_str(),
                r.compile_ms,
                r.cached_ms,
                r.median_ms,
                r.max_ms,
                get_peak_rss_mb(),
                r.input_shape
            ),
            Err(e) => println!("{:<6} | failed: {e:#}", device.as_str()),
        }
    }
    println!("{:-<92}", "");
    println!("Process peak = the benchmark process's memory high-water mark so far (VmHWM).\n");

    Ok(())
}

struct DeviceResult {
    compile_ms: f64,
    cached_ms: f64,
    median_ms: f64,
    max_ms: f64,
    input_shape: Vec<usize>,
}

/// Compile (first, then from OpenVINO's model cache), 3 warm-up runs, 10 timed runs.
fn bench_device(
    backend: &OpenVinoBackend,
    model_path: &Path,
    device: photon_ai::backend::Device,
    make_input: &dyn Fn(&[usize]) -> Result<Tensor>,
) -> Result<DeviceResult> {
    let t = Instant::now();
    drop(backend.load(model_path, device)?);
    let compile_ms = t.elapsed().as_secs_f64() * 1000.0;

    let t = Instant::now();
    let mut model = backend.load(model_path, device)?;
    let cached_ms = t.elapsed().as_secs_f64() * 1000.0;

    let input_shape = model.input_shapes().into_iter().next().context("the model has no inputs")?;
    let input = make_input(&input_shape)?;
    for _ in 0..3 {
        model.run(&[input.clone()])?;
    }
    let mut durations = Vec::with_capacity(10);
    for _ in 0..10 {
        let t0 = Instant::now();
        model.run(&[input.clone()])?;
        durations.push(t0.elapsed().as_secs_f64() * 1000.0);
    }
    durations.sort_by(|a, b| a.total_cmp(b));
    let median_ms = (durations[4] + durations[5]) / 2.0;
    let max_ms = durations[9];
    Ok(DeviceResult { compile_ms, cached_ms, median_ms, max_ms, input_shape })
}
