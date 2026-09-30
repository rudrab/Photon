# Keyboard Culling & Triage

Photon provides high-speed, keyboard-driven triage modeled on commercial standards (Photo Mechanic and Lightroom Classic).

---

## 1. Ergonomic Shortcuts

All culling actions operate in both the timeline grid and the 1-up photo viewer:

| Shortcut | Action | Effect |
| :--- | :--- | :--- |
| `1`–`5` | Star Rating | Sets rating from 1 to 5 stars (`xmp:Rating`) |
| `0` / `` ` `` | Clear Rating / Color | Sets rating to 0 and clears color label |
| `6` | Red Label | Sets Color Label to Red (`xmp:Label="Red"`, `darktable:colorlabels=0`) |
| `7` | Yellow Label | Sets Color Label to Yellow (`xmp:Label="Yellow"`, `darktable:colorlabels=1`) |
| `8` | Green Label | Sets Color Label to Green (`xmp:Label="Green"`, `darktable:colorlabels=2`) |
| `9` | Blue Label | Sets Color Label to Blue (`xmp:Label="Blue"`, `darktable:colorlabels=3`) |
| `P` | Pick Flag | Marks shot as flagged pick (`flagged = 1`) |
| `X` | Reject Flag | Marks shot as reject (`flagged = -1`, `xmp:Rating = -1`) |
| `U` | Unflag | Clears pick/reject status |
| `[` / `]` | Rotate | Rotates counter-clockwise / clockwise (updates EXIF orientation) |

---

## 2. Shift Auto-Advance

Holding **Shift** while pressing any rating or flag shortcut (`Shift+1` through `Shift+9`, `Shift+P`, `Shift+X`, `Shift+U`) applies the metadata change and **automatically advances to the next photo immediately**, cutting culling time in half.

---

## 3. The Continuous Sub-Pixel Canvas (`PhotoView`)

The 1-up viewer canvas (`photo_view.rs`) provides precise focus verification:
* **Instant 1:1 Pixel Lock (`Z` or Double Click):** Immediately toggles between Fit-to-Screen and 100% full-resolution sensor crop.
* **Anchor-Preserved Zoom:** Dragging the zoom slider or using `Ctrl+Scroll` continuously zooms into the exact image coordinates directly beneath the mouse cursor without jumping.
* **Persistent Zoom During Rating:** Changing star ratings or color labels while zoomed into a photo retains the current zoom scale and pan position.

---

## 4. Side-by-Side Dual Compare Mode

When choosing between similar candidate shots:
1. Select two photos in the timeline grid.
2. Press `C` (or the Compare button in the bottom selection bar).
3. Photon renders both frames side-by-side with synchronized zoom and metadata controls, allowing you to pick the winner and reject the duplicate in one view.

---

## 5. 50-Step Reversible Undo Stack

Photon maintains an in-memory transactional undo/redo stack (`UndoManager`):
* Press **`Ctrl+Z`** to undo the last rating, color label, pick flag, tag, rotation, or trash action.
* Press **`Ctrl+Shift+Z`** to redo.
* Restoring a trashed image automatically recovers the source file from the FreeDesktop Trash bin, re-inserts all database relationships, and writes the sidecar.
