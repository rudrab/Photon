# Color Management & ICC

Photon includes a complete color management pipeline based on **LittleCMS2 (`lcms2`)** to guarantee color fidelity across wide-gamut displays, raw sensor renders, and exported deliverables.

---

## 1. Embedded ICC Profile Extraction

When importing and rendering images:
* Photon parses embedded color spaces directly from image metadata:
  * **JPEG:** APP2 `ICC_PROFILE` segments.
  * **TIFF:** Tag `34675` (`Exif.Image.InterColorProfile`).
  * **PNG:** `iCCP` chunks.
  * **HEIC / AVIF:** `colr` boxes.
* If no embedded profile is detected, Photon safely assumes the standard **sRGB** color space.

---

## 2. LittleCMS2 Color Transformation Pipeline

* **Thumbnail & Preview Normalization:** All wide-gamut sources (such as AdobeRGB or Display P3 camera JPEGs) are transformed through LittleCMS2 into calibrated sRGB texture representations, preventing the desaturated or washed-out appearance common in basic image viewers.
* **16-Bit Processing:** During high-precision export, Photon maintains a 16-bit linear color transform pipeline directly from the RAW decode stage.

---

## 3. Export Output Color Spaces

When exporting photos (**`Ctrl+E`**), you can select the target color profile:
* **sRGB (Standard):** Recommended for web publishing, social media, and consumer printing.
* **AdobeRGB (1998):** Wide-gamut color space ideal for commercial offset printing and publishing.
* **Display P3:** Wide-gamut color space designed for modern Apple displays, OLED screens, and HDR monitors.

Photon embeds the selected ICC profile directly into the exported JPEG, TIFF, or WebP file container.
