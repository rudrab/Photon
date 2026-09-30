<div align="center">
  <h1>📷 Photon</h1>
  <p><strong>A high-performance, non-destructive photo manager and culling workstation for GNOME Linux.</strong></p>
  <p>Built with <strong>Rust</strong>, <strong>GTK4</strong>, and <strong>Libadwaita</strong> — designed as the modern ingest, triage, and asset management companion for <strong>Darktable</strong>.</p>
</div>

<div align="center">

[![License](https://img.shields.io/badge/License-GPLv3-blue.svg)](LICENSE)
[![Language](https://img.shields.io/badge/Rust-1.75%2B-orange.svg?logo=rust)](https://www.rust-lang.org/)
[![Toolkit](https://img.shields.io/badge/Toolkit-GTK4%20%2F%20Libadwaita-46a946.svg?logo=gnome)](https://gnome.pages.gitlab.gnome.org/libadwaita/)
[![Platform](https://img.shields.io/badge/Platform-Linux%20(Wayland%2FX11)-FCC624.svg?logo=linux&logoColor=black)](https://www.kernel.org/)
[![Database](https://img.shields.io/badge/Storage-SQLite%20(WAL)-003B57.svg?logo=sqlite)](https://www.sqlite.org/)

</div>

> [!WARNING]
> **Beta software.** Core features work, but Photon is young and may contain bugs or unfinished features. It never overwrites originals, but **keep backups of your photos and catalogue**, and try new features on a copy of your library first.

---

**Photon** is an open-source Digital Asset Manager (DAM) built from the ground up in Rust for working and enthusiast photographers. It combines the rapid keyboard ergonomics and ingest safety of **Photo Mechanic** with a modern **GNOME HIG / Libadwaita** interface and seamless **two-way XMP sidecar interoperability with Darktable**.

Unlike legacy managers that lock edits into proprietary catalogs or freeze under large RAW libraries, Photon keeps your filesystem clean, provides instant ThumbHash-powered timeline navigation, runs local neural network eye-focus analysis, and guarantees zero data loss during card ingestion.

---

## 🌟 Core Highlights

```
┌────────────────────────────────────────────────────────────────────────┐
│                          THE PHOTON WORKFLOW                           │
├──────────────────┬───────────────────────┬─────────────────────────────┤
│  1. Safe Ingest  │  2. Culling & Triage  │  3. Organize & Develop      │
│  ──────────────  │  ───────────────────  │  ─────────────────────      │
│  • Staged Part   │  • Keys 1-5, 6-9, P/X │  • 2-Way Darktable Sync     │
│    File Commits  │  • Sub-Pixel Pan/Zoom │  • Live Smart Collections   │
│  • BLAKE3 Dedup  │  • AI Eye-Focus Score │  • LittleCMS2 16-Bit Export │
│  • Mirror Safety │  • Side-by-Side Dual  │  • Tag & Missing Relinking  │
│    Card Backup   │    Compare Viewport   │  • Normalized Orientation   │
└──────────────────┴───────────────────────┴─────────────────────────────┘
```

### 🛡️ 1. Hardware-Safe Ingest & Deduplication
* **Staged Atomic Commits:** Imports write to temporary `.part` files in library structure (`YYYY/MM/DD`). Pulled cards or power outages leave zero corrupt records or partial files.
* **Automated Card-Wipe Mirror Backup:** Move imports automatically duplicate and verify the source to a secondary backup drive *before* purging the camera card.
* **Streaming Hash Deduplication:** Computes streaming BLAKE3 hashes with quick-stat heuristics, preventing duplicate files from being imported twice.
* **RAW + JPEG Shot Grouping:** Correlates raw files and JPEG companion files under a unified shot entity (`group_hash`), synchronizing culls across variants.

### ⚡ 2. Commercial Keyboard Culling & Canvas
* **Industry Standard Shortcuts:** Star ratings (`1`–`5`), color labels (`6`–`9`), pick/reject flags (`P`, `X`, `U`), and orientation rotation (`[`, `]`, `Ctrl+R`).
* **Continuous Sub-Pixel Canvas:** Smooth continuous pan and fractional zoom (10% to 800%) anchored precisely under the pointer with 1:1 sensor-pixel lock.
* **ThumbHash Memory Placeholders:** High-speed 60fps virtualized timeline scrolling using compact 28-byte ThumbHashes rendered directly from RAM with zero blank grey tiles.
* **50-Step Reversible Undo:** Full in-memory undo/redo stack (`Ctrl+Z` / `Ctrl+Shift+Z`) for culling, ratings, colors, tags, and FreeDesktop Trash restorations.

### 🤖 3. Embedded AI Quality & Eye-Focus Scoring
* **Local Face Detection (optional):** Runs the **YuNet** model (MIT) through **OpenVINO** on your CPU or Intel GPU to find faces and 5-point landmarks. The model is a ~230 KB download you approve; nothing leaves your machine.
* **Laplacian Eye Micro-Contrast:** Measures high-frequency Laplacian variance specifically on the eye crop to evaluate iris and eyelash focus.
* **Session-Calibrated Burst Triage:** Benchmarks portrait shots against the session's 80th percentile baseline and suggests missed-focus shots for rejection. Blur and clipped exposure are checked too, without needing the model. Photon only *suggests*; you confirm.

### 🔄 4. Non-Destructive Darktable Ecosystem Sync
* **Full Two-Way XMP Interoperability:** Reads and writes `xmp:Rating`, `xmp:Label`, `darktable:colorlabels` (0–4 sequences), and tags.
* **Edit History Protection:** Intelligently preserves all `<darktable:history>` blocks, parametric masks, and module settings untouched.
* **Loop Prevention:** Tracks `xmp_mtime` to eliminate infinite reload ping-pongs between Photon and Darktable.

### 🎨 5. Color Management & Production Export
* **LittleCMS2 Integration:** Accurate ICC color transformations for sRGB, AdobeRGB, and Display P3 wide-gamut displays and files.
* **16-Bit Processing:** Full 16-bit linear TIFF export pipeline preserving maximum sensor dynamic range.
* **Sharpening & Watermarking:** Configurable unsharp masking for screen/print, opacity-blended text/image watermarks, WebP/JPEG exports, and collision-safe naming.

---

## 📂 Supported Formats

| Format Category | Extensions / Formats | Pipeline |
| :--- | :--- | :--- |
| **RAW Formats** | `.orf`, `.nef`, `.cr2`, `.cr3`, `.arw`, `.dng`, `.rw2`, `.pef`, `.raf` | `rawler` (pure Rust) / Embedded Preview / `darktable-cli` (optional) |
| **Standard Images** | `.jpg`, `.jpeg`, `.png`, `.webp`, `.tif`, `.tiff`, `.gif` | Native Image & WebP Crate Decoders |
| **Next-Gen Formats** | `.heic`, `.heif`, `.avif` | `libheif` with Orientation Decoupling |
| **Video Clips** | `.mp4`, `.mov`, `.m4v`, `.mkv`, `.avi`, `.webm`, `.mts`, `.m2ts` | GStreamer Frame Extraction & GTK4 Video |
| **Sidecars** | `.xmp`, `.darktable.xmp` | Two-Way XML Parsing & Non-Destructive Merging |

---

## ⌨️ Essential Shortcuts

| Action | Shortcut | Scope |
| :--- | :--- | :--- |
| **Rate 1–5 Stars** | `1`, `2`, `3`, `4`, `5` | Grid & Viewer |
| **Clear Rating / Color** | `0` / `` ` `` | Grid & Viewer |
| **Color Labels (Red, Yellow, Green, Blue)** | `6`, `7`, `8`, `9` | Grid & Viewer |
| **Pick / Reject / Unflag** | `P` / `X` / `U` | Grid & Viewer |
| **Rotate Counter-Clockwise / Clockwise** | `[` / `]` or `Ctrl+R` | Grid & Viewer |
| **1:1 Full-Res Zoom Toggle** | `Z` / Double-Click | Viewer |
| **Step Next / Previous** | `Right` / `Left` (or `Shift + Cull Key`) | Viewer |
| **Toggle Info Panel / Sidecars** | `I` | Viewer |
| **Compare Mode** | `C` | Grid & Viewer |
| **Undo / Redo** | `Ctrl+Z` / `Ctrl+Shift+Z` | Application |
| **Global Search** | `Ctrl+F` | Grid |
| **Import Photos** | `Ctrl+O` | Application |
| **Export Selected** | `Ctrl+E` | Grid & Viewer |
| **Slideshow** | `F5` | Grid |

---

## 🚀 Building & Installation

### 1. System Dependencies

**Build:** a Rust toolchain (1.75+), a C compiler, `pkg-config`, GTK 4, libadwaita, LittleCMS 2, and libheif. RAW decoding is pure Rust, so LibRaw is *not* needed.

**Run (optional extras):**

| Extra | Enables | Without it |
| :--- | :--- | :--- |
| OpenVINO 2026.x + oneTBB | AI eye-focus scoring | AI shows as unavailable; blur and exposure checks still work |
| GStreamer plugins (`-good`, `-libav`) | Video playback | Videos do not play |
| `darktable-cli` | RAW export with your darktable edits | Built-in RAW renderer |
| `exiftool` | Fuller metadata in exports | Reduced built-in writer |

```bash
# Fedora: AI runtime (OpenVINO does not pull in oneTBB)
sudo dnf install openvino tbb
```

Then download the face model in **Preferences → AI**. See the [AI guide](https://mavensgroup.github.io/photon/guide/ai_quality.html) for GPU and NPU notes.

#### Fedora / RHEL:
```bash
sudo dnf install gcc gcc-c++ pkgconf-pkg-config gtk4-devel libadwaita-devel \
    sqlite-devel lcms2-devel libheif-devel gstreamer1-devel \
    gstreamer1-plugins-base-devel gstreamer1-plugins-bad-free-devel
```

#### Ubuntu / Debian (23.10+):
```bash
sudo apt install build-essential pkg-config libgtk-4-dev libadwaita-1-dev \
    libsqlite3-dev liblcms2-dev libheif-dev libgstreamer1.0-dev \
    libgstreamer-plugins-base1.0-dev libgstreamer-plugins-bad1.0-dev
```

#### Arch Linux:
```bash
sudo pacman -S base-devel gtk4 libadwaita sqlite lcms2 libheif gstreamer gst-plugins-base
```

### 2. Build & Run

```bash
# Clone repository
git clone https://github.com/mavensgroup/photon.git
cd photon

# Run test suite
cargo test --release

# Build and run
cargo run --release -p photon-app
```

---

## 📖 Documentation

Full documentation is available in the **[Photon Online Documentation](https://mavensgroup.github.io/photon/)**:
* **[Ingestion & Mirror Safety Guide](https://mavensgroup.github.io/photon/guide/import_safety.html)**
* **[Keyboard Culling & Triage](https://mavensgroup.github.io/photon/guide/culling_triage.html)**
* **[Darktable Two-Way Synchronization](https://mavensgroup.github.io/photon/guide/darktable_interop.html)**
* **[AI Quality & Facial Focus Scoring](https://mavensgroup.github.io/photon/guide/ai_quality.html)**
* **[Smart Collections & SQL Rules](https://mavensgroup.github.io/photon/guide/smart_collections.html)**
* **[Color Management & 16-Bit Pipeline](https://mavensgroup.github.io/photon/guide/color_management.html)**

---

## 📜 License

Photon is open-source software licensed under the **GNU General Public License v3.0 or later (GPL-3.0-or-later)**.
See [LICENSE](LICENSE) for details.

## Licensing, Support and Commercial Use

Photon is and will remain free software under the **GPL-3.0-or-later** licence. You can use, study, modify and share it freely under those terms. The project is sustained in these ways:

* **Paid convenience builds:** signed binaries, installers, and store or Flathub listings, offered for a fee or with a donation option. The source code always stays free, and you can build it yourself.
* **Sponsorship and donations:** GitHub Sponsors, Open Collective, or grants.
* **Dual licensing:** organisations that cannot accept the GPL can obtain a separate proprietary licence from the copyright holder.

To make dual licensing possible, outside contributions are accepted only under a contributor licence agreement (CLA) or copyright assignment. See [`CONTRIBUTING.md`](CONTRIBUTING.md). For commercial licensing enquiries, contact [The Mavens Group](https://mavens-group.github.io/).

## Citation and Attribution

Photon is free software under the GPL-3.0-or-later licence. If you use Photon, or parts of its code, in your work, please cite it. GitHub's **Cite this repository** button uses [`CITATION.cff`](CITATION.cff); in BibTeX:

```bibtex
@software{banerjee_photon,
  author  = {Banerjee, Rudra},
  title   = {Photon: a non-destructive photo culling and asset manager for GNOME},
  year    = {2026},
  version = {0.1.0},
  url     = {https://github.com/mavensgroup/photon},
  license = {GPL-3.0-or-later}
}
```

If you reuse code, the GPL also requires that you keep the copyright and licence notices, mark your changes, and release your derived work under the same licence.
