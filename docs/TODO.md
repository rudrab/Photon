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

### 🔴 R-5 · Collections and smart collections · M — builds on P-4

- Smart collection = a saved `LibraryQuery` (`models.rs`), e.g. "★≥4 AND tag:client-x AND not rejected AND date in 2026".
- Store the query as JSON, and evaluate it live in the sidebar with a count.

### 🔴 R-6 · Colour labels · S

- Red, yellow, green, blue, purple; keys 6–9 (as in Lightroom and Bridge).
- DB column `color_label`.
- XMP: `xmp:Label="Red"` (Lightroom/Bridge/digiKam) **and** `darktable:colorlabels` (a `rdf:Seq` of 0–4).
- Note that the merge code currently deletes `xmp:Label` only when it's "Pick" or "Reject" (legacy Photon values): keep that.
- Read labels back from XMP (moved here from R-1): `XmpReadResult` + `queries::update_from_xmp`.
- Add a filter in the header's filter popover.

### 🟡 R-7 · Picks in XMP · S — decide first

- No standard field exists. Options:
  - keep them DB-only (the current state)
  - use digiKam's `digiKam:PickLabel` (verify the exact name and values against digiKam source before using it)
- Document the decision in `sidecar.rs`.

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

### 🔴 R-13 · Robustness tests · M

- 🔴 Truncated, zero-byte, permission-denied and wrong-extension files in an import: the import continues and each error is reported.
- 🔴 A card pulled mid-import (simulate a reader that errors mid-stream): no partial file under a final name, the source untouched, and the hash released.
- 🔴 Kill during import: on the next start, delete stale `*.photon-part` files under the library root. **Not implemented yet**: add a startup sweep.
- 🔴 Unsupported RAW (rawler can't decode, e.g. some CR3 or compressed variants): the viewer falls back to the embedded preview with a visible note, instead of only a log line.

### 🔴 R-14 · Catalog backup hardening · S

- 🔴 Also copy the newest catalog backup into the import backup folder (a different disk).
- 🔴 "Restore from backup…" in Preferences: list backups with dates and photo counts; restore = copy over the DB after closing the pool, with a restart prompt.
- 🔴 A "Back up now" button.

### 🔴 R-15 · darktable round trip in the grid · M

- After "Open in darktable" (`detail.rs::open_in_editor`), watch the sidecar.
- When its history changes, re-render the Grid and Large thumbnails with `darktable-cli --width 2560` so the grid shows the edited look (the way Lightroom shows develop changes).
- Mark edited photos with a badge.
- Serialize renders (the `raw.rs` mutex exists).

### 🔴 R-16 · Culling power features · M

- 🔴 Compare mode (`detail.rs::render_compare`) uses the Large previews: use full resolution, with synchronized 1:1 pan and zoom.
- 🔴 Preload the next and previous full-res renders while zoomed (a background task into `FULL_RES`).
- 🟢 Continuous zoom in the 1-up viewer (`ui/photo_view.rs`): log slider, Fit / 1:1 buttons, Ctrl+wheel and pinch anchored at the pointer, +/−, drag to pan, double-click toggles Fit ↔ 1:1 at the clicked point. Full resolution loads only once the zoom needs more pixels than the Large preview.
- 🟡 Optional: sort or filter a burst by a sharpness score (variance of the Laplacian on the Large preview) to find the sharpest frame.

### 🔴 R-17 · Stacks · S–M

`group_hash` already groups RAW+JPG+edits. Add a UI to:

- collapse a group into one tile with a count badge
- expand it
- choose which version is the cover

### 🟡 R-18 · Nice to have for pros · L

- 🟡 Tethered shooting: a `gphoto2` capture into a watched folder (R-11).
- 🟡 Client proofing: a static HTML gallery export with a pick/comment form (or a contact-sheet PDF).
- 🟡 Map view and geotagging from GPX.
- 🟡 Faces (import from digiKam at least; `sources/digikam.rs` already reads the digiKam DB).

---

## PART 3 — Known bugs (small, fix when nearby)

- 🟢 WebP export writes JPEG data (fixed in R-4).
- 🔴 `timeline.rs` `cull_rating`/`cull_flag`: DB errors are ignored (`if … .is_ok()`); on failure the UI shows a rating that wasn't saved. Log it and show a toast.
- 🟢 `export.rs`: copying the source XMP next to the export ignores errors (now reported in the export report).
- 🔴 `detail.rs` `open_in_editor`: a spawn failure (editor not installed) is ignored; show a toast.
- 🔴 Move import: the XMP sidecar is moved even when its photo's source is kept because the backup failed (harmless, but inconsistent).
- 🔴 Compare mode shows Large previews, not real pixels (see R-16).
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
- 🔴 **Faces:** detection, then recognition and naming, written to XMP
  `mwg-rs:Regions` (the digiKam/Lightroom format). Import digiKam's existing face
  tags first (`sources/digikam.rs`), since that is cheap. ONNX models (e.g. YuNet + SFace).
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
- 🟡 **Image quality sorter:** auto-reject blurred/under-/over-exposed frames
  (the R-16 sharpness score is the first step).
- 🟡 **Auto-tagging:** object/scene classification to suggest tags.
- 🟡 **Versioning:** track derivatives of an original (R-17 stacks are the basis).
- 🟡 Deliberately out of scope: an image editor (darktable/GIMP handoff), a
  plugin system, a MySQL backend, web-service uploaders beyond the share portal.

## Suggested order

- **Personal:** P-0 → P-1 → P-2 → P-3 → P-4 → P-5 → rest.
- **Professional (after personal P-0…P-3):** R-1 → R-3 → R-4 (WebP bug first) → R-2 → R-6 → R-9 → R-10 → R-12/R-13 → R-5/R-8 → R-15/R-16 → rest.
- **digiKam parity (PART 4):** R-6/R-8/R-5 + search builder → metadata editor (with R-9) → folder view + R-11 → map/GPX → similarity → faces → R-12 at 200k.
