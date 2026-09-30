# Introduction to Photon

Photon is engineered to solve a long-standing challenge on the Linux desktop: providing a digital asset manager (DAM) that matches the speed, reliability, and ergonomics of commercial tools like Photo Mechanic and Lightroom Classic while remaining fully open-source and respecting local file ownership.

>[!WARNING]
>This is a beta release. Core features work, but Photon may contain bugs or unfinished features. It never overwrites originals, but keep backups of your photos and catalogue, and try new features on a copy of your library first.

---

## Why Photon?

Traditional Linux photo management forced a difficult trade-off:
1. **Shotwell:** Lightweight and GNOME-native, but single-threaded, lacks pro culling controls, fails to handle RAW+JPEG pairs properly, and does not support `.xmp` sidecar workflows.
2. **digiKam:** Extremely feature-dense, but heavy, complex, slower to startup, and prone to metadata namespace conflicts with Darktable.

Photon takes a focused, modular approach:
* **Ingestion and Triage Station:** Rapidly import, deduplicate, backup, cull, rate, and organize large volumes of raw and raster assets.
* **Darktable Companion:** Delegates raw pixel development to specialized tools like Darktable while maintaining bidirectional XMP metadata synchronization.
* **Modern GNOME Architecture:** Built in pure Rust on GTK4 and Libadwaita with smooth Wayland rendering, native responsive layouts, and zero bloat.

---

## Optional Local AI

Photon can score eye focus and suggest rejects using YuNet face detection through OpenVINO (CPU or Intel GPU). It is optional: the model is a small one-time download you approve, and nothing leaves your computer. See [AI Focus & Quality Analysis](guide/ai_quality.md) and the [dependency table](installation.md#1-dependencies).

---

## Crate Architecture

Photon is organized into four modular workspace crates:

```
crates/
├── photon-core/     # Core domain models, SQLite schema migrations (001-017), queries, FTS5
├── photon-import/   # Ingest engine, XMP sidecar engine, LittleCMS2 ICC, thumbnailing, export
├── photon-ai/       # OpenVINO runtime, model manager, YuNet face detection, eye sharpness
└── photon-app/      # GTK4/Libadwaita UI, PhotoView canvas, timeline, undo stack, detail viewer
```

### 1. `photon-core`
Manages data integrity and database access.
* **Database:** SQLite in WAL (Write-Ahead Logging) mode with strict foreign key constraints.
* **Migrations:** 17 incremental migrations managing tables for `images`, `tags`, `albums`, `events`, `smart_collections`, `image_quality`, and FTS5 search.
* **Domain Models:** Strongly typed Rust structs for shots, timeline items, color labels, ratings, and queries.

### 2. `photon-import`
Handles heavy file I/O, format decoding, and metadata transformations.
* **Atomic Ingestion:** Staged imports with `.part` file commit pipelines.
* **XMP Engine:** Bidirectional parser/writer preserving Darktable edit history.
* **Color Engine:** LittleCMS2 profile extraction and transform pipeline.
* **Thumbnail Pipeline:** Atomic thumbnail generation and compact 28-byte ThumbHash storage.

### 3. `photon-ai`
Local machine learning and quality analysis.
* **Inference runtime:** OpenVINO, loaded at run time, so Photon starts and works without it.
* **Model manager:** Verified download, storage, and deletion of models (YuNet face detection today).
* **Focus Assessment:** Isolates eye crops and computes high-frequency Laplacian variance.

### 4. `photon-app`
The desktop application frontend.
* **Libadwaita Controls:** HeaderBar, Popovers, Toasts, and responsive Navigation Sidebar.
* **PhotoView:** Custom continuous sub-pixel pan/zoom rendering canvas.
* **Undo Engine:** 50-step in-memory undo/redo manager with sidecar synchronization.

---

## Licensing, Support and Commercial Use

Photon is and will remain free software under the **GPL-3.0-or-later** licence. You can use, study, modify and share it freely under those terms. The project is sustained in these ways:

* **Paid convenience builds:** signed binaries, installers, and store or Flathub listings, offered for a fee or with a donation option. The source code always stays free, and you can build it yourself.
* **Sponsorship and donations:** GitHub Sponsors, Open Collective, or grants.
* **Dual licensing:** organisations that cannot accept the GPL can obtain a separate proprietary licence from the copyright holder.

To make dual licensing possible, outside contributions are accepted only under a contributor licence agreement (CLA) or copyright assignment. See `CONTRIBUTING.md` in the repository. For commercial licensing enquiries, contact [The Mavens Group](https://mavens-group.github.io/).

## Citation and Attribution

Photon is free software under the GPL-3.0-or-later licence. If you use Photon, or parts of its code, in your work, please cite it. GitHub's **Cite this repository** button uses `CITATION.cff` in the repository; in BibTeX:

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
