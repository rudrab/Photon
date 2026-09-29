//! Why an inference device isn't usable, in terms the user can act on (AI-0).
//!
//! Which devices *are* usable comes from OpenVINO itself (`openvino.rs`);
//! this module only explains the ones it doesn't list, from what is on the
//! system: device nodes, their permissions, and the library OpenVINO failed
//! to load.

use crate::backend::Device;
use std::fs::File;
use std::path::Path;

/// The kernel device node a device needs, if any.
fn device_node(device: Device) -> Option<&'static str> {
    match device {
        Device::Cpu => None,
        Device::Gpu => Some("/dev/dri/renderD128"),
        Device::Npu => Some("/dev/accel/accel0"),
    }
}

/// Why OpenVINO doesn't list `device`. `has_plugin`: whether OpenVINO has a
/// plugin for this kind of device at all.
pub fn why_unavailable(device: Device, has_plugin: bool) -> String {
    if let Some(node) = device_node(device) {
        if !Path::new(node).exists() {
            return match device {
                Device::Npu => "No NPU found (the intel_vpu driver or NPU firmware is missing)".into(),
                _ => format!("No GPU found ({node} doesn't exist)"),
            };
        }
        if let Err(e) = File::open(node) {
            return format!("No permission to use {node} ({e}); add your user to the 'render' group");
        }
    }
    match (device, has_plugin) {
        (Device::Cpu, _) => "OpenVINO's CPU plugin is missing (dnf install openvino)".into(),
        (Device::Gpu, false) => "This OpenVINO build has no GPU plugin".into(),
        (Device::Gpu, true) => "The GPU compute runtime is missing (dnf install intel-compute-runtime)".into(),
        (Device::Npu, false) => "This OpenVINO build has no NPU plugin".into(),
        (Device::Npu, true) => "The NPU user-space driver is missing (dnf install intel-npu-driver)".into(),
    }
}

/// A user-facing reason from the error OpenVINO's C library failed to load
/// with, e.g. "the shared library at …/libopenvino_c.so.2600 could not be
/// opened: libtbb.so.12: cannot open shared object file".
pub fn why_openvino_failed(error: &str) -> String {
    const MISSING: &str = ": cannot open shared object file";
    if let Some(end) = error.find(MISSING) {
        let lib = error[..end].rsplit([' ', ':']).next().unwrap_or("").trim();
        if !lib.is_empty() && !lib.contains("openvino_c") {
            let hint = if lib.starts_with("libtbb") { " (dnf install tbb)" } else { "" };
            return format!("OpenVINO can't start: {lib} is missing{hint}");
        }
    }
    if error.contains("openvino_c") || error.contains("Cannot find") || error.contains("not found") {
        return "OpenVINO isn't installed (dnf install openvino)".into();
    }
    format!("OpenVINO can't start: {error}")
}

pub fn cpu_name() -> Option<String> {
    let content = std::fs::read_to_string("/proc/cpuinfo").ok()?;
    content
        .lines()
        .find_map(|line| line.strip_prefix("model name"))
        .and_then(|rest| rest.split_once(':'))
        .map(|(_, name)| name.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_the_missing_library() {
        let e = "Loading(SystemFailure(\"the shared library at /usr/lib64/libopenvino_c.so.2600 could not be \
                 opened: libtbb.so.12: cannot open shared object file: No such file or directory\"))";
        assert_eq!(why_openvino_failed(e), "OpenVINO can't start: libtbb.so.12 is missing (dnf install tbb)");

        let e = "the shared library at libopenvino_c.so could not be opened: libopenvino_c.so: cannot open \
                 shared object file: No such file or directory";
        assert_eq!(why_openvino_failed(e), "OpenVINO isn't installed (dnf install openvino)");
    }
}
