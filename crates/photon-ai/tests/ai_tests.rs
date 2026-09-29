use photon_ai::backend::{Device, InferenceBackend, Tensor};
use photon_ai::manifest::ModelManifest;
use photon_ai::openvino::OpenVinoBackend;
use photon_ai::store::{ModelStatus, ModelStore};
use std::path::PathBuf;

#[test]
fn test_manifest_parses_and_validates() {
    let manifest = ModelManifest::load_embedded();
    assert!(!manifest.models.is_empty(), "Manifest must have at least one model");

    for m in &manifest.models {
        assert!(!m.id.is_empty(), "Model id cannot be empty");
        assert!(!m.task.is_empty(), "Model task cannot be empty");
        assert!(!m.file.is_empty(), "Model file cannot be empty");
        assert!(!m.url.is_empty(), "Model url cannot be empty");
        assert_eq!(m.sha256.len(), 64, "SHA-256 must be 64 hex characters for model {}", m.id);
        assert!(m.sha256.chars().all(|c| c.is_ascii_hexdigit()), "SHA-256 must be valid hex for model {}", m.id);
        assert!(!m.licence.is_empty(), "Licence cannot be empty for model {}", m.id);
        assert!(!m.licence_url.is_empty(), "Licence URL cannot be empty for model {}", m.id);
        assert!(m.size_bytes > 0, "Size bytes must be positive for model {}", m.id);
        assert!(!m.preferred_devices.is_empty(), "Preferred devices cannot be empty for model {}", m.id);
    }
}

#[test]
fn test_store_sha256_rejection() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let store = ModelStore::new(temp_dir.path().to_path_buf());

    // Create a dummy manifest
    let manifest_toml = r#"
[[model]]
id = "dummy-model"
task = "test"
file = "dummy.onnx"
url = "http://127.0.0.1:9/dummy.onnx"
sha256 = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
size_bytes = 100
licence = "MIT"
licence_url = "https://example.com/license"
source = "Test"
inputs = [[1, 3, 32, 32]]
preferred_devices = ["cpu"]
"#;
    let manifest = ModelManifest::parse(manifest_toml).unwrap();
    let spec = manifest.get_by_id("dummy-model").unwrap();

    // Write a corrupt file directly into the model path
    let model_file = temp_dir.path().join("dummy.onnx");
    std::fs::write(&model_file, b"corrupted data").unwrap();

    let status = store.status(spec);
    match status {
        ModelStatus::Corrupt { .. } => {}
        other => panic!("Expected Corrupt status, got {:?}", other),
    }

    // Verify verify() returns error due to size/hash mismatch
    assert!(store.verify(spec).is_err());
}

#[test]
fn test_openvino_cpu_fixture_execution() {
    match openvino::Core::new() {
        Ok(_) => println!("OpenVINO Core::new() succeeded!"),
        Err(e) => {
            println!("SKIPPING OpenVINO test: libopenvino_c.so not loadable on this system: {e:?}");
            return;
        }
    }

    let fixture_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("identity.onnx");

    if !fixture_path.exists() {
        println!("SKIPPING OpenVINO test: identity.onnx fixture does not exist yet.");
        return;
    }

    let backend = OpenVinoBackend::new();
    let mut model = backend
        .load(&fixture_path, Device::Cpu)
        .expect("Model fixture should load on CPU");

    let input_shapes = model.input_shapes();
    assert_eq!(input_shapes.len(), 1);
    assert_eq!(input_shapes[0], vec![1, 3, 32, 32]);

    // Create input tensor: 1x3x32x32 = 3072 floats
    let data: Vec<f32> = (0..3072).map(|i| i as f32 / 3072.0).collect();
    let input_tensor = Tensor::new(vec![1, 3, 32, 32], data.clone());

    let outputs = model.run(&[input_tensor]).expect("Inference should succeed");
    assert_eq!(outputs.len(), 1);
    assert_eq!(outputs[0].shape, vec![1, 3, 32, 32]);
    assert_eq!(outputs[0].data, data);
}
