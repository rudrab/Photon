# AI Focus & Quality Analysis

Photon can check your photos for blur, missed eye focus, and clipped exposure, and suggest which shots to reject. All analysis runs **on your machine**. No photo, thumbnail, face or score is ever sent anywhere.

>[!NOTE]
>AI is **optional**. Without OpenVINO or the face model, Photon works normally. The whole-frame blur and exposure checks still run; only the eye-focus check is skipped. Nothing is ever rejected automatically: Photon *suggests* and you confirm.

---

## 1. What you need

| Requirement | Needed for | Notes |
| :--- | :--- | :--- |
| **OpenVINO runtime** (`libopenvino_c.so`) | Face and eye-focus detection | Loaded at run time; not needed to build or start Photon. Tested with 2026.0. |
| **oneTBB** (`libtbb.so.12`) | OpenVINO itself | On Fedora the `openvino` package does not pull it in; install `tbb`. |
| **YuNet face model** (about 230 KB) | Face and eye-focus detection | Not bundled. Downloaded once, after you confirm. |
| Intel GPU / NPU drivers | Optional acceleration | The CPU is the default and is fast enough (see below). |

### Install OpenVINO

```bash
# Fedora
sudo dnf install openvino tbb
```

Other distributions: install an OpenVINO 2026.x runtime that provides `libopenvino_c.so` (from your distribution, or Intel's archives), plus oneTBB.

Optional acceleration, for Intel hardware only:

```bash
sudo dnf install intel-compute-runtime   # Intel GPU
sudo usermod -aG render "$USER"          # permission to use /dev/dri (log out and in)
```

>[!WARNING]
>**NPU:** Fedora's OpenVINO does not yet ship the NPU plugin (`libopenvino_intel_npu_plugin.so`), so the NPU cannot be used even with `intel-npu-driver` installed. Photon reports this in Preferences.

### Download the model

1. Open **Preferences → AI**.
2. Under **AI Models**, choose **Download** on *YuNet*. Photon downloads it only after you confirm.
3. The file is saved to `~/.local/share/photon/models/` and checked against a fixed SHA-256 before it is used, and again before every load. A corrupt or altered file is refused.

The same page lists the devices OpenVINO found (CPU, GPU, NPU). For any device it does not list, Photon says why: a missing driver, a missing plugin, or a missing permission.

| Model | Task | Licence | Source |
| :--- | :--- | :--- | :--- |
| YuNet 2023mar | Face detection with 5 landmarks | MIT | [OpenCV Zoo](https://github.com/opencv/opencv_zoo/tree/main/models/face_detection_yunet) |

>[!TIP]
>Photon only makes network requests to download a model you asked for. Delete it from **Preferences → AI** at any time.

### Speed

Measured on a Core Ultra 5 125H, 640 × 640 input:

| Device | Median per frame |
| :--- | :--- |
| CPU (default) | 4.6 ms |
| Intel Arc GPU | 2.8 ms, but about 460 MB more memory |

Decoding the preview takes longer than either, which is why the CPU is the default.

---

## 2. Running the analysis

Choose **Analyse Photo Quality…** from the menu. Photon scores the photos in your selection, or in the current view if nothing is selected (a day, event, album, tag or search). Progress is shown, and you can stop at any time. When it finishes, the photos it suggests rejecting are listed for you to confirm.

* Scores are stored, so later runs only score new photos. **Analyse Again** re-scores everything.
* A **Possibly blurred** filter shows the suspect photos in the grid.
* Confirmed rejects are written like a manual reject: to the catalogue and the XMP sidecar, and you can undo them.
* Photon never suggests a pick, a photo you have already rejected, or the best frame of a burst.

---

## 3. What is measured

All measurements use the **Large preview** (scaled to 1024 px), so RAW files are not fully decoded.

### Whole-frame sharpness

The variance of the Laplacian, taken over a 4 × 4 grid of tiles. The sharpest tile counts, so a sharp subject on a soft background is not penalised.

### Eye sharpness (needs the face model)

Whole-frame sharpness cannot tell a blurred face in front of a detailed background from a sharp one. So, when a face model is available:

1. YuNet finds the faces; the largest is treated as the subject.
2. The area around both eyes is cropped and scaled to a fixed width, so faces of different sizes compare fairly.
3. The variance of the Laplacian of that crop measures the micro-contrast of the iris and lashes.

### Exposure

* **Blown highlights:** more than 25 % of pixels at luma 253 or above.
* **Crushed shadows:** more than 35 % of pixels at luma 2 or below.

A saturated colour is not counted as clipped.

---

## 4. When a shot is suggested for rejection

Photon judges each **shot**, not each file: a RAW and its JPEG companion count once and are suggested together.

| Situation | A shot is suggested when… |
| :--- | :--- |
| **Burst** (2 or more shots, same camera, up to 2 s apart) | its sharpness is below 60 % of the burst's sharpest |
| **Not in a burst** | its sharpness is below 8 % of your library's median |
| **Faces, in a session** (same camera, within ±30 min, 3 or more face shots) | its eye sharpness is below 50 % of the session's 80th percentile |
| **Faces, in a burst** | its eye sharpness is below 60 % of the sharpest eyes in the burst |
| **Exposure** | highlights or shadows are clipped beyond the limits above |

If a session has too few face shots, the library's 80th percentile is used instead.

---

## 5. Limits

* Motion blur that leaves sharp specular highlights can score higher than it looks.
* Only the largest face in a frame is measured.
* Analysis is not run during import.
* The thresholds are tuned on one photographer's library. Treat the suggestions as a shortlist and check them in the viewer before confirming.
