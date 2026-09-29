//! Abstractions for inference backends, devices, and tensors.

use anyhow::Result;
use std::fmt;
use std::path::Path;

/// Target hardware device for inference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Device {
    Cpu,
    Gpu,
    Npu,
}

impl Device {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Cpu => "CPU",
            Self::Gpu => "GPU",
            Self::Npu => "NPU",
        }
    }
}

impl fmt::Display for Device {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Information about hardware device presence and availability.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DeviceInfo {
    pub device: Device,
    pub name: String,
    pub available: bool,
    pub reason: Option<String>,
}

/// N-dimensional tensor with f32 data.
#[derive(Debug, Clone, PartialEq)]
pub struct Tensor {
    pub shape: Vec<usize>,
    pub data: Vec<f32>,
}

impl Tensor {
    pub fn new(shape: Vec<usize>, data: Vec<f32>) -> Self {
        let expected_len: usize = shape.iter().product();
        assert_eq!(
            expected_len,
            data.len(),
            "Tensor data length {} does not match shape {:?} (expected {})",
            data.len(),
            shape,
            expected_len
        );
        Self { shape, data }
    }

    pub fn zeros(shape: Vec<usize>) -> Self {
        let len: usize = shape.iter().product();
        Self {
            shape,
            data: vec![0.0f32; len],
        }
    }
}

/// An inference backend capable of listing devices and loading models.
pub trait InferenceBackend: Send + Sync {
    /// List present hardware devices and their availability status.
    fn devices(&self) -> Vec<DeviceInfo>;

    /// Load and compile a model for the specified device.
    fn load(&self, model_path: &Path, device: Device) -> Result<Box<dyn LoadedModel>>;
}

/// An instantiated, compiled model ready to run inference.
pub trait LoadedModel: Send {
    /// Expected shapes for each input tensor.
    fn input_shapes(&self) -> Vec<Vec<usize>>;

    /// The names of the outputs, in the order `run` returns them.
    fn output_names(&self) -> Vec<String>;

    /// Execute inference with the given input tensors and return output tensors.
    fn run(&mut self, inputs: &[Tensor]) -> Result<Vec<Tensor>>;
}
