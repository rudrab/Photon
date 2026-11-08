# Changelog

## 0.5.0 — 2026-11-08

Photo Quality now judges how a photo looks, not only whether it is sharp, and
lets you take back or act on its rejects.

### Changed
- **Linux releases are a Flatpak** (`photon.flatpak`, x86_64), alongside the
  source code. The RPM is gone: distribution packages kept breaking on library
  versions. The Flatpak uses the same library folders as a source build, runs
  your own darktable-cli, exiftool, thumbnailers and editors, and includes
  OpenVINO (CPU) for the AI analysis. Build it with `flatpak/build.sh`.

### Added
- **Aesthetic score** in Analyse Photo Quality (optional, ~612 MB download,
  MIT + Apache-2.0): CLIP ViT-L/14 with the LAION aesthetic predictor judges how
  good a photo looks as a whole, not just whether it is sharp. Weak-looking
  photos and clearly weaker burst frames are listed (not ticked), the best
  frame of a burst is the best-looking sharp one, and the info panel shows the
  score and its rank in the library. Runs locally, on the GPU when there is one.
- **Crop-aware weak suggestions:** before a photo is offered as weak-looking,
  about 20 square crops that keep the main face are scored; if one is clearly
  stronger, the photo isn't suggested, and the info panel says which crop
  (Photon never crops).
- **Restore rejected photos** from the Photo Quality results: the rejected
  photos of the selection or view are listed, ticked; untick one to restore it.
- **Move Rejects to Trash…** in the Photo Quality results: applies the choices,
  then trashes every rejected photo there (asks first, undoable, follows
  "Move to Trash Removes").

## 0.4.0 — 2026-11-01

### Removed
- The **WhatsApp Web** and **Google Photos** share entries. They only copied the
  photos and opened a website, which looked like a one-click send. Use Copy and
  paste, or Open With for an installed app.

### Added
- **Import by day**: the Review Import dialog lists every day on the card or
  in the folder (newest first) with a checkbox, a count and a size; only the
  ticked days are imported. Previews load when a day is opened, so no day is
  hidden behind the first 120 photos.
- **Check Quality before import**: scores the embedded previews on the card
  with the same rules as Photo Quality, outlines likely rejects and can leave
  them out of the import. No face detection at this stage.

## 0.3.0 — 2026-10-21

A release for taking Photon on a photo trip: safer imports, more culling and
keywording tools, and a round trip with darktable. Much of the GUI side is
not yet checked by hand; see `docs/TODO.md`.

### Added
- **Adjust Date and Time** (`Ctrl+Shift+T`): shift the capture time of the
  selection by an amount, or set one photo's correct time. Applies to the whole
  shot, writes `exif:DateTimeOriginal` to the sidecars, and can be undone.
- **Creator and copyright** (Preferences): written to the sidecar of new
  imports and embedded in exports, filling only what a photo doesn't have.
- **Hierarchical keywords**: a tag tree in the sidebar with rolled-up counts,
  two-way sync with darktable's `lr:hierarchicalSubject`, digiKam tag trees.
- **Batch keywording** (`Ctrl+T`): tag popover with autocomplete, applied to
  the whole shot (RAW + JPG), with undo.
- **darktable round trip**: edits are detected from sidecars, thumbnails are
  re-rendered, and edited photos get a badge. A GIMP `.xcf` beside a photo is
  shown and reopened by "Open in editor".
- **Zoom to faces** (`F` / `Shift+F`) in the viewer, Compare and Survey.
- **Continuous zoom** in the viewer, anchored at the pointer.
- **Photo library folder** is a setting (default: your Pictures folder).

### Import safety
- Logout and suspend are held off while an import runs; closing the window
  mid-import asks first.
- An import cut short is reported at the next start, and importing again no
  longer duplicates photos that were already copied.
- A Move import deletes the card's copy only after the photos are committed to
  the database and you confirm. If Photon closes before you answer, it asks
  again at the next start.
- Reads one stream at a time from USB drives and cards (about twice as fast).

### Changed
- Rotation applies to every file of a shot, and can be undone.
- Full-resolution renders are limited to two at a time; neighbours are
  preloaded while zoomed.
- Faster timeline, search and library check on large libraries; the timeline
  query no longer blocks the UI.
- New app icon.

### Fixed
- Library tags are kept when a sidecar has no keywords; darktable's own
  keywords are ignored.
- The catalog backup retries a busy database instead of reporting corruption.
- Thumbnails try every embedded JPEG in a RAW until one decodes.
- A darktable re-render no longer deletes its own thumbnails.

### Known gaps
No read-only mode for archives, no damaged-files report, no watching for
external file changes, no time-zone picker, memory use is high on very large
libraries. See `docs/TODO.md`.
