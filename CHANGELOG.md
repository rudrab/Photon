# Changelog

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
