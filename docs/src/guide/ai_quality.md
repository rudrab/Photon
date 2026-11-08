# AI Focus & Quality Analysis

Photon can check your photos for blur, missed eye focus, clipped exposure and — with the aesthetic model — how good they look as a whole, and suggest which shots to reject. All analysis runs **on your machine**. No photo, thumbnail, face or score is ever sent anywhere.

>[!NOTE]
>AI is **optional**. Without OpenVINO or the face model, Photon works normally. The whole-frame blur and exposure checks still run; only the eye-focus check is skipped. Nothing is ever rejected automatically: Photon *suggests* and you confirm.

---

## 1. What you need

| Requirement | Needed for | Notes |
| :--- | :--- | :--- |
| **OpenVINO runtime** (`libopenvino_c.so`) | Face and eye-focus detection | Loaded at run time; not needed to build or start Photon. Tested with 2026.0. |
| **oneTBB** (`libtbb.so.12`) | OpenVINO itself | On Fedora the `openvino` package does not pull it in; install `tbb`. |
| **YuNet face model** (about 230 KB) | Face and eye-focus detection | Not bundled. Downloaded once, after you confirm. |
| **Aesthetic model** (about 612 MB: CLIP ViT-L/14 image encoder + LAION aesthetic head) | The aesthetic score | Not bundled. Downloaded once, after you confirm. A GPU makes it several times faster. |
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
| CLIP ViT-L/14 image encoder (fp16 ONNX) | Image embedding for the aesthetic score | MIT | [OpenAI CLIP](https://github.com/openai/CLIP), ONNX export by [Xenova](https://huggingface.co/Xenova/clip-vit-large-patch14) |
| LAION aesthetic predictor (sac+logos+ava1-l14-linearMSE) | Aesthetic score from the embedding | Apache-2.0 | [improved-aesthetic-predictor](https://github.com/christophschuhmann/improved-aesthetic-predictor) |

>[!TIP]
>Photon only makes network requests to download a model you asked for. Delete it from **Preferences → AI** at any time.

### Speed

Measured on a Core Ultra 5 125H, 640 × 640 input:

| Device | Median per frame |
| :--- | :--- |
| CPU (default) | 4.6 ms |
| Intel Arc GPU | 2.8 ms, but about 460 MB more memory |

Decoding the preview takes longer than either, which is why the CPU is the default.

The aesthetic model is far heavier (about 80 GFLOPs per photo), so it prefers the GPU and falls back to the CPU. Measured on the same machine, per photo including decoding a full-size JPEG: about 0.1–0.25 s on the Arc GPU, about 0.8 s on the CPU.

---

## 2. Running the analysis

Choose **Analyse Photo Quality…** from the menu. The first time, Photon offers to download the face model and the aesthetic model; either can be skipped. Photon scores the photos in your selection, or in the current view if nothing is selected (a day, event, album, tag or search). Progress is shown, and you can stop at any time. When it finishes, the results list:

* **Technical faults** (blur, missed focus, clipping): ticked, ready to reject.
* **Weak-looking photos** (a low aesthetic score): listed but **not ticked**. Taste is yours; tick the ones you agree with.
* **Already rejected** photos in the selection or view: ticked. **Untick one to restore it** (un-reject it). This is how you take back a single reject made here or by hand; Ctrl+Z takes back the whole last batch.

The button reads, for example, *Reject 3 · Restore 1*.

**Move Rejects to Trash…** at the bottom applies the same choices, then moves every photo that is rejected after them — the ones you just ticked and the ones still ticked under *Already Rejected* — to the system Trash and removes them from the library. It asks first, just as **Move to Trash** does, and follows **Preferences → Move to Trash Removes** for RAW+JPG shots. Restore them from the Trash, or with Ctrl+Z.

* Scores are stored, so later runs only score new photos (and, once a model is downloaded, the photos scored before it). **Analyse Again** re-scores everything.
* A **Possibly blurred** filter shows the suspect photos in the grid.
* Rejects and restores are written like a manual reject or unflag: to the catalogue and the XMP sidecar, and you can undo them (one Ctrl+Z for the rejects, one for the restores).
* Photon never suggests a pick, a photo you have already rejected, or the best frame of a burst.
* The viewer's info panel shows a photo's aesthetic score and how it ranks in your library.

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

### Aesthetic score (needs the aesthetic model)

Sharpness and exposure say whether a photo is technically sound, not whether it is any good. The aesthetic score judges the photo as a whole — composition, light, subject, colour — on a 1–10 scale:

1. The CLIP ViT-L/14 image encoder turns the photo into a 768-number description of what it shows and how.
2. The LAION aesthetic predictor, trained on human ratings of photos, maps that to the mean rating people would give it.

Most everyday photos score between 4 and 6.5. CLIP looks at the **centre square** of the photo (shortest edge scaled to 224 px), as it was trained, so what lies near the long edges of a wide photo is not seen. It is one number with no explanation: it can't tell you *what* is wrong with a photo.

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
| **Looks weak** (not ticked) | outside a burst: its aesthetic score is in the lowest 10 % of your library (needs 30 or more scored photos), and no crop lifts it out (below) |
| **Weaker frame** (not ticked) | in a burst, a sharp frame scores 0.5 or more below the best-looking sharp frame, even at its best crop |

If a session has too few face shots, the library's 80th percentile is used instead.

In a burst, the frame marked **★ Best** is the best-looking of the sharp frames as shot, or the sharpest when the aesthetic model isn't used.

### A weak frame that would be strong cropped

A photo can look weak as shot and be a keeper once cropped. Before offering a photo as *looks weak* or *weaker frame*, Photon tries about 20 square crops of it (full height, 85 % and 70 % of the short edge, at the left, centre and right, top, middle and bottom) and scores each one. With the face model, only crops that keep the **largest face whole** count: the aesthetic score on its own happily cuts a face in half.

* If the best crop, less 0.2, still beats the photo as shot, the crop counts. The 0.2 allowance is there because the best of 20 tries beats the frame by about 0.2 through luck alone. If that lifts the photo out of the weak range, it is not suggested.
* The info panel then says how, for example *Stronger cropped (5.5): a square from the left, keeping 54% of the frame*, and the results show *aesthetic 5.1, 5.5 cropped*.
* **Photon never crops.** The crop is a hint; do it in darktable if you agree.
* This only runs on photos that would otherwise be offered as weak, after scoring (about 2 s each on a GPU, much longer on the CPU), with its own progress and Stop.
* Only square crops can be judged, because the model sees a square. With several people, only the largest face is protected.

---

## 5. Limits

* Motion blur that leaves sharp specular highlights can score higher than it looks.
* Only the largest face in a frame is measured.
* The aesthetic score reflects the taste of the people who rated the training photos (photography-site and web users), not yours, and sees only the centre square.
* Analysis is not run during import.
* The thresholds are tuned on one photographer's library. Treat the suggestions as a shortlist and check them in the viewer before confirming.
