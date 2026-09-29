//! Overlapping tile inference for large images (AI-0).
//!
//! Splits large images into overlapping tiles, executes inference on each tile,
//! and blends the results linearly across overlap seams to prevent tile boundary artifacts.

use crate::backend::Tensor;
use anyhow::{bail, Result};

/// Default tile size for neural image models (e.g. denoise).
pub const DEFAULT_TILE_SIZE: usize = 512;
/// Default overlap in pixels between adjacent tiles.
pub const DEFAULT_OVERLAP: usize = 32;

/// Run a model function `f` over an input tensor `[1, C, H, W]` in overlapping tiles.
pub fn tile_and_run(
    input: &Tensor,
    tile_size: usize,
    overlap: usize,
    mut f: impl FnMut(&Tensor) -> Result<Tensor>,
) -> Result<Tensor> {
    if input.shape.len() != 4 || input.shape[0] != 1 {
        bail!(
            "Input tensor must be 4D [1, C, H, W], got shape {:?}",
            input.shape
        );
    }

    let _channels = input.shape[1];
    let height = input.shape[2];
    let width = input.shape[3];

    // If image is smaller than or equal to tile_size, run directly
    if height <= tile_size && width <= tile_size {
        return f(input);
    }

    let stride = tile_size.saturating_sub(overlap).max(1);

    // Compute tile coordinate ranges
    let mut y_starts = Vec::new();
    let mut y = 0;
    while y < height {
        let y0 = if y + tile_size > height && height >= tile_size {
            height - tile_size
        } else {
            y
        };
        y_starts.push(y0);
        if y0 + tile_size >= height {
            break;
        }
        y += stride;
    }
    y_starts.dedup();

    let mut x_starts = Vec::new();
    let mut x = 0;
    while x < width {
        let x0 = if x + tile_size > width && width >= tile_size {
            width - tile_size
        } else {
            x
        };
        x_starts.push(x0);
        if x0 + tile_size >= width {
            break;
        }
        x += stride;
    }
    x_starts.dedup();

    // Run first tile to determine output channel count and scale factor
    let first_tile_y0 = y_starts[0];
    let first_tile_x0 = x_starts[0];
    let first_tile_h = (height - first_tile_y0).min(tile_size);
    let first_tile_w = (width - first_tile_x0).min(tile_size);

    let first_tile_in = extract_crop(input, first_tile_y0, first_tile_x0, first_tile_h, first_tile_w);
    let first_tile_out = f(&first_tile_in)?;

    if first_tile_out.shape.len() != 4 || first_tile_out.shape[0] != 1 {
        bail!(
            "Model output tensor must be 4D [1, C, H, W], got shape {:?}",
            first_tile_out.shape
        );
    }

    let out_channels = first_tile_out.shape[1];
    let out_h_ratio = first_tile_out.shape[2] as f64 / first_tile_h as f64;
    let out_w_ratio = first_tile_out.shape[3] as f64 / first_tile_w as f64;

    let total_out_h = (height as f64 * out_h_ratio).round() as usize;
    let total_out_w = (width as f64 * out_w_ratio).round() as usize;

    let mut accum_out = vec![0.0f32; out_channels * total_out_h * total_out_w];
    let mut accum_weight = vec![0.0f32; total_out_h * total_out_w];

    for &y0 in &y_starts {
        let th = (height - y0).min(tile_size);
        for &x0 in &x_starts {
            let tw = (width - x0).min(tile_size);

            let tile_in = extract_crop(input, y0, x0, th, tw);
            let tile_out = f(&tile_in)?;

            let out_th = tile_out.shape[2];
            let out_tw = tile_out.shape[3];

            let out_y0 = (y0 as f64 * out_h_ratio).round() as usize;
            let out_x0 = (x0 as f64 * out_w_ratio).round() as usize;

            // Generate 2D trapezoidal blending weight
            let is_top = y0 == 0;
            let is_bottom = y0 + th == height;
            let is_left = x0 == 0;
            let is_right = x0 + tw == width;

            let weight_map = compute_weight_map(
                out_th,
                out_tw,
                (overlap as f64 * out_h_ratio).round() as usize,
                is_top,
                is_bottom,
                is_left,
                is_right,
            );

            // Accumulate tile output
            for c in 0..out_channels {
                let out_c_offset = c * total_out_h * total_out_w;
                let tile_c_offset = c * out_th * out_tw;

                for dy in 0..out_th {
                    let dst_y = out_y0 + dy;
                    if dst_y >= total_out_h {
                        continue;
                    }
                    for dx in 0..out_tw {
                        let dst_x = out_x0 + dx;
                        if dst_x >= total_out_w {
                            continue;
                        }

                        let w = weight_map[dy * out_tw + dx];
                        let val = tile_out.data[tile_c_offset + dy * out_tw + dx];

                        accum_out[out_c_offset + dst_y * total_out_w + dst_x] += val * w;
                    }
                }
            }

            for dy in 0..out_th {
                let dst_y = out_y0 + dy;
                if dst_y >= total_out_h {
                    continue;
                }
                for dx in 0..out_tw {
                    let dst_x = out_x0 + dx;
                    if dst_x >= total_out_w {
                        continue;
                    }
                    let w = weight_map[dy * out_tw + dx];
                    accum_weight[dst_y * total_out_w + dst_x] += w;
                }
            }
        }
    }

    // Normalize accumulated output by weight map
    for c in 0..out_channels {
        let c_offset = c * total_out_h * total_out_w;
        for i in 0..(total_out_h * total_out_w) {
            let w = accum_weight[i];
            if w > 0.0 {
                accum_out[c_offset + i] /= w;
            }
        }
    }

    Ok(Tensor::new(
        vec![1, out_channels, total_out_h, total_out_w],
        accum_out,
    ))
}

fn extract_crop(tensor: &Tensor, y0: usize, x0: usize, h: usize, w: usize) -> Tensor {
    let channels = tensor.shape[1];
    let full_h = tensor.shape[2];
    let full_w = tensor.shape[3];

    let mut crop_data = Vec::with_capacity(channels * h * w);
    for c in 0..channels {
        let c_offset = c * full_h * full_w;
        for y in y0..(y0 + h) {
            let row_offset = c_offset + y * full_w;
            crop_data.extend_from_slice(&tensor.data[row_offset + x0..row_offset + x0 + w]);
        }
    }

    Tensor::new(vec![1, channels, h, w], crop_data)
}

fn compute_weight_map(
    h: usize,
    w: usize,
    overlap: usize,
    is_top: bool,
    is_bottom: bool,
    is_left: bool,
    is_right: bool,
) -> Vec<f32> {
    let ov = overlap.max(1) as f32;
    let mut weights = vec![1.0f32; h * w];

    for y in 0..h {
        let wy = if !is_top && y < overlap {
            (y + 1) as f32 / ov
        } else if !is_bottom && y >= h - overlap {
            (h - y) as f32 / ov
        } else {
            1.0
        };

        for x in 0..w {
            let wx = if !is_left && x < overlap {
                (x + 1) as f32 / ov
            } else if !is_right && x >= w - overlap {
                (w - x) as f32 / ov
            } else {
                1.0
            };

            weights[y * w + x] = (wy * wx).clamp(0.001, 1.0);
        }
    }

    weights
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiling_identity_model_roundtrips_exactly() {
        // Create an image tensor of non-multiple dimensions: 3 channels, 750 x 920
        let h = 750;
        let w = 920;
        let mut data = Vec::with_capacity(3 * h * w);
        for c in 0..3 {
            for y in 0..h {
                for x in 0..w {
                    data.push(((c * 1000 + y * w + x) % 256) as f32 / 255.0);
                }
            }
        }

        let input = Tensor::new(vec![1, 3, h, w], data);

        // Identity model: returns input unchanged
        let output = tile_and_run(&input, 256, 32, |tile| Ok(tile.clone())).unwrap();

        assert_eq!(output.shape, input.shape);
        assert_eq!(output.data.len(), input.data.len());

        for i in 0..input.data.len() {
            let diff = (output.data[i] - input.data[i]).abs();
            assert!(
                diff < 1e-4,
                "Mismatch at index {}: expected {}, got {} (diff {})",
                i,
                input.data[i],
                output.data[i],
                diff
            );
        }
    }
}
