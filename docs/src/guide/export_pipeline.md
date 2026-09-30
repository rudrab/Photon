# Export & Production Pipeline

Photon includes a full-featured production export engine designed for delivering finished assets to clients, web galleries, or print labs.

---

## 1. Export Formats & Depth

* **JPEG:** Optimized 8-bit JPEG with user-selectable quality slider (`1–100`) and ICC profile embedding.
* **TIFF (8-bit / 16-bit):** High-precision uncompressed or LZW-compressed TIFFs. 16-bit TIFFs use a true 16-bit decode pipeline from raw files.
* **WebP:** Native modern WebP encoding with lossy or lossless compression.

---

## 2. Output Resizing & Sharpening

* **Dimension Constraints:**
  * Original resolution.
  * Fit within bounds (e.g. `2048 x 2048 px` for web).
  * Long edge or short edge limits.
* **Unsharp Mask (USM):**
  * Applies edge sharpening post-resize to compensate for downsampling softness.
  * Tuned modes for **Screen (Web)** and **Print**.

---

## 3. Watermarking

Photon includes flexible watermark blending directly in the export dialog:
* **Text Watermark:** Custom text string, font selection, size, opacity slider (`0–100%`), and 9-point anchor placement (Top-Left, Center, Bottom-Right, etc.).
* **Image / Logo Watermark:** Upload a PNG logo with alpha transparency, set scaling factor, margin, and opacity.

---

## 4. Metadata Preservation & Privacy Stripping

* **Embedded EXIF / IPTC / XMP:** Injects camera make, model, lens, capture timestamp, title, description, and keywords directly into the export file.
* **Orientation Normalization:** Always exports with pixel dimensions upright and sets EXIF `Orientation = 1` to prevent orientation glitches on social media platforms.
* **Privacy Options:**
  * **Strip GPS Location:** Removes latitude, longitude, and altitude coordinates.
  * **Strip All Metadata:** Delivers clean, anonymous pixel-only files.

---

## 5. Collision-Safe Naming Templates

Exports support dynamic naming patterns:
* **Tokens:** `{name}`, `{date:%Y%m%d}`, `{camera_model}`, `{seq:04}`.
* **Collision Safety:** Photon never silently overwrites existing files. If a destination file exists, Photon automatically appends a non-destructive suffix (`_2`, `_3`).
