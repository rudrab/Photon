# Keyboard Shortcuts Cheat Sheet

Photon is designed to be operated almost entirely from the keyboard during import, culling, and navigation.

---

## 1. Culling & Metadata

| Key | Action | Notes |
| :--- | :--- | :--- |
| `1`–`5` | Star Rating | Rates 1 to 5 stars (`xmp:Rating`) |
| `0` / `` ` `` | Clear Rating / Color | Sets rating to 0 and clears color label |
| `6` | Red Color Label | `xmp:Label="Red"`, `darktable:colorlabels=0` |
| `7` | Yellow Color Label | `xmp:Label="Yellow"`, `darktable:colorlabels=1` |
| `8` | Green Color Label | `xmp:Label="Green"`, `darktable:colorlabels=2` |
| `9` | Blue Color Label | `xmp:Label="Blue"`, `darktable:colorlabels=3` |
| `P` | Pick Flag | Flags photo as pick (`flagged = 1`) |
| `X` | Reject Flag | Marks photo as reject (`flagged = -1`) |
| `U` | Unflag | Clears pick/reject status |
| `Shift + [1–9, P, X, U]` | **Cull + Auto-Advance** | Applies rating/flag and steps to next shot |
| `[` / `]` | Rotate ±90° | Rotates counter-clockwise / clockwise |
| `Ctrl + R` | Rotate 90° Clockwise | Updates EXIF orientation in DB & XMP |
| `Delete` | Trash Photo | Moves to system Trash with XMP sidecars |

---

## 2. Viewer & Canvas Navigation

| Key | Action | Notes |
| :--- | :--- | :--- |
| `Left` / `Right` | Previous / Next Photo | In 1-up photo viewer |
| `Space` | Next Photo | Quick advance |
| `Escape` | Back to Grid / Deselect | Exits viewer or clears selection |
| `Z` / `Double-Click` | **1:1 Full-Res Zoom** | Toggles 100% pixel lock on cursor |
| `+` / `=` / `KP_Add` | Zoom In | Smooth continuous step in |
| `-` / `KP_Subtract` | Zoom Out | Smooth continuous step out |
| `I` | Toggle Info Panel | Shows EXIF, sidecars, and histogram |
| `V` | Switch RAW / JPEG Version | Toggles embedded JPEG vs RAW sensor view |
| `C` | Compare Mode | Compares selected photos side-by-side |

---

## 3. Global & Grid Controls

| Key | Action | Notes |
| :--- | :--- | :--- |
| `Ctrl + O` | Import Photos | Opens import dialog / card picker |
| `Ctrl + E` | Export Selected | Opens export dialog |
| `Ctrl + F` | Search Library | Focuses search bar |
| `Ctrl + A` | Select All | Selects all photos in active view |
| `F5` | Slideshow | Starts a slideshow from the library |
| `Ctrl + Z` | **Undo** | 50-step reversible undo |
| `Ctrl + Shift + Z` | **Redo** | Reapplies undone action |
| `Ctrl + Q` | Quit Application | Clean exit |
