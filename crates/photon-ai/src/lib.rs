//! photon-ai: Inference runtime, model manager, and hardware acceleration for Photon.

pub mod backend;
pub mod devices;
pub mod faces;
pub mod manifest;
pub mod openvino;
pub mod store;
pub mod tiling;

pub use backend::{Device, DeviceInfo, InferenceBackend, LoadedModel, Tensor};
pub use manifest::{ModelManifest, ModelSpec};
pub use store::{ModelStatus, ModelStore};
pub use tiling::tile_and_run;
