//! OpenVINO inference backend using runtime linking (AI-0).
//!
//! This is the only file that touches the `openvino` crate. If `libopenvino_c.so`
//! is not present on the system, calls fail gracefully with clear instructions.

use crate::backend::{Device, DeviceInfo, InferenceBackend, LoadedModel, Tensor};
use crate::devices::{cpu_name, why_openvino_failed, why_unavailable};
use anyhow::{bail, Context, Result};
use openvino::{Core, DeviceType, ElementType, PropertyKey, RwPropertyKey, Shape, Tensor as OvTensor};
use std::path::{Path, PathBuf};

/// OpenVINO inference engine backend.
pub struct OpenVinoBackend {
    cache_dir: Option<PathBuf>,
}

impl OpenVinoBackend {
    pub fn new() -> Self {
        let cache_dir = dirs::cache_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("photon")
            .join("openvino");
        let _ = std::fs::create_dir_all(&cache_dir);
        Self {
            cache_dir: Some(cache_dir),
        }
    }

    pub fn with_cache_dir(cache_dir: PathBuf) -> Self {
        let _ = std::fs::create_dir_all(&cache_dir);
        Self {
            cache_dir: Some(cache_dir),
        }
    }

    /// Check if OpenVINO C library is loadable via runtime linking.
    pub fn is_available() -> bool {
        Core::new().is_ok()
    }
}

impl Default for OpenVinoBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl InferenceBackend for OpenVinoBackend {
    /// Ask OpenVINO which devices it can run on (`GPU`, or `GPU.0`, `GPU.1`
    /// with several), with their names; explain the others.
    fn devices(&self) -> Vec<DeviceInfo> {
        const ALL: [Device; 3] = [Device::Cpu, Device::Gpu, Device::Npu];
        let fallback_name = |device: Device| match device {
            Device::Cpu => cpu_name().unwrap_or_else(|| "CPU".into()),
            Device::Gpu => "GPU".into(),
            Device::Npu => "NPU".into(),
        };
        let core = match Core::new() {
            Ok(core) => core,
            Err(e) => {
                let reason = why_openvino_failed(&format!("{e:?}"));
                return ALL
                    .iter()
                    .map(|&device| DeviceInfo {
                        device,
                        name: fallback_name(device),
                        available: false,
                        reason: Some(reason.clone()),
                    })
                    .collect();
            }
        };
        let listed: Vec<String> = match core.available_devices() {
            Ok(list) => list.iter().map(|d| d.as_ref().to_string()).collect(),
            Err(e) => {
                log::warn!("OpenVINO couldn't list its devices: {e}");
                Vec::new()
            }
        };

        ALL.iter()
            .map(|&device| {
                let kind = device.as_str();
                let found = listed.iter().find(|n| *n == kind || n.starts_with(&format!("{kind}.")));
                match found {
                    Some(ov_name) => DeviceInfo {
                        device,
                        name: core
                            .get_property(&DeviceType::from(ov_name.as_str()), &PropertyKey::DeviceFullName)
                            .unwrap_or_else(|_| fallback_name(device)),
                        available: true,
                        reason: None,
                    },
                    None => DeviceInfo {
                        device,
                        name: fallback_name(device),
                        available: false,
                        // `versions` fails when no plugin is registered for the device.
                        reason: Some(why_unavailable(device, core.versions(kind).is_ok())),
                    },
                }
            })
            .collect()
    }

    fn load(&self, model_path: &Path, device: Device) -> Result<Box<dyn LoadedModel>> {
        if !model_path.exists() {
            bail!("Model file not found at {}", model_path.display());
        }

        let mut core = Core::new().context(
            "Failed to initialize OpenVINO runtime (is openvino installed? dnf install openvino)",
        )?;

        let device_str = device.as_str();
        let dev_type: DeviceType = device_str.into();

        // Configure compiled model cache directory if available
        if let Some(ref cache_dir) = self.cache_dir {
            let cache_str = cache_dir.to_string_lossy();
            let _ = core.set_property(&dev_type, &RwPropertyKey::CacheDir, &cache_str);
        }

        let model_path_str = model_path.to_str().context("invalid UTF-8 model path")?;
        let model = core
            .read_model_from_file(model_path_str, "")
            .with_context(|| format!("Failed to read model from {}", model_path.display()))?;

        let mut compiled = core
            .compile_model(&model, dev_type)
            .with_context(|| format!("Failed to compile model for device {device_str}"))?;

        let infer_request = compiled
            .create_infer_request()
            .context("Failed to create OpenVINO infer request")?;

        // Retrieve input shapes
        let mut input_shapes = Vec::new();
        let num_inputs = compiled.get_input_size().unwrap_or(1);
        for i in 0..num_inputs {
            if let Ok(input_node) = compiled.get_input_by_index(i) {
                if let Ok(shape) = input_node.get_shape() {
                    let dims: Vec<usize> = shape.get_dimensions().iter().map(|&d| d as usize).collect();
                    input_shapes.push(dims);
                }
            }
        }

        let mut output_names = Vec::new();
        for i in 0..compiled.get_output_size().unwrap_or(0) {
            output_names.push(compiled.get_output_by_index(i).and_then(|n| n.get_name()).unwrap_or_default());
        }

        Ok(Box::new(OpenVinoModel {
            compiled,
            infer_request,
            input_shapes,
            output_names,
            device,
        }))
    }
}

struct OpenVinoModel {
    compiled: openvino::CompiledModel,
    infer_request: openvino::InferRequest,
    input_shapes: Vec<Vec<usize>>,
    output_names: Vec<String>,
    device: Device,
}

impl LoadedModel for OpenVinoModel {
    fn input_shapes(&self) -> Vec<Vec<usize>> {
        self.input_shapes.clone()
    }

    fn output_names(&self) -> Vec<String> {
        self.output_names.clone()
    }

    fn run(&mut self, inputs: &[Tensor]) -> Result<Vec<Tensor>> {
        if inputs.is_empty() {
            bail!("No input tensors provided for inference");
        }

        for (idx, input) in inputs.iter().enumerate() {
            let i64_shape: Vec<i64> = input.shape.iter().map(|&d| d as i64).collect();
            let shape = Shape::new(&i64_shape)?;
            let mut ov_tensor = OvTensor::new(ElementType::F32, &shape)?;
            let buffer = ov_tensor.get_data_mut::<f32>()?;
            buffer.copy_from_slice(&input.data);
            self.infer_request.set_input_tensor_by_index(idx, &ov_tensor)?;
        }

        self.infer_request
            .infer()
            .with_context(|| format!("Inference execution failed on {}", self.device))?;

        let num_outputs = self.compiled.get_output_size().unwrap_or(1);
        let mut outputs = Vec::with_capacity(num_outputs);

        for idx in 0..num_outputs {
            let out_tensor = self.infer_request.get_output_tensor_by_index(idx)?;
            let shape = out_tensor.get_shape()?;
            let dims: Vec<usize> = shape.get_dimensions().iter().map(|&d| d as usize).collect();
            let data = out_tensor.get_data::<f32>()?.to_vec();
            outputs.push(Tensor::new(dims, data));
        }

        Ok(outputs)
    }
}
