# Smart Collections & Rules

Smart Collections in Photon are dynamic, saved queries that automatically update their contents and live count badges as your library changes.

---

## 1. Creating a Smart Collection

1. In the navigation sidebar, locate the **Smart Collections** section.
2. Click the **`+`** button in the section header.
3. Configure your desired filtering criteria in the dialog:
   * **Collection Name:** e.g., "5-Star Landscapes", "Recent Sony Picks".
   * **Minimum Star Rating:** Any rating, ★ 1+, ★ 2+, ★ 3+, ★ 4+, or ★ 5.
   * **Status / Flag:** Any status, Picks only, Unflagged only, or Rejects only.
   * **Colour Label:** Any colour, No colour (None), 🔴 Red, 🟡 Yellow, 🟢 Green, 🔵 Blue, or 🟣 Purple.
   * **Text Search / Keyword:** Substring or filename match (e.g., "sunset", "raw").
   * **Included Tags:** Comma-separated list of required tags (e.g. `nature, travel`).
   * **Excluded Tags:** Comma-separated list of excluded tags (e.g. `draft, private`).
   * **Camera Model:** e.g., `Sony A7IV`, `Canon EOS R5`.
   * **Lens Model:** e.g., `50mm`, `24-70mm`.
   * **Exclude Rejected Photos:** Automatically excludes `flagged = -1` items.
4. Click **Create Collection**.

---

## 2. Live Dynamic Evaluation

Smart collections are stored as structured JSON queries in the `smart_collections` SQLite table:

* Every time photos are imported, rated, flagged, or tagged, Photon re-evaluates the query in SQL.
* The badge next to the smart collection in the sidebar displays the **live photo count** in real-time.
* Selecting the collection displays matching photos in the virtualized timeline.

---

## 3. Managing Collections

Right-click any Smart Collection in the sidebar to open the context menu:
* **Edit Rules…:** Re-opens the rules dialog with current criteria populated.
* **Rename…:** Quick rename dialog.
* **Delete Collection:** Removes the smart collection from the sidebar (underlying photos remain safe in your library).
