# Darktable 2-Way Synchronization

Photon is engineered as the primary ingestion, culling, and Digital Asset Management companion for **Darktable**.

---

## 1. Non-Destructive Sidecar Synchronization

Photon stores metadata in standard `.xmp` sidecars associated with your RAW and raster files:

```xml
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:xmp="http://ns.adobe.com/xap/1.0/"
    xmlns:darktable="http://darktable.sf.net/"
    xmlns:dc="http://purl.org/dc/elements/1.1/"
    xmlns:tiff="http://ns.adobe.com/tiff/1.0/"
    xmp:Rating="4"
    xmp:Label="Red"
    tiff:Orientation="1">
   <darktable:colorlabels>
    <rdf:Seq>
     <rdf:li>0</rdf:li>
    </rdf:Seq>
   </darktable:colorlabels>
   <dc:subject>
    <rdf:Bag>
     <rdf:li>Landscape</rdf:li>
     <rdf:li>Sunset</rdf:li>
    </rdf:Bag>
   </dc:subject>
   <!-- Darktable edit history is preserved untouched -->
   <darktable:history>
    ...
   </darktable:history>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>
```

---

## 2. Edit History Protection

Other photo managers often overwrite or invalidate Darktable's non-destructive processing modules when editing metadata. 

Photon's XML engine (`photon-import/src/sidecar.rs`):
* Parses existing sidecars and **preserves all `<darktable:history>` blocks, parametric masks, and module states intact**.
* Only updates ratings, color labels, tags, titles, and descriptions.

---

## 3. Color Label Mapping

Color labels are synchronized across standard Adobe/Lightroom formats and Darktable sequences:

| Color | `xmp:Label` | `darktable:colorlabels` sequence | Key |
| :--- | :--- | :--- | :--- |
| **None** | Removed | Removed | `0` |
| **Red** | `Red` | `0` | `6` |
| **Yellow** | `Yellow` | `1` | `7` |
| **Green** | `Green` | `2` | `8` |
| **Blue** | `Blue` | `3` | `9` |
| **Purple** | `Purple` | `4` | Menu |

---

## 4. Live Focus Sync & Ping-Pong Loop Prevention

* **External Changes Detected:** When you switch back to the Photon window from Darktable, Photon automatically checks the `mtime` of sidecars and refreshes ratings or tags applied in Darktable.
* **Timestamp Tracking:** Photon records the exact `xmp_mtime` written during its own saves, preventing infinite reload cycles.

---

## 5. Recommended Darktable Settings

To ensure the smoothest two-way synchronization:
1. In Darktable, open **Preferences → Storage**.
2. Set **"write sidecar file for each image"** to **"on edit"** or **"always"**.
3. When returning to Darktable after culling in Photon, select the photos in the Lighttable and click **"reload selected XMP files"** (or enable automatic sidecar lookup on startup).
