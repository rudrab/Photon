//! Rendering camera RAW files from their sensor data, rather than from the
//! JPEG preview the camera embedded in them.
//!
//! Two renderers:
//!   * [`develop`]: built in (rawler). Demosaic, white balance, camera colour
//!     matrix, then a tone curve fitted so the result is as bright as the
//!     camera's own preview. A neutral rendering, for checking focus at 1:1.
//!   * [`render_with_darktable`]: `darktable-cli` with the photo's XMP
//!     sidecar, so the result carries the user's darktable edits. For export.
//!
//! Neither applies the EXIF orientation: [`develop`] returns sensor
//! orientation (like the embedded previews), darktable renders upright.

use crate::thumbnails;
use anyhow::{bail, Context, Result};
use crate::icc::{ColorSpace, Rgb16Image};
use image::{DynamicImage, RgbImage};
use rawler::imgop::develop::{Intermediate, ProcessingStep, RawDevelop};
use rayon::prelude::*;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Decode and develop the RAW file at `path` at full sensor resolution, 8 bits
/// per channel, sRGB.
pub fn develop(path: &Path) -> Result<RgbImage> {
    let (w, h, linear, gain) = develop_linear(path)?;
    let lut = tone_lut(gain, 255.0, |v| v as u8);
    RgbImage::from_raw(w, h, apply_lut(&linear, &lut)).context("RAW buffer size mismatch")
}

/// [`develop`] at 16 bits per channel, for exports that keep them.
pub fn develop16(path: &Path) -> Result<Rgb16Image> {
    let (w, h, linear, gain) = develop_linear(path)?;
    let lut = tone_lut(gain, 65535.0, |v| v as u16);
    Rgb16Image::from_raw(w, h, apply_lut(&linear, &lut)).context("RAW buffer size mismatch")
}

/// The sensor data demosaiced, white balanced and in linear sRGB (0–1,
/// interleaved RGB), with the tone-curve gain that matches the camera's
/// preview.
fn develop_linear(path: &Path) -> Result<(u32, u32, Vec<f32>, f32)> {
    let raw = rawler::decode_file(path).with_context(|| format!("Decoding RAW {}", path.display()))?;
    // Everything but the final sRGB gamma: the tone curve works in linear light.
    let steps = [
        ProcessingStep::Rescale,
        ProcessingStep::Demosaic,
        ProcessingStep::FujiRotate,
        ProcessingStep::CropActiveArea,
        ProcessingStep::WhiteBalance,
        ProcessingStep::Calibrate,
        ProcessingStep::CropDefault,
    ];
    let developed = RawDevelop::new_with(&steps)
        .develop_intermediate(&raw)
        .context("Developing RAW")?;
    let Intermediate::ThreeColor(pixels) = developed else {
        bail!("Unsupported sensor layout (monochrome or four-colour)");
    };
    let (w, h) = (pixels.dim().w as u32, pixels.dim().h as u32);
    let linear = pixels.flatten();

    let gain = thumbnails::embedded_preview_luma(path)
        .map(|target| fit_gain(&linear, target))
        .unwrap_or(DEFAULT_GAIN);
    Ok((w, h, linear, gain))
}

fn apply_lut<T: Copy + Default + Send + Sync>(linear: &[f32], lut: &[T]) -> Vec<T> {
    let mut out = vec![T::default(); linear.len()];
    out.par_chunks_mut(3 * 4096)
        .zip(linear.par_chunks(3 * 4096))
        .for_each(|(out, inp)| {
            for (o, &x) in out.iter_mut().zip(inp) {
                *o = lut[(x.clamp(0.0, 1.0) * (LUT_SIZE - 1) as f32) as usize];
            }
        });
    out
}

/// Used when there is no preview to match: about +1.5 EV in the midtones,
/// close to what cameras do.
const DEFAULT_GAIN: f32 = 3.0;
// 16 bits of input precision, so 16-bit output keeps smooth gradients.
const LUT_SIZE: usize = 1 << 16;

/// The tone curve `g·x / (1 + (g−1)·x)` on linear light: slope `g` in the
/// shadows, rolling off smoothly to keep highlights instead of clipping them.
fn tone(x: f32, gain: f32) -> f32 {
    gain * x / (1.0 + (gain - 1.0) * x)
}

fn srgb_encode(x: f32) -> f32 {
    if x <= 0.003_130_8 {
        12.92 * x
    } else {
        1.055 * x.powf(1.0 / 2.4) - 0.055
    }
}

/// The tone curve and sRGB encoding for every LUT input, scaled to `max`
/// (255 or 65535) and converted by `to`.
fn tone_lut<T>(gain: f32, max: f32, to: impl Fn(f32) -> T) -> Vec<T> {
    (0..LUT_SIZE)
        .map(|i| {
            let x = i as f32 / (LUT_SIZE - 1) as f32;
            to((srgb_encode(tone(x, gain)) * max + 0.5).clamp(0.0, max))
        })
        .collect()
}

/// The gain for which the mean sRGB luma of `linear` (interleaved RGB, 0–1)
/// comes out at `target` (0–1).
fn fit_gain(linear: &[f32], target: f32) -> f32 {
    // A few tens of thousands of pixels are plenty for a mean.
    let step = (linear.len() / 3 / 40_000).max(1) * 3;
    let lumas: Vec<f32> = linear
        .chunks_exact(3)
        .step_by(step / 3)
        .map(|p| 0.2126 * p[0] + 0.7152 * p[1] + 0.0722 * p[2])
        .map(|y| y.clamp(0.0, 1.0))
        .collect();
    if lumas.is_empty() {
        return DEFAULT_GAIN;
    }
    let mean_at = |gain: f32| lumas.iter().map(|&y| srgb_encode(tone(y, gain))).sum::<f32>() / lumas.len() as f32;

    // Brightness grows with the gain: bisect in log space.
    let (mut lo, mut hi) = (0.25f32.ln(), 64f32.ln());
    for _ in 0..30 {
        let mid = (lo + hi) / 2.0;
        if mean_at(mid.exp()) < target {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    ((lo + hi) / 2.0).exp()
}

// ── darktable ────────────────────────────────────────────

/// Path of `darktable-cli`, if installed.
pub fn darktable_cli() -> Option<PathBuf> {
    static FOUND: OnceLock<Option<PathBuf>> = OnceLock::new();
    FOUND
        .get_or_init(|| {
            std::env::split_paths(&std::env::var_os("PATH")?)
                .map(|dir| dir.join("darktable-cli"))
                .find(|p| p.is_file())
        })
        .clone()
}

/// Longest a single darktable render may take before it is abandoned.
const DARKTABLE_TIMEOUT: Duration = Duration::from_secs(300);

/// Render `raw` through darktable with its edit history from `xmp` (or, with
/// none, darktable's defaults for an unedited photo). Upright, full size, in
/// colour space `space` (tagged with its profile), at 8 or 16 bits.
///
/// darktable runs with a private copy of the user's configuration: it gets
/// their preferences and presets, but can't clash with a running darktable
/// over the library lock. Renders are serialised: each one uses every core
/// (and the GPU) already.
pub fn render_with_darktable(raw: &Path, xmp: Option<&Path>, space: ColorSpace, sixteen_bit: bool) -> Result<DynamicImage> {
    static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());
    let cli = darktable_cli().context("darktable-cli is not installed")?;
    let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());

    let work = tempfile::Builder::new().prefix("photon-darktable-").tempdir()?;
    let configdir = private_darktable_config(work.path())?;
    let out = work.path().join("render.tif");

    let mut cmd = Command::new(cli);
    cmd.arg(raw);
    if let Some(xmp) = xmp {
        cmd.arg(xmp);
    }
    cmd.arg(&out)
        .args(["--hq", "true", "--icc-type", space.darktable_name(), "--core", "--configdir"])
        .arg(&configdir)
        .args(["--conf", if sixteen_bit { "plugins/imageio/format/tiff/bpp=16" } else { "plugins/imageio/format/tiff/bpp=8" }])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().context("Starting darktable-cli")?;

    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() > DARKTABLE_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            bail!("darktable-cli took over {} s; abandoned", DARKTABLE_TIMEOUT.as_secs());
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    if !status.success() || !out.exists() {
        let mut stderr = String::new();
        if let Some(mut pipe) = child.stderr.take() {
            let _ = std::io::Read::read_to_string(&mut pipe, &mut stderr);
        }
        let last = stderr.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("no output");
        bail!("darktable-cli failed ({status}): {}", last.trim());
    }
    image::open(&out).context("Reading darktable's render")
}

/// A configuration directory under `work` holding a copy of the user's
/// darktable preferences and presets.
fn private_darktable_config(work: &Path) -> Result<PathBuf> {
    let dir = work.join("config");
    std::fs::create_dir_all(&dir)?;
    if let Some(user) = dirs::config_dir().map(|c| c.join("darktable")) {
        for file in ["darktablerc", "data.db"] {
            let src = user.join(file);
            if src.is_file() {
                std::fs::copy(&src, dir.join(file)).with_context(|| format!("Copying {}", src.display()))?;
            }
        }
    }
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tone_curve_keeps_black_and_white_and_is_monotonic() {
        for gain in [0.5, 1.0, 3.0, 20.0] {
            assert_eq!(tone(0.0, gain), 0.0);
            assert!((tone(1.0, gain) - 1.0).abs() < 1e-6);
            let lut = tone_lut(gain, 255.0, |v| v as u8);
            assert!(lut.windows(2).all(|w| w[0] <= w[1]));
            assert_eq!(lut[0], 0);
            assert_eq!(lut[LUT_SIZE - 1], 255);
        }
    }

    #[test]
    fn fitted_gain_hits_the_target_brightness() {
        // A dim, flat linear image: mid-grey at 5 %.
        let linear: Vec<f32> = (0..30_000).map(|i| 0.02 + 0.06 * (i % 100) as f32 / 100.0).collect();
        for target in [0.3, 0.45, 0.6] {
            let gain = fit_gain(&linear, target);
            let lumas: Vec<f32> = linear.chunks_exact(3).map(|p| p[1]).collect();
            let mean = lumas.iter().map(|&y| srgb_encode(tone(y, gain))).sum::<f32>() / lumas.len() as f32;
            assert!((mean - target).abs() < 0.01, "target {target}: got {mean}");
        }
    }
}
