# Brief: AI-0 (inference runtime) and AI-7 (image quality sorter)

For an AI agent or contributor implementing these two tasks in Photon.
Written 2026-09-29. Read this whole file before writing code.

---

## 1. Read first

1. `docs/TODO.md` §0 ("Read this first"): layout, commands, **rules for agents**,
   **invariants**. They are binding. The most important ones here:
   - **Open source only**, model *weights* included (OSI licence; no
     "research only"/"non-commercial" weights). No CUDA/TensorRT. Linux-firmware
     blobs for the GPU/NPU are accepted.
   - **Never touch the user's real photos, sidecars or
     `~/.local/share/photon/photon.db`.** Test on temp dirs / a copy.
     To run the app against a copy: `XDG_DATA_HOME=<tmp>/data XDG_CACHE_HOME=<tmp>/cache`
     (copy `~/.local/share/photon/photon.db` and `~/.cache/photon/thumbnails` there),
     and `dbus-run-session -- env GTK_A11Y=none …` so it doesn't attach to the user's
     running Photon (it is a single-instance GApplication).
   - `cargo build --offline` must be warning-free; `cargo test --offline` must pass.
   - Schema changes = a **new numbered migration** in `crates/photon-core/src/db/schema.rs`
     (the last one is 014). New `Preferences` fields need `#[serde(default)]`.
   - AI never overwrites an original or rewrites a sidecar. Ratings/rejects go
     through the existing paths (DB + `sidecar::write_image_xmp`).
   - **The maintainer commits. Do not commit.**
   - When done, update the status markers of AI-0 and AI-7 in `docs/TODO.md`.
2. `docs/TODO.md` PART 5 (AI-0 … AI-8), and R-16 (the sharpness score is shared).
3. Code to know (read before changing):
   - `crates/photon-import/src/thumbnails.rs` — `ThumbnailGenerator`, `thumb_path`,
     `ThumbSize::{Grid, Large}` (Large = 2560 px long edge, JPEG, keyed by content hash).
   - `crates/photon-import/src/engine.rs` — import pipeline; the thumbnail stage
     is where per-photo analysis can run during import.
   - `crates/photon-core/src/db/{schema.rs,queries.rs}`, `models.rs`.
   - `crates/photon-app/src/ui/window.rs` — the status card (`status_label`,
     `progress_bar`, `cancel_button`) used for long jobs; `check_library` shows the
     background-thread + `async_channel` pattern used throughout.
   - `crates/photon-app/src/ui/timeline.rs` — `cull`/`Cull` (rating/flag changes
     with undo + XMP sync + revert on failure). **Reuse it for rejects.**
   - `crates/photon-app/src/ui/preferences.rs` — `adw::PreferencesGroup` style.
   - `crates/photon-app/src/catalog.rs` — a small module in the style to follow.

## 2. The machine

- CPU: Intel Core Ultra 5 125H (Meteor Lake), 14 cores / 18 threads.
- GPU: Intel Arc iGPU (7 Xe cores), kernel drivers `xe`/`i915`, `/dev/dri/renderD128`.
- NPU: Intel NPU (Meteor Lake), kernel driver `intel_vpu`, `/dev/accel/accel0`.
- RAM: 16 GB, **shared by CPU and iGPU**.
- Fedora 45, Rust 1.98 (workspace `rust-version = 1.75`, edition 2021).
- Installed from Fedora repos: `openvino` 2026.0.0 (+ ONNX frontend, CPU and
  GPU plugins, auto/hetero/batch plugins), `intel-compute-runtime`,
  `intel-level-zero`, `oneapi-level-zero`, `intel-npu-driver` 1.32,
  `intel-npu-firmware`.

### Preflight — do this first, report the results

The install plan the user showed had **no OpenVINO NPU plugin** and **no
`openvino-devel`** in it. Check, don't assume:

```sh
rpm -q openvino intel-compute-runtime oneapi-level-zero intel-npu-driver intel-npu-firmware
rpm -ql openvino $(rpm -qa 'libopenvino*') | grep -E 'libopenvino_c\.so|plugins|npu' 
dnf repoquery 'libopenvino*npu*' 'openvino-devel'     # is an NPU plugin packaged at all?
ls -l /dev/dri/renderD128 /dev/accel/accel0            # user must be able to open both
```

- If `libopenvino_c.so` is missing, the C API (which the Rust crate needs) is
  in another package: find it with `dnf repoquery --whatprovides 'libopenvino_c.so*'`
  and tell the user which package to install. Don't install anything yourself.
- If no NPU plugin is packaged, **NPU is simply "unavailable"** in Photon (with
  the reason shown in Preferences). Don't build OpenVINO from source, and don't
  block on it. CPU and GPU are enough for AI-0 and AI-7.

## 3. AI-0 — inference runtime and model manager

### 3.1 Design decisions (already made — don't relitigate)

- **Backend: OpenVINO, used directly** through the `openvino` crate
  (Apache-2.0), not through ONNX Runtime. OpenVINO reads `.onnx` directly; one
  API covers CPU, GPU and NPU. ONNX Runtime may be added later behind the same
  trait for AMD/NVIDIA; **don't implement it now**.
- **Use the crate's runtime-linking mode** (`openvino` loads `libopenvino_c.so`
  with `dlopen`; check the crate docs for the exact feature name in the current
  version). Photon must **start and work normally without OpenVINO installed**;
  AI features then show as unavailable. No link-time dependency on OpenVINO.
- Check the latest `openvino` crate version supports the OpenVINO **2026.0**
  C API. If it doesn't, say so and stop; don't write raw FFI.
- New dependencies need network once (`cargo fetch`); the repo otherwise builds
  `--offline`. Tell the user which crates you added and their licences.

### 3.2 New crate `crates/photon-ai`

Add to the workspace. Depends on `photon-core` only if needed (keep it
independent of GTK). Suggested modules:

```
photon-ai/
  src/lib.rs          // pub API, re-exports
  src/backend.rs      // trait InferenceBackend + Device enum + tensor types
  src/openvino.rs     // OpenVINO implementation (only file that touches the crate)
  src/devices.rs      // detection: which devices exist, and why one is unavailable
  src/manifest.rs     // model manifest (serde), embedded with include_str!
  src/store.rs        // download, SHA-256 verify, cache dir, delete, sizes
  src/tiling.rs       // run a model over a large image in overlapping tiles
  models.toml         // the manifest (see 3.4)
  examples/bench.rs   // the benchmark (see 3.7)
  tests/…             // see 3.8
```

`InferenceBackend` (roughly — refine as needed):

```rust
pub enum Device { Cpu, Gpu, Npu }
pub trait InferenceBackend: Send + Sync {
    fn devices(&self) -> Vec<DeviceInfo>;             // present + usable, with names
    fn load(&self, model: &Path, device: Device) -> Result<Box<dyn LoadedModel>>;
}
pub trait LoadedModel: Send {
    fn input_shapes(&self) -> Vec<Vec<usize>>;
    fn run(&mut self, inputs: &[Tensor]) -> Result<Vec<Tensor>>;   // f32 NCHW
}
```

Errors: `anyhow` or a `thiserror` enum like the rest of the workspace; every
error message must say what to do ("OpenVINO isn't installed: dnf install openvino").

### 3.3 Devices

- Detect at startup, off the UI thread; cache the result.
- `DeviceInfo { device, name /* e.g. "Intel Arc Graphics" */, available, reason }`.
  `reason` explains unavailability in user terms ("install intel-npu-driver",
  "no NPU plugin in this OpenVINO build", "no permission for /dev/accel/accel0").
- **Compiled-model cache:** set OpenVINO's cache dir to
  `~/.cache/photon/openvino/` so GPU/NPU compilation (seconds) happens once per model.

### 3.4 Model manifest (`models.toml`, in the repo)

One entry per model:

```toml
[[model]]
id = "yunet-2023mar"
task = "face-detection"
file = "face_detection_yunet_2023mar.onnx"
url = "<exact upstream URL>"
sha256 = "<computed by you from the downloaded file>"
size_bytes = 0
licence = "MIT"
licence_url = "<link to the licence text for the weights>"
source = "OpenCV Zoo"
inputs = [[1, 3, 640, 640]]
preferred_devices = ["npu", "gpu", "cpu"]   # first available wins
```

- **You fill in `url`, `sha256`, `size_bytes` from the real file.** Never invent
  or copy a hash from memory. Verify the weights' licence yourself from the
  upstream repository and quote its location in `licence_url`.
- For AI-0, include only what the benchmark needs (3.7). YuNet (OpenCV Zoo) is
  the first model (AI-3 will use it).

### 3.5 Model store

- Location: `~/.local/share/photon/models/<file>`.
- **Downloads only after the user agrees** (a dialog naming the model, its size,
  source and licence). No background downloads, no telemetry, no other network access.
- Download to `<file>.part`, verify SHA-256 (`sha2` crate), then rename. A
  mismatch deletes the part file and reports it. Verify the hash again before
  every load (cheap compared with inference) — refuse a modified file.
- HTTP: `ureq` with rustls (both MIT/Apache) is fine. Check the licence of every
  new crate.
- API: `status(id) -> NotDownloaded | Ready { size } | Corrupt`, `download(id, progress_cb, cancel)`,
  `delete(id)`, `total_size()`.

### 3.6 Jobs, memory, UI

- One background job queue in the app (`crates/photon-app/src/ai_jobs.rs` or
  similar): a single worker thread per device class (don't run two GPU jobs at
  once), progress + cancel shown in the existing status card, like imports.
- Memory: 16 GB shared with the iGPU. Keep one job ≲ 2 GB. `tiling.rs` splits a
  large image into overlapping tiles (e.g. 512² with 32 px overlap) and blends
  the seams linearly; test it with an identity model (output == input exactly).
- **Preferences → new "AI" group** (in `preferences.rs`):
  - Devices: one row each (CPU / GPU / NPU) with name or reason.
  - "Use the NPU for background scans" switch (default on if available).
  - Models: list with status and size, Download…/Delete buttons, total size.
  - New `Preferences` fields with `#[serde(default)]`.
- Everything AI is **off until a model is downloaded**; the app must behave
  exactly as before for someone who never opens this group.

### 3.7 Benchmark — the first deliverable

`cargo run -p photon-ai --release --example bench -- --model <id> --image <path>`

- For each available device: model compile/load time (first run, and cached),
  then 3 warm-up runs and 10 timed runs → **median and max ms**, and **peak RSS**
  (read `/proc/self/status` VmHWM).
- Models/inputs:
  1. YuNet on a 640×640 resize of a **Large preview** (copy one from
     `~/.cache/photon/thumbnails/large/` to a temp dir — read-only use).
  2. A denoise model **only if** you find one with ready-made ONNX weights and a
     verified OSI licence (candidates in TODO AI-1: NAFNet, SCUNet). Run it tiled
     on a 20 MP image (e.g. a full-res render of an ORF sample copied to /tmp).
     If none qualifies, skip it and say so — don't convert models with PyTorch
     as part of this task.
- Write the numbers into `docs/TODO.md` under AI-0 as a table
  (model × device: load, median, max, peak memory), with the date and OpenVINO
  version. Then set each model's `preferred_devices` from the numbers.

### 3.8 Tests

- A tiny ONNX model (identity or a 1×1 conv, a few KB) in
  `crates/photon-ai/tests/fixtures/`. Commit the generator script next to it
  (`gen_fixture.py`, using the `onnx` Python package in a throwaway venv) so it
  can be regenerated; the tests themselves must not need Python.
- Tests: manifest parses and every entry has a licence and a 64-hex sha256;
  store rejects a file with the wrong hash; tiling round-trips an image through
  the identity model exactly (including edges and non-multiple sizes); loading
  and running the fixture on CPU.
- **Tests that need OpenVINO skip cleanly** (print why, return) when
  `libopenvino_c.so` isn't loadable, so `cargo test` passes on machines without it.
- No test downloads anything.

### 3.9 AI-0 is done when

- `cargo build --offline` warning-free, `cargo test --offline` green (after the
  one-time `cargo fetch`).
- Photon starts and works with OpenVINO uninstalled (test by running with
  `LD_LIBRARY_PATH` pointing nowhere useful, or on a toolbox without it).
- Preferences → AI shows the three devices correctly on this machine, can
  download/verify/delete YuNet.
- The benchmark table is in `docs/TODO.md`.

---

## 4. AI-7 — image quality sorter

**Do the classical part first; it needs no model and no AI-0.** The learned
score is optional (🟡) and comes last, only after the AI-0 benchmark.

### 4.1 Scores (pure Rust, in `photon-import`, e.g. `src/quality.rs`)

Computed from the **Large preview** (already on disk after import; never
decode the RAW for this), downscaled to a **fixed 1024 px long edge** so scores
don't depend on resolution:

- **Sharpness:** variance of the Laplacian of the luma. Photos are often sharp
  only on the subject, so compute it on a grid of tiles (e.g. 4×4) and keep
  the **maximum tile** (plus the global value, stored too). Use a 3×3 Laplacian.
- **Exposure clipping:** fraction of pixels with luma ≤ 2 (shadows) and ≥ 253
  (highlights), and the fraction with any channel at 255.
- **Mean luma** (for "very dark"/"very bright" hints).
- Version the algorithm: `quality_version = 1`, so a change can trigger recompute.

Unit tests with synthetic images made in the test (`image` crate): a sharp
checkerboard vs. the same with a Gaussian blur (blurred must score clearly
lower); black/white images for clipping; a photo that is sharp in one tile only
(max-tile score must stay high).

### 4.2 Storage

Migration **015**: a `image_quality` table (don't widen `images`):
`image_id INTEGER PRIMARY KEY REFERENCES images(id) ON DELETE CASCADE,
sharpness REAL, sharpness_global REAL, clip_shadows REAL, clip_highlights REAL,
mean_luma REAL, quality_version INTEGER, computed_at INTEGER`.
Queries in `queries.rs` with tests, like the existing ones.

### 4.3 When it's computed

- During import, in the thumbnail stage of `engine.rs` — **only if** it adds
  little: the Large preview may not exist yet at that point; if computing it
  would slow imports noticeably, don't. Measure on 100 photos and report.
- A backfill job ("Analyse photo quality", status card with progress and
  cancel) for photos without a current score, processing the Large previews in
  parallel with rayon. Photos whose file or preview is missing are skipped quietly.

### 4.4 Bursts and suggestions (the user-facing part)

- **Burst:** consecutive photos (by `created_at`) from the same camera
  (`camera_model`) with ≤ 2 s between neighbours, size ≥ 2. A pure function
  with unit tests (edge cases: equal timestamps, missing dates, missing camera).
- **"Suggest Rejects…"** (on the current timeline view and on a selection):
  within each burst, suggest frames with sharpness < 0.6 × the burst's best,
  and outside bursts, photos whose sharpness is in the bottom few percent of the
  library *and* below an absolute floor (tune on the user's library copy; put
  the constants in one place with a comment on how they were chosen). Also flag
  heavy clipping (e.g. > 25 % highlights clipped).
- Show the suggestions in a dialog: thumbnails grouped by burst, the best frame
  marked, each suggestion with a checkbox (checked by default) and the reason
  ("blurred — 38 % of the sharpest in its burst", "highlights clipped").
- **Nothing is rejected until the user confirms.** Confirming applies reject
  (`flagged = -1`) through the **existing** culling path (`timeline.rs` `cull` /
  `queries::batch_set_flag` + `sync_cull_to_xmp`), so it is undoable, writes
  `xmp:Rating="-1"`, and reverts with a toast on failure. Already-picked photos
  (`flagged = 1`) are never suggested.
- A filter in the header's filter popover: "Possibly blurred" (below threshold).
- Viewer info panel: show the sharpness (relative to its burst when in one).

### 4.5 Learned score (optional, later)

Only after AI-0's benchmark, and only with a model whose weights have a
verified OSI licence (NIMA-style aesthetic/technical models: check each;
many are research-only). Stored as another column with its own version.
If nothing qualifies, leave it 🟡 and say why in the TODO.

### 4.6 AI-7 is done when

- Scores, migration, queries, burst grouping and suggestion logic have unit tests.
- On a **copy** of the user's library: backfill runs with progress and cancel;
  "Suggest Rejects…" gives sensible results on a real burst (describe what you
  saw); confirming rejects them, Ctrl+Z restores them, and the XMP sidecars show
  `xmp:Rating="-1"` only for the confirmed ones.
- `docs/TODO.md` updated: AI-7 classical 🟢, learned 🟡 (or done), and the
  R-16 "sharpness score" item points to AI-7.

---

## 5. Order of work

1. Preflight (§2) → report.
2. AI-7 classical (§4.1–4.4) — independent of AI-0, immediately useful.
3. AI-0 crate, devices, manifest, store, tiling, tests (§3.2–3.5, 3.8).
4. Benchmark (§3.7) → numbers into the TODO.
5. Preferences → AI group and job queue (§3.6).
6. Optional: AI-7 learned score (§4.5).

After each step: build warning-free, run the tests, and stop to report if
anything in this brief turns out to be wrong for this machine (packages, crate
versions, licences) rather than working around it silently.

## 6. What to report back to the maintainer

- Files added/changed; new crates with versions and licences.
- Preflight results; benchmark table.
- Anything verified by hand in the GUI vs. only by tests (be explicit).
- Open questions and anything skipped, with the reason.
