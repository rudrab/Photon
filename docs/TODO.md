# Photon — Roadmap & TODO

The work needed to make Photon (1) a full replacement for Shotwell for
personal use and (2) a manager a working photographer can trust. Written so
any agent or contributor can pick up a task without the conversation it came
from.

Last reviewed: 2026-09-29 (library of ~600 photos, Olympus ORF + JPEG, Fedora, GNOME).

---

## 0. Read this first

### Layout

| Crate | Role |
|---|---|
| `crates/photon-core` | SQLite schema + migrations (`db/schema.rs`), all queries (`db/queries.rs`), models (`models.rs`), DB pool and catalog backup (`db.rs`) |
| `crates/photon-import` | Import pipeline (`engine.rs`), file placement and verification (`library.rs`), metadata (`metadata.rs`), XMP sidecars (`sidecar.rs`), thumbnails and full-resolution decoding (`thumbnails.rs`), RAW rendering (`raw.rs`), export (`export.rs`), import sources (`sources/`: disk, shotwell, digikam) |
| `crates/photon-app` | GTK4 + libadwaita UI. `ui/window.rs` (main window), `ui/timeline.rs` (virtualized grid, culling keys), `ui/detail.rs` (1-up viewer, zoom, compare, info panel, metadata editing), `ui/preferences.rs`, `ui/export_dialog.rs`, `handlers/import_handler.rs`, `main.rs` (startup, catalog backup) |

### Commands

```sh
cargo build --offline                   # must be warning-free
cargo test --offline                    # all tests must pass
RUST_LOG=info cargo run --offline       # run the app
PHOTON_SCREENSHOT=/path/shot.png cargo run --offline   # renders the window after 4 s, then quits
```

Useful tools installed on the dev machine: `xmllint`, `exiv2`, `darktable-cli`,
`sqlite3`. Real sample RAW files (Olympus `.ORF` with darktable sidecars) are in
`~/Pictures/YYYY/MM/DD/`.

### Rules for agents

- **Never modify the user's real photos, sidecars or `~/.local/share/photon/photon.db`**
  while testing. Copy samples to a scratch/temp dir first. Tests use `tempfile`.
- Match the surrounding code: comments explain *why*, names are plain, no
  `unwrap()` on I/O in non-test code, errors are logged (`log::warn!`) or
  shown — never silently dropped with `let _ =` for anything that matters to
  the user's data.
- Every data-safety change needs a test that would have caught the bug.
- UI changes can't be fully verified headless: say so in the report, and list
  what the user should click through.
- The maintainer commits. Don't commit unless asked.
- **Open source only.** Every dependency, runtime and ML model must be under an
  OSI-approved licence (MIT, Apache-2.0, BSD, GPL/LGPL, MPL…). Model *weights*
  count: "research only", "non-commercial" or custom-restricted weights are out,
  even when the code is MIT. No proprietary GPU stacks (CUDA, TensorRT) by
  default (see AI-0). Hardware firmware from linux-firmware (GPU GuC/HuC, NPU)
  is accepted: it is needed to run the hardware at all.
- Schema changes go in a new numbered migration in `db/schema.rs` (never edit
  an applied one). New `Preferences` fields need `#[serde(default)]`.
- When you finish or start a task, update its status marker (see the legend below).

### Invariants — don't break these

1. **XMP sidecars are merged, never regenerated.** `sidecar::sync_xmp_metadata`
   takes an `XmpUpdate` whose `None` fields are left alone. The rest of the file
   stays byte for byte, including darktable history and Lightroom `crs:*`. A
   sidecar it can't parse is left untouched and an error is returned.
2. **Reject = `xmp:Rating="-1"`** (the Lightroom/Bridge/darktable convention).
   Picks are *not* written to XMP (no standard field); they live in the DB only.
3. **The DB is the source of truth for Photon's own state**; XMP is two-way synced
   with darktable/external tools (read on import, on view, at startup and when
   the window is focused again; the newer write wins). Every change Photon makes
   to a photo's rating, reject, tags, title, description or orientation is
   written to its sidecar through `sidecar::write_image_xmp`, which records
   `images.xmp_mtime` so the write isn't read back as an outside edit. A sidecar's
   `dc:subject` is taken as the photo's complete tag list, so this matters.
4. **Thumbnails are keyed by content hash** (`thumbnails::thumb_path`), not DB
   id. Anything that changes how a photo looks (rotation, edits) must
   invalidate or re-key its cache entries.
5. **Orientation:** `raw::develop` and embedded previews come out in sensor
   orientation, and callers apply EXIF orientation (`thumbnails::apply_orientation`,
   the only copy). `raw::render_with_darktable` output is already upright.
6. **Imports:** copies are fsynced, verified when `ImportConfig.verify` is set
   (read back after dropping the page cache), and a Move deletes the source only
   after the library copy, and the backup copy if configured, exist.
   `.photon-part` files are never exposed under the final name.
7. **Tags:** use `queries::ensure_tag`, never `create_tag` (which returns a wrong
   id when the tag already exists).

### Done (2026-09-29) — don't redo

- Phase 1 (harden): PART 3 bugs, R-13, R-14. See those tasks for what and where.
  P-0's hand checks are the user's.

- Continuous zoom in the 1-up viewer (`ui/photo_view.rs`, see R-16). The viewer's
  hidden info-panel revealer must keep `hexpand(false)`: its entries expand, and
  otherwise the hidden panel takes half the width from the photo.
- All the 2026-09-28 work below is committed (P-0's commit half).

### Done (2026-09-28) — don't redo

- XMP merge instead of overwrite; grid culling writes XMP; stale-rating bug
  fixed; `create_tag` → `ensure_tag` in the info panel.
- Real RAW decoding (rawler) for 1:1 zoom, brightness-matched to the camera
  preview; true full-resolution 1:1 for all formats, with a 3-entry texture
  cache (`detail.rs::load_full_resolution`).
- RAW export via `darktable-cli` + sidecar (private config copy, serialized,
  300 s timeout), falling back to rawler, then to the embedded preview.
- Export orientation 5/7 bug fixed (duplicate `apply_orientation` removed).
- Import: fsync on every copy, read-back verification, hash comparison on
  cross-device moves, optional backup folder (`ImportConfig.backup_dir`,
  Preferences → Import Safety).
- Daily catalog integrity check + `VACUUM INTO` backup, keeping 14 copies in
  `~/.local/share/photon/backups/`, with a dialog if the check fails.
- Manual rotation (P-1): 8×2 EXIF orientation composition table, SQLite persistence,
  cache invalidation, XMP `tiff:Orientation` sync, grid/viewer toolbar buttons & keys.
- Video support (P-2): QuickTime `mvhd` atom & exiftool extraction, `gst-video-thumbnailer`
  frame grabbing, timeline duration/play badges, `gtk4::Video` + `gtk4::MediaControls` viewer.
- HEIC / AVIF / GIF support (P-3): `libheif-rs` HEIC/AVIF decoding (with double-rotation
  guard), GIF first-frame thumbnails and animated playback in viewer.
- Albums (P-4): schema migration 008, sidebar "Albums" section with count badges & context menus, selection bar add/remove, timeline filtering, persistence.
- Undo/redo (P-5): in-memory stack (Ctrl+Z / Ctrl+Shift+Z) for rating, flags, rotate, tags, and FreeDesktop system trash restore with XMP resyncing.
- Slideshow (P-6): fullscreen window (F5), 3s/5s/10s timer, space pause/play, keyboard stepping, auto-hiding OSD bar.
- Named events (P-7): schema migration 009, event naming from sidebar and timeline headers, header event display.
- Two-way XMP sync (R-1): migration 010 (`xmp_mtime`); `sidecar::read_image_xmp` reads ratings,
  rejects (into `flagged`), orientation, title, description and the complete keyword list (tags
  removed elsewhere are removed in Photon) on import, on view, at startup and on window focus
  (throttled to 30 s); `sidecar::write_image_xmp` for every write, digiKam imports included.
  Migration 013 repairs rejects an earlier version stored as `rating = -1`.
- Colour management (R-2): `icc.rs` reads profiles from JPEG, TIFF, PNG, WebP and HEIC/AVIF
  (`colr` ICC, or nclx Display P3); thumbnails and the viewer are converted to sRGB. HEIF/AVIF are
  recorded upright at import (libheif applies `irot`/`imir` itself). Exports convert to sRGB,
  Adobe RGB (1998) or Display P3 (built-in lcms profiles; darktable renders straight into the
  space with `--icc-type`) and carry that profile.
- Embedded metadata in exports (R-3): `export.rs::embed_metadata`. With exiftool: the source's
  EXIF/IPTC/XMP from any format, Orientation=1, the new pixel size, optionally without GPS, plus the
  library's title, description, keywords (replacing the file's) and rating. Without exiftool: a
  JPEG export gets a JPEG source's EXIF (patched in place: orientation, size, GPS IFD blanked) and
  an XMP packet with the library's fields; other formats get a generated sidecar and a notice.
  "Include metadata" off = no metadata at all, only the colour profile.
- Export (R-4): a 16-bit pipeline for 16-bit TIFF (`raw::develop16`, darktable `bpp=16`,
  unoptimised 16-bit lcms transforms); presets in the DB (migration 014 seeds "Web 2048 sRGB",
  "Client full-res", "Print TIFF"; save/replace/delete in the dialog); text (cairo) or image
  watermarks with position, opacity and size; the dialog remembers the last export's settings
  (`library_meta.last_export_config`). WebP, sharpening, TIFF 8-bit, templates, no-overwrite
  suffixes and the error report as before.
- Missing and offline file detection & relink (R-10): schema migration 011 (`missing`), startup
  background check, offline mount detection vs missing file, tile warning badges, sidebar "Missing
  Photos" row, info panel status banner with inline "Locate File...", and recursive folder relink
  matching filename, size, and BLAKE3 hash.
- Adaptable bottom bar (Shotwell style): replaced floating overlay pill with a docked
  libadwaita/GTK4 `ActionBar` that adapts between idle (status summary + smooth thumbnail zoom
  slider + slideshow) and selection mode (selection count + deselect + culling/rating/rotation/album/export/trash
  actions) without covering grid photos.

**Not yet verified by hand in the GUI:** 1:1 zoom on ORF and large JPEG,
Preferences → Import Safety, a Move import with a backup folder. See P-0.
The rebuilt export dialog (presets save/delete, watermark rows, remembered
settings) and the re-read of sidecars when the window is focused again.

---

## Status legend

The marker sits at the start of each task heading and each checklist item.

| Marker | Meaning |
|---|---|
| 🔴 | Open — not started |
| 🟠 | Ongoing — in progress |
| 🟢 | Done |
| 🟡 | Needs a decision, or optional / low priority |

**Size:** S ≤ 1 day · M 2–5 days · L > 1 week

---

## PART 1 — Personal use (replace Shotwell)

### 🟠 P-0 · Verify and commit the 2026-09-28 work · S

Committed 2026-09-29. Still to verify by hand — run the app and check:

- 🔴 In the 1-up viewer, Z or double-click on an ORF: the spinner shows, the image gets sharper, brightness doesn't jump, and scrollbars cover the full sensor size.
- 🔴 The same on a large JPEG: real pixels, not a blurry upscale.
- 🔴 Step prev/next while zoomed: previously seen photos show instantly (from the cache).
- 🔴 Continuous zoom: drag the slider and Ctrl+scroll — the point under the pointer stays put, no jump when the full-resolution render replaces the preview; rating a photo while zoomed keeps the zoom and position.
- 🔴 Preferences → Import Safety: the switch persists across restarts; choose and clear the backup folder; the subtitle warns when it's on the library's disk.
- 🔴 Move import from a card with a backup folder: files appear in both places, and the card is emptied.
- 🔴 Rate a darktable-edited ORF in Photon, then open it in darktable: the edit history is intact and the rating shows.
- 🔴 Export an edited ORF with "Render RAW files with darktable" on: the export includes the edit.

### 🟢 P-1 · Manual rotate · S

- **Why:** There is no way to rotate a photo. Wrongly tagged photos stay sideways.
- **Where:**
  - `timeline.rs` (keys, selection)
  - `detail.rs` (toolbar button + keys)
  - `queries.rs` (new `set_orientation`/`batch_set_orientation`)
  - `images.orientation` (already exists, migration 005)
  - thumbnail cache
- **Approach:**
  - Keys `[` / `]` (rotate left/right) and `Ctrl+R`, in both grid and viewer; add toolbar buttons in the viewer and entries in `shortcuts.rs`.
  - Compose with the existing EXIF orientation (a table of the 8 EXIF values × rotate ±90). Store the result in `images.orientation`.
  - Invalidate the Grid and Large thumbnails for that hash (delete the files, or add the orientation to the cache key) and the `FULL_RES` cache entry in `detail.rs`. Regenerate asynchronously and refresh the tile.
  - Write `tiff:Orientation` into the XMP via a new `XmpUpdate` field (Lightroom and digiKam honour it). **darktable ignores it**, so a darktable export ignores Photon's rotation: document this in the UI tooltip, or pass the rotation to darktable-cli (investigate the `--style` / flip module).
  - Never rewrite the image file.
- **Acceptance:** Rotating 20 selected photos updates their tiles in under 1 s each, survives a restart, and exports upright through the built-in path.
- **Tests:** orientation composition table (all 8 × 2); an XMP merge test for `tiff:Orientation`.

### 🟢 P-2 · Video support · M

- **Why:** Camera and phone clips are invisible, and a *Move* import currently leaves them on the card, so formatting the card loses them.
- **Where:**
  - `models.rs` (`ImageFormat` → add `Video(kind)`, or a separate `MediaKind`; `all_extensions`)
  - `sources/disk.rs` (scan)
  - `engine.rs`
  - `thumbnails.rs`
  - `detail.rs`
  - `timeline.rs` (play badge + duration)
- **Approach:**
  - Extensions: mp4, mov, m4v, mts, m2ts, avi, mkv, 3gp, webm, plus Olympus `.MOV`.
  - Capture date: `exiftool -json` (already used as a fallback in `metadata.rs`), else the QuickTime `mvhd` creation time, else mtime.
  - Thumbnail: a GStreamer frame grab at about 1 s (`gstreamer` crate), or `ffmpegthumbnailer` if installed. Cache it like photos.
  - Viewer: a `gtk4::Video` widget (GTK's media support via gstreamer), plus "Open in external player".
  - Imports, dedup, verification and backup already work on bytes, so videos get them for free once scanned.
  - Culling, rating and tagging work the same. XMP sidecar: `clip.mp4.xmp`.
  - **Until this lands:** make a Move import warn about unsupported files left behind in the source folder (count them in `ImportProgress::Completed`).
- **Acceptance:** Importing a card with JPG+ORF+MOV brings in all three, and the MOV plays in the viewer.

### 🟢 P-3 · HEIC / AVIF / GIF decoding · M

- **Why:** `ImageFormat` accepts heic/heif/avif/gif and imports them, but the `image` crate has only `jpeg, png, tiff, webp` enabled (`photon-import/Cargo.toml`), so these get no thumbnail and can't be viewed or exported. iPhone photos are HEIC.
- **Approach:**
  - `libheif-rs` (system libheif) for HEIC/AVIF; the `image` crate's `gif` feature for GIF (first frame).
  - Route them through `thumbnails::load_smart`, `full_resolution` and export.
  - Read the HEIC orientation (the `irot`/`imir` boxes; libheif applies them by default, so don't double-rotate).
- **Tests:** a small fixture of each format; thumbnail + full-res + export round trip.

### 🟢 P-4 · Albums (manual collections) · M — also needed by R-5

- Schema migration 008: `albums(id, name, created_at, cover_image_id)` and `album_images(album_id, image_id, position)`.
- Sidebar section "Albums" with photo count badges, secondary-click rename/delete menus, and "+ New Album…" creation.
- Selection bar integration: "Add to album…" dropdown popup with quick creation, and "Remove from Album" when viewing an album.
- Survives restarts; timeline filtered via `TimelineFilter::Album(id)`.

### 🟢 P-5 · Undo for culling, tagging and trash · M

- In-memory undo/redo stack (`UndoManager`) with 50-action depth bound to `Ctrl+Z` / `Ctrl+Shift+Z`.
- Reversible actions: rating changes, pick/reject flags, orientation rotations, tag add/remove, and trash deletion.
- Trash restore integrates with FreeDesktop Trash (`$XDG_DATA_HOME/Trash/info/*.trashinfo`), restores source image files and sidecars, and re-inserts database records with tags.
- Automatically re-syncs XMP sidecars on undo and redo.

### 🟢 P-6 · Slideshow · S

- Standalone fullscreen dark window (`F5` or header bar menu), showing current selection or the active timeline view.
- 3s / 5s / 10s configurable auto-advance interval, spacebar pause/resume, left/right keyboard stepping.
- Auto-hiding OSD toolbar on mouse inactivity, using Large preview thumbnails with preloading.

### 🟢 P-7 · Named events · S

- Schema migration 009: `events(id, name, start, end)`.
- Event naming for days or ranges from both sidebar (secondary click) and timeline section headers (`.photon-day-edit-btn`).
- Displays event names in sidebar date rows (`"Event Name (Sun Aug 24)"`) and timeline section headers (`"Event Name — Sunday, August 24, 2025 · 42 photos"`).

### 🔴 P-8 · Shotwell metadata migration · M — for other users (this user has no Shotwell DB)

- **Why:** `ShotwellSource::apply_metadata` is a no-op (`sources.rs:28`) and `extract_tags` is never called. Importing from Shotwell brings over the files only.
- **Do:** implement `apply_metadata` like `sources/digikam.rs`:
  - ratings (Shotwell `rating` -1..5; -1 = rejected)
  - flags (`flags & 16` = flagged → pick)
  - hidden (`flags & 4`)
  - `title`
  - `comment` → description
  - tags (`TagTable.photo_id_list`, a comma list of `thumbXXXXXXXX` hex ids)
  - event names
  - `orientation`
- **Report what can't be migrated:**
  - `transformations` (Shotwell's non-destructive crops and adjustments)
  - faces
  - videos (until P-2)
- **Tests:** a fixture Shotwell DB built in the test with the real schema (see Shotwell 0.32 `PhotoTable`).

### 🟡 P-9 · Optional, low priority

- 🟡 Quick crop/straighten (non-destructive, stored in the DB and XMP `crs:Crop*`). Probably skip: GIMP and darktable handoff covers it.
- 🟡 Print: export to a temp JPEG and open the GTK print dialog.
- 🟡 "Set as wallpaper" via the portal.

---

## PART 2 — Professional use

Ordered by importance. R-1 to R-4 are the minimum before recommending Photon for paid work.

### 🟢 R-1 · Read XMP back (two-way sync with darktable) · M

- Done: see "Done" above. `xmp:Label` moved to R-6 (it needs the colour label column) and
  `lr:hierarchicalSubject` to R-8 (it needs tag hierarchy). A "Picked" keyword is an ordinary tag:
  picks aren't in XMP (invariant 2, R-7).
- Tell users: darktable writes sidecars on every change only if "write sidecar file for each
  image" is "on edit"; after rating in Photon, darktable needs "reload selected XMP files".

- **Why:** Photon writes XMP but never reads it back. Ratings, rejects and tags set in darktable (or Lightroom, or digiKam) never reach Photon, and a later Photon edit merges its own (stale) values on top.
- **Where:**
  - `metadata.rs` (parse)
  - `engine.rs` (on import)
  - a new migration adding `images.xmp_mtime INTEGER`
  - `sidecar.rs` (reuse `property`, `element`, `list_items`, `attribute_value`; they're private and can become `pub(crate)`)
- **Approach:**
  - Parse:
    - `xmp:Rating` (-1 → rejected, 0–5)
    - `dc:subject` (skip tags starting `darktable|`, which are darktable-internal)
    - `lr:hierarchicalSubject` (see R-8)
    - `dc:title`, `dc:description`
    - `xmp:Label` (see R-6)
    - `tiff:Orientation` (see P-1)
  - On import: seed the DB from the XMP (currently only the capture date is read).
  - Afterwards: store the sidecar mtime. When a photo is shown, and in a periodic background scan of recently used folders (or a gio `FileMonitor`, see R-11), re-read XMP files whose mtime changed, and apply them.
  - **Policy:** the newer write wins. Photon's own writes update `xmp_mtime` so they don't bounce back.
  - darktable writes XMP on every change only if its "write sidecar file for each image" setting is "on edit"/"always". Document this.
- **Acceptance:** Rate 3★ in darktable and the rating appears in Photon within seconds of viewing. A rating changed in Photon shows in darktable after "reload selected XMP".
- **Tests:** a parse fixture from a real darktable sidecar (see `sidecar.rs` tests), a round trip, and no ping-pong.

### 🟢 R-2 · Colour management · L

- Done: see "Done" above.
- 🟡 Optional: the display profile via colord for the viewer (the viewer assumes an sRGB display).

- **Why:** There's no ICC handling anywhere. AdobeRGB and other wide-gamut JPEGs and TIFFs show desaturated and wrong in thumbnails, the viewer and exports. Exports carry no profile.
- **Approach:**
  - `lcms2` crate (system lcms2 is on every Linux desktop).
  - Extract the embedded ICC (JPEG APP2 `ICC_PROFILE` chunks, TIFF tag 34675, PNG `iCCP`, HEIC `colr`).
  - Convert to sRGB when generating thumbnails and full-res textures. No profile → assume sRGB.
  - Optional later: the display profile via colord (`org.freedesktop.ColorManager` over D-Bus) for the viewer.
  - Export: convert to the chosen output space (sRGB default, AdobeRGB, Display P3) and embed the profile. Tag the `raw::develop` output as sRGB.
- **Acceptance:** An AdobeRGB test chart looks the same as in darktable or GIMP (with colour management on). Exports have the right ICC per `exiv2 -pS`.

### 🟢 R-3 · EXIF/IPTC/XMP in exported files · S–M

- Done: see "Done" above. Copyright comes with R-9 (it has no source in the library yet).

- **Why:** Exports have no camera data, capture date or copyright. Metadata only goes to a side `.xmp`. Clients, stock sites and social platforms need it embedded.
- **Where:** `export.rs` step 5.
- **Approach:**
  - Copy the metadata block from the source into the output (`img-parts` crate to inject the APP1 EXIF + XMP segments into the JPEG; or `exiftool -tagsFromFile SRC -all:all -Orientation=1 -overwrite_original` when installed).
  - **Set Orientation=1** (the pixels are already upright) and fix the pixel dimensions.
  - Options: "Remove GPS location", "Remove all metadata".
  - Include the title, description, keywords and copyright (R-9).
- **Acceptance:** `exiv2 -pa out.jpg` shows camera, lens, date, copyright; Orientation = 1.

### 🟢 R-4 · Export fixes and features · M

- 🟢 **Bug:** `ExportFormat::Webp` writes JPEG bytes into a `.webp` file (`export.rs`, the `Webp` arm). Fixed with a real WebP encoder (`webp` crate).
- 🟢 Export presets (saved in the DB): "Web 2048 sRGB", "Client full-res", "Print TIFF", and your own (save, replace, delete).
- 🟢 Output sharpening for screen and for print (unsharp mask after the resize).
- 🟢 Watermark (text or image; position, opacity, scale), set in the export dialog.
- 🟢 TIFF output, 8/16-bit (16-bit is a real 16-bit pipeline from the RAW or 16-bit source).
- 🟢 Filename templates: `{date:%Y%m%d}_{name}`, and non-overwriting collision suffixes (`_2`).
- 🟢 Show `ExportReport.errors` to the user via UI dialog, including "RAW fell back to embedded preview" warnings.
- 🟢 Don't overwrite an existing export silently: add a suffix (`_2`, `_3`).

### 🟢 R-5 · Collections and smart collections · M — builds on P-4

- Smart collection = a saved `LibraryQuery` / `SmartQuery` (`models.rs`), e.g. "★≥4 AND tag:client-x AND not rejected AND date in 2026".
- Store the query as JSON in `smart_collections` table (migration 017), and evaluate it live in the sidebar with a dynamic count badge.
- Full UI: Smart Collections section in sidebar with "+" creation dialog, context menu (Edit Rules, Rename, Delete), and timeline filtering via `TimelineFilter::SmartCollection(id)`.

### 🟢 R-6 · Colour labels · S

- Red, Yellow, Green, Blue, Purple; keys 6–9 = Red/Yellow/Green/Blue (as in Lightroom and
  Bridge; Purple and "no label" from the label menu). 0 clears the *rating* only. Shown as a
  dot in the theme's palette colour (grid tile, 1-up, menus, filter).
- Sidecar writes carry only the fields that changed (rating/reject vs label), and a label
  read from one file's sidecar is spread to the shot without touching its rating (and the
  reverse): a rating change never removes a label set in darktable. Files of a shot with
  different labels are left alone; a label-only mismatch is repaired.
- DB column `color_label INTEGER NOT NULL DEFAULT 0` (migration 017).
- XMP two-way sync: writes `xmp:Label="Red"` (Lightroom/Bridge/digiKam) **and** `darktable:colorlabels` (`rdf:Seq` of 0–4), removes when None.
- Preserved legacy Pick/Reject deletion in XMP.
- Read labels back from XMP (`XmpReadResult` + `queries::update_from_xmp`).
- Colour label filter in header filter popover, timeline grid, viewer toolbar, selection bar, and undo/redo manager (`UndoAction::ColorLabel`).

### 🟢 R-7 · Picks in XMP · S — decide first

- Decided to keep picks database-only (`flagged = 1`), as there is no universal industry XMP standard (digiKam's `digiKam:PickLabel` is non-standard and rejected by darktable/Lightroom).
- Documented in `crates/photon-import/src/sidecar.rs`.

### 🔴 R-8 · Hierarchical keywords · M

- `tags.parent_id` (a migration).
- UI tree in the sidebar.
- XMP `lr:hierarchicalSubject` with `|` separators (darktable uses the same format), plus the leaf and ancestors in `dc:subject`.
- Import from the digiKam and XMP sources: read `lr:hierarchicalSubject` back (moved here from R-1).

### 🔴 R-9 · Copyright / IPTC template at import · S

- Preferences: creator, copyright notice, contact email/URL, usage terms.
- Applied on import to the DB + XMP (`dc:creator`, `dc:rights`, `xmpRights:UsageTerms`, `Iptc4xmpCore:CreatorContactInfo`) and to exports: add them to `export.rs::Embed` (exiftool args and the no-exiftool XMP packet).

### 🟢 R-10 · Missing and offline files, relink · M

- **Why:** Pros keep their archives on external drives. Nothing currently handles a path that no longer exists.
- **Detect:** a background check at startup (stat each path in batches; mark `missing`) and on view. Distinguish *offline* (the path's mount point, e.g. under `/run/media`, isn't mounted) from *missing*.
- **UI:**
  - a badge on tiles (`⚠ Offline` / `⚠ Missing`)
  - a "Missing photos" sidebar entry
  - "Locate folder…" to relink by matching filename + size (then verify the hash) under a chosen folder
  - Viewer info panel status card with "Locate File…" dialog
- Never delete DB rows automatically.

### 🔴 R-11 · Watch for external changes · M

A gio `FileMonitor` (or the `notify` crate) on the library root and folders imported in place. Rate-limit.

- New files → offer to import.
- Changed XMP → R-1.
- Renamed or moved → relink.
- Deleted → mark missing.

### 🔴 R-12 · Scale testing and performance targets · M

Build a synthetic library generator (a test or bench) with 200k image rows plus a few thousand real files. Measure and keep in `benches/`:

- Cold start to first paint < 2 s; timeline scroll at 60 fps; filter/search < 200 ms.
- Import of 2,000 JPEG+RAW from a USB 3 card: throughput and CPU/IO.
- Disk usage of the thumbnail cache (add a cap and eviction for `Large`); memory of `FULL_RES` (3 × ~60 MB at 20 MP; make it size-aware for 60 MP bodies).
- `quick_check` time on a large DB: if too slow for startup, move to idle time.

### 🟢 R-13 · Robustness tests · M

- 🟢 Truncated, zero-byte, permission-denied and wrong-extension files in an import: the import continues and each error is reported (`engine.rs` test `damaged_and_unreadable_files_…`). Empty files are refused; files that import but can't be previewed are reported.
- 🟢 A card pulled mid-import: no partial file under a final name (`library.rs` test `card_pulled_mid_copy_…`, a reader that fails mid-stream). The source is only read; the hash is claimed only after a successful copy (Copy) or released on error (Move).
- 🟢 Kill during import: a startup sweep (`library::sweep_partial_files`, called from `main.rs`) deletes `*.photon-part` files older than the app's start in the default library, the import backup folder and every folder holding library photos. A custom import destination's *new* day folder (no photo committed yet) isn't covered.
- 🟢 Unsupported RAW: the viewer shows "Full resolution unavailable — showing the camera's embedded preview" over the photo (reason in the tooltip).

### 🟢 R-14 · Catalog backup hardening · S

- 🟢 The newest catalog backup is mirrored to `<import backup folder>/Photon Catalog Backups/` (`db::mirror_newest_backup`, rotated to 14); an unplugged backup disk is skipped quietly and caught up next time.
- 🟢 Preferences → Library Database → "Restore from Backup": lists local and mirrored backups with date, photo count and size. Restore is **staged** (`db::stage_restore` checks the copy with `quick_check`), then applied at the next start before the DB opens (`db::apply_staged_restore`); the replaced DB and its `-wal`/`-shm` are kept in `backups/before-restore-…/`. The app offers "Quit Photon" to finish.
- 🟢 "Back Up Now" in the same group, with the last backup's time.

### 🔴 R-15 · darktable round trip in the grid · M

- After "Open in darktable" (`detail.rs::open_in_editor`), watch the sidecar.
- When its history changes, re-render the Grid and Large thumbnails with `darktable-cli --width 2560` so the grid shows the edited look (the way Lightroom shows develop changes).
- Mark edited photos with a badge.
- Serialize renders (the `raw.rs` mutex exists).

### 🔴 R-16 · Culling power features · M

- 🟢 Compare (`ui/compare.rs`, 2 selected): full-resolution panes with zoom and pan in step;
  select vs candidate (← → step the candidate through the timeline, ↑ / "Make Select" keeps
  it); click or Tab picks the active pane that marks and keys apply to.
- 🟢 Survey (`ui/survey.rs`, 3–9 selected): a grid with marks and quality rings per photo;
  ✕ / Backspace drops a photo from the survey, Enter opens it in 1-up. The bar's Compare
  button opens Compare for 2, Survey for 3–9.
- 🔴 Preload the next and previous full-res renders while zoomed (a background task into `FULL_RES`).
- 🟢 Continuous zoom in the 1-up viewer (`ui/photo_view.rs`): log slider, Fit / 1:1 buttons, Ctrl+wheel and pinch anchored at the pointer, +/−, drag to pan, double-click toggles Fit ↔ 1:1 at the clicked point. Full resolution loads only once the zoom needs more pixels than the Large preview.
- 🟢 Sharpness score & burst rejects: variance of the Laplacian (4×4 tile max + global), exposure clipping, burst detection (≤ 2 s), "Photo Quality" results dialog with grouped bursts, XMP Rating="-1" sync, viewer info panel sharpness, timeline "Possibly blurred" filter, and parallel rayon quality backfill (see AI-7).

### 🟠 R-17 · Stacks · S–M

`group_hash` groups RAW+JPG+edits.

- 🟢 One tile per shot (`queries::collapse_versions`): the JPG is the cover; a "RAW+JPG"
  (or "N versions") badge. Sidebar/date counts count shots. The 1-up viewer steps
  shot by shot; a button (and **V**) switches to the shot's other files.
- 🟢 Ratings, picks and rejects apply to every file of the shot (grid and viewer),
  with undo restoring each file's own value; each file's XMP is written.
- 🔴 Rotation still applies to the file on screen only; make it shot-wide.
- 🔴 Choose which version is the cover; expand a stack in the grid.

### 🟡 R-18 · Nice to have for pros · L

- 🟡 Tethered shooting: a `gphoto2` capture into a watched folder (R-11).
- 🟡 Client proofing: a static HTML gallery export with a pick/comment form (or a contact-sheet PDF).
- 🟡 Map view and geotagging from GPX.
- Faces: moved to AI-3 / AI-4 (PART 5).

---

## PART 3 — Known bugs (small, fix when nearby)

- 🟢 WebP export writes JPEG data (fixed in R-4).
- 🟢 `timeline.rs` culling: a failed save now reverts the tiles, logs, and shows a toast; the undo entry is added only once saved.
- 🟢 `export.rs`: copying the source XMP next to the export ignores errors (now reported in the export report).
- 🟢 `detail.rs` `open_in_editor`: a spawn failure (editor not installed) shows a toast.
- 🟢 Move import: when a photo stays on the card (backup failed), its XMP sidecar is now copied, not moved.
- 🟢 Compare mode shows real pixels now (R-16).
- 🟢 HEIC/AVIF/GIF are accepted but can't be decoded (fixed by P-3).

---

## PART 4 — digiKam parity

digiKam is the bar for "serious open-source DAM". Photon is past Shotwell for
culling, RAW, XMP round trip and export; these are the gaps, biggest first.
Several already have tasks above.

- 🔴 **Search and collections:** smart collections (R-5), hierarchical keywords (R-8),
  colour labels (R-6), a search builder over every field (camera, lens, focal
  length, ISO, date range, rating, label, tag, path) with saved searches.
- 🔴 **Map and geolocation:** a map view of GPS photos (libshumate), geotag from
  GPX tracks, reverse geocoding to place names as searchable tags (R-18).
- 🔴 **Faces:** see AI-3 (detection, digiKam import) and AI-4 (recognition).
- 🔴 **Similarity and duplicates:** a perceptual hash per photo for near-duplicate
  finding and "find similar" (digiKam's fuzzy search). Exact duplicates by BLAKE3 exist.
- 🔴 **Metadata editor:** edit EXIF/IPTC/XMP fields (capture date shift for a
  wrong camera clock, GPS, creator/copyright — R-9) on a selection.
- 🔴 **Batch tools:** rename by template, date shift, batch convert/resize (the export
  pipeline covers much of this), and a queue for them.
- 🔴 **Folder view:** browse the library by folder on disk, not only by date/event,
  with multiple roots including removable and network drives (R-10, R-11).
- 🔴 **Scale:** digiKam users have 100k–500k photos. R-12 targets; a background
  maintenance task (rebuild thumbnails, re-read metadata, find missing, DB vacuum).
- 🟡 **Light table:** compare more than two photos (R-16's compare is two).
- 🟡 **Image quality sorter:** see AI-7 (the R-16 sharpness score is the classical first step).
- 🟡 **Auto-tagging and "search by description":** see AI-6.
- 🟡 **Versioning:** track derivatives of an original (R-17 stacks are the basis).
- 🟡 Deliberately out of scope: an image editor (darktable/GIMP handoff), a
  plugin system, a MySQL backend, web-service uploaders beyond the share portal.

## PART 5 — AI features (AI-*)

**Implementing AI-0 or AI-7? Read `docs/AI-0_AI-7_BRIEF.md` first** (detailed
instructions, preflight checks, acceptance criteria).

All AI runs **locally**; no photo, face or embedding ever leaves the machine.
Open-source models only (see "Rules for agents"). AI results never overwrite an
original or rewrite a sidecar: they are either **metadata** (faces, tags, scores)
merged into the DB/XMP like any other change, or **derivative files** (denoised,
enhanced) stored next to the original and grouped with it (R-17 stacks).

Dependency chain:

```
AI-0 runtime ──┬── AI-1 denoise ─────┐
               ├── AI-3 face detect ── AI-4 face recognition
               ├── AI-5 segmentation ── AI-8 darktable mask handoff (spike)
               ├── AI-6 tags + semantic search
               └── AI-7 quality sorter
AI-2 auto-enhance (classical first, no AI-0 needed) ─┘   AI-1/AI-2 need R-17 (versions)
```

### 🟢 AI-0 · Inference runtime and model manager · M

Done 2026-09-29 (implemented by another agent, reviewed and corrected). `crates/photon-ai`.

- **Backend:** OpenVINO 2026.0 through the `openvino` crate 0.11 with runtime linking:
  Photon runs normally without OpenVINO, and AI shows as unavailable with the reason.
  Needs Fedora's `tbb` (`libtbb.so.12`); the `openvino` package doesn't pull it in.
- **Devices** come from OpenVINO itself (`available_devices`, `FULL_DEVICE_NAME`);
  `devices.rs` only explains missing ones (device node, permission, missing plugin or
  driver, or which library OpenVINO couldn't load). Checked off the UI thread in Preferences.
- **NPU:** the driver (`intel-npu-driver` 1.32, firmware) is installed, but Fedora's
  OpenVINO has **no NPU plugin** (`libopenvino_intel_npu_plugin.so`), so the NPU is
  unavailable. Revisit when Fedora packages it.
- **Models:** `models.toml` (embedded). YuNet 2023mar: hash verified against the file,
  licence **MIT** (the model folder's own LICENSE; the opencv_zoo repo's Apache-2.0
  doesn't apply to it). Downloads only after the user confirms; SHA-256 checked before
  rename and before every load. Preferences → AI: devices, models, download/delete.
- **Tiling** helper with an identity-model round-trip test.
- **Job queue: not built yet** — no AI job exists until AI-3. The first version was
  unused code and was removed with its GPU/NPU switches; build the queue with AI-3.

**Benchmark** (2026-09-29, Core Ultra 5 125H, Fedora 45, kernel 7.2.8, OpenVINO 2026.0,
`cargo run -p photon-ai --release --example bench -- --model <id|path> --image <jpg>`,
3 warm-up + 10 timed runs):

| Model (input) | Device | First compile | Cached load | Median | Max |
|---|---|---|---|---|---|
| YuNet (1×3×640×640) | CPU | 112 ms | 51 ms | 4.6 ms | 5.1 ms |
| YuNet (1×3×640×640) | Arc GPU | 792 ms | 58 ms | 2.8 ms | 3.0 ms |
| YuNet | NPU | — | — | — | no OpenVINO NPU plugin |

The GPU run added ~460 MB to the process. For YuNet the CPU is the default (decoding a
preview costs more than either inference); NPU first once available. No denoise model
was benchmarked: none with verified OSI-licensed ONNX weights was chosen yet (AI-1).

**Not verified by hand in the GUI:** Preferences → AI device rows, model download/delete.

### 🔴 AI-1 · Neural denoise (derivative file) · M–L — needs AI-0, R-17

- **Why:** the biggest AI win for Micro Four Thirds / high-ISO RAW; classical
  denoise (including darktable's profiled denoise) is clearly behind learned models.
- **Where in the pipeline:** on linear, demosaiced data from `raw::develop`,
  before the tone curve; for JPEGs, on the decoded image (weaker results).
- **Output:** a new 16-bit TIFF (later linear DNG, so darktable can still
  develop it) next to the original, e.g. `P6030189-denoised.tif`, imported and
  grouped with it; the XMP metadata (rating, tags) copied to it.
- **Candidate models (verify licence and weights before use):** NAFNet (MIT),
  SCUNet (Apache-2.0). Prefer a model trained on real camera noise (SIDD).
- **UI:** "Denoise…" on a selection, with strength, and a before/after in compare mode.

### 🔴 AI-2 · Auto-enhance (derivative file) · S–M — needs R-17

- Google Photos–style one-click enhance. **Start classical, no model needed:**
  auto levels / white balance, an S-curve, local contrast (CLAHE), vibrance.
  Instant, and most of the visible effect.
- Later, optionally a small learned tone-curve model (e.g. Zero-DCE, check
  its licence) behind AI-0.
- Output as a derivative like AI-1. **No render-time (non-destructive) enhance:**
  that needs a develop pipeline, and darktable is that pipeline.

### 🟠 AI-3 · Face detection · M — needs AI-0 (first user of it)

- 🟢 Detector: `photon-ai::faces` (YuNet decoding as OpenCV's `FaceDetectorYN`, letterbox,
  NMS, `MIN_SCORE` 0.6), used by AI-7 for eye sharpness; found the face in all 16 files of
  a test session. Faces aren't stored as regions yet (only a count per photo).

- **First, no AI:** import existing face regions from digiKam
  (`sources/digikam.rs`, its `ImageTagProperties` "tagRegion") and from XMP
  `mwg-rs:Regions` (written by digiKam, Lightroom, Picasa).
- Schema: `faces` (image id, rectangle in upright normalized coords, person id,
  source = detected/imported/manual, confidence). A new migration.
- Detection model: **YuNet** (OpenCV Zoo, MIT). *Not* InsightFace/SCRFD weights
  (non-commercial licence).
- Run over the Large preview (2560 px is enough), in the AI-0 queue, on import
  and as a library-wide scan.
- XMP: write `mwg-rs:Regions` through `sidecar::write_image_xmp` (merge, as ever).
- UI: face boxes toggle in the viewer; draw/delete a box by hand.

### 🔴 AI-4 · Face recognition and People · M–L — needs AI-3

- Embedding model: **SFace** (OpenCV Zoo, Apache-2.0). Store embeddings in the DB.
- Cluster unnamed faces (e.g. DBSCAN on cosine distance); "Who is this?" to
  name a cluster; suggest matches for new faces, confirmed by the user.
- A "People" sidebar section; a person is also a tag (hierarchical under
  `People|`, with R-8).
- Privacy: an option to exclude a person, and "delete all face data".

### 🔴 AI-5 · Segmentation (subject / sky / click-to-select) · M — needs AI-0

- Models: **SAM / SAM 2 / MobileSAM** (all Apache-2.0); MobileSAM for
  interactive speed on CPU. Click a point → mask; or "subject", "sky" with a
  text-free heuristic (largest salient object) first.
- Masks are kept as PNG files in the cache keyed by hash (like thumbnails), not in XMP.
- Uses on their own: export with transparent background, handoff to GIMP as a
  layer mask (easy: a TIFF with alpha, or the mask as a second file).

### 🟡 AI-6 · Auto-tagging and semantic search · M — needs AI-0

- CLIP-style image/text embeddings (OpenAI CLIP code and weights are MIT; for
  OpenCLIP check each checkpoint's licence) for "search by description"
  ("beach at sunset", "kids at a table") and suggested tags.
- Suggested tags are shown for confirmation, never applied silently (tags go to XMP).
- Store embeddings in the DB; a vector search over 100k photos must stay < 200 ms (R-12).
- Also gives AI-quality "find similar", on top of PART 4's perceptual hash.

### 🟢 AI-7 · Image quality sorter (classical) · S–M
### 🟡 AI-7 · Learned score · S–M (deferred until OSI-licensed technical quality weights are verified)

Done 2026-09-29 (implemented by another agent, reviewed and corrected). Menu: "Analyse
Photo Quality…" (scores what isn't scored in the selection/view, then shows the
blurred and badly exposed photos to reject, with Analyse Again); filter "Possibly blurred"; sharpness in the info panel.

- `photon-import::quality`: from the **Large preview only** (made if missing), scaled to
  1024 px: sharpness = variance of the 3×3 Laplacian, max over a 4×4 tile grid (plus
  global); shadows (luma ≤ 2) and highlights (luma ≥ 253 — a saturated colour isn't
  clipping); mean luma. `QUALITY_VERSION` 2 (1 mixed thumbnail/original sources and
  counted saturated colours as clipped; bumping it re-scores everything).
- Migration 015 `image_quality`. Analysis runs in the background with progress, Stop,
  and counts of skipped/unsaved photos, on **the selection, else the current view**
  (day, event, album, tag, search); the year/month overviews mean the whole library.
  One command: offers the face model if missing, scores, then always shows the results.
- Judged per **shot** (the files sharing `group_hash`, e.g. RAW + JPG, count once and
  are suggested together — a JPG+ORF pair used to look like a two-frame burst, which hid
  blurred shots). Bursts: ≥ 2 shots, same camera, ≤ 2 s apart; suggest < 60 % of the
  burst's sharpest. Outside bursts: < 8 % of the **library's** median sharpness
  (`BLUR_RATIO_OF_LIBRARY_MEDIAN`; tuned on this library: median ≈ 1350, motion-blurred
  shots 22–35), also used by the "Possibly blurred" filter. Clipping > 25 % highlights /
  > 35 % shadows. Never picks, never already-rejected shots, never a burst's best.
- **Faces** (when YuNet is downloaded): the eyes of the largest face are measured
  (`faces::eye_sharpness`: Laplacian variance around both eyes, scaled to 160 px wide).
  A shot is "eyes blurred" below 50 % of its **session's** 80th-percentile eye
  sharpness (same camera, ±30 min, ≥ 3 face shots), else of the library's; in a burst
  of faces, below 60 % of the sharpest eyes. This catches a blurred subject in front of
  a sharp, detailed background, which the whole-frame score can't (P1010651: frame
  2390, eyes 62 vs 128–151 for the sharp shots). Migration 016 (`faces`,
  `eye_sharpness`); photos scored before the model was downloaded get their faces
  checked on the next analysis.
- Confirmed rejects go through the same DB write + XMP sync as manual culling, off the
  UI thread; undoable; sidecars that couldn't be written are reported.
- Not done: scoring during import (not measured whether it would slow imports).
  Motion blur that leaves sharp specular highlights can score higher than it looks;
  a learned score (below) would catch those.

**Not verified by hand in the GUI:** the analysis run and its results on a real burst,
undo restoring the sidecars. Check on a copy of the library.

### 🟡 AI-8 · AI masks handed over to darktable · spike, then M–L — needs AI-5; decide first

- **Feasibility (to verify in a spike):** darktable can't import a bitmap mask
  from outside (check the installed version first; if upstream gains native AI
  masks, use that instead and drop this task). The route that works with
  today's darktable:
  1. Vectorise the AI-5 mask into polygons (contour tracing + simplification),
     then into darktable **path** shapes (points with bezier control points and a feather border).
  2. Write them to the XMP `darktable:masks_history` (`mask_id`, `mask_type`,
     `mask_name`, `mask_version`, `mask_points` hex blob, `mask_nb`, …) as a
     mask group the user can pick in any module's drawn-mask blending.
  3. Coordinates are normalized to darktable's input image space; check how
     this interacts with lens correction/crop (distortion back-transform).
- **Risks:** the binary layouts (`dt_masks_point_path_t`, blend params) are
  darktable-internal and versioned; get them from darktable's source
  (`src/develop/masks.h`, `src/develop/blend.h`) for the supported version,
  refuse unknown versions, and test with `darktable-cli`.
- **Safety:** never edit the user's existing darktable history; write to a
  **new duplicate** sidecar (`IMG.ORF_01.xmp`, darktable's duplicate naming) so
  the original edit is untouched. This is the only place Photon writes
  darktable-specific XMP: document it in the invariants when it lands.
- Decide after the spike: go, or GIMP-only handoff (AI-5).

## Suggested order

- **Personal:** P-0 → P-1 → P-2 → P-3 → P-4 → P-5 → rest.
- **Professional, digiKam parity and AI — the plan from 2026-09-29.** Phases in
  order; within a phase, left to right. Grouped so each area of code
  (`sidecar.rs`, the schema, the viewer) is opened once, and so every task's
  prerequisites land before it.

  1. **Harden (≈1–1.5 wk):** P-0 hand checks → PART 3 bugs → R-14 → R-13
     (incl. the stale `.photon-part` sweep). *Why first:* everything after this
     writes more metadata; make failures loud and backups restorable before that.
  2. **Metadata model (≈2–3 wk):** R-6 colour labels → R-7 decide picks → R-8
     hierarchical keywords → R-9 copyright template → metadata editor (PART 4:
     date shift, GPS, creator). *Why together:* all of them are schema +
     `sidecar.rs` + XMP read-back work. R-8 is also needed by AI-4 (`People|…`)
     and AI-6 (suggested tags).
  3. **Find (≈1.5 wk):** R-5 smart collections + the search builder (PART 4).
     *Why here:* labels and keywords now exist to search on.
  4. **Cull and versions (≈2 wk):** R-16 rest (full-res compare with synced
     zoom, preload) + AI-7's classical sharpness score → R-17 stacks → R-15
     darktable round trip. *Why:* R-17 is the home for AI-1/AI-2's derivative
     files; the viewer code is fresh from the zoom work.
  5. **AI foundation + faces (≈3 wk):** AI-3 part 1 (import digiKam/XMP face
     regions, no model) → AI-0 with the benchmark → AI-3 detection → AI-4
     recognition and People.
  6. **Library at scale (≈2 wk):** R-12 (200k rows) → R-11 watch for changes →
     folder view (PART 4). *Why before AI-6:* vector search and library-wide
     background jobs need the performance baseline; R-11 feeds new files to the AI queue.
  7. **Image AI (≈3–4 wk):** AI-2 classical enhance → AI-1 denoise (GPU) →
     AI-5 segmentation → AI-8 spike and go/no-go.
  8. **Search AI and extras:** AI-6 semantic search and tag suggestions → AI-7
     learned score → map/GPX (PART 4) → R-18 and the other 🟡 items.

  Rough total: 4–5 months of focused work. Phases 1–3 alone make Photon a
  credible digiKam alternative for people who don't need faces or maps.
