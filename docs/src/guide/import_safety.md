# Ingestion & Backup Safety

Photon is designed with strict file integrity guarantees to ensure that zero photos are lost or corrupted during card ingest, even in the event of hardware disconnection or power failure.

---

## 1. Staged Atomic Commit Pipeline

When importing photos into a managed library structure (`YYYY/MM/DD`), Photon implements a three-stage transactional pipeline:

```
[ Camera Card / Source ]
           │
           ▼
[ 1. Staged Copy ] ──► Write to temporary destination (.part)
           │
           ▼
[ 2. Byte Hash Verification ] ──► BLAKE3 streaming verification
           │
           ▼
[ 3. Atomic Rename ] ──► Atomic filesystem rename to final filename
           │
           ▼
[ 4. Database Transaction ] ──► SQLite WAL progressive batch insert
```

* **Zero Partial Files:** If a memory card is pulled or the process is killed mid-copy, Photon's startup sweep automatically purges dangling `.part` files.
* **Non-Blocking Ingest:** Photos are progressively committed in batches, making them visible in the timeline while the remainder of the card imports.

---

## 2. Card-Wipe Mirror Backup Protection

Performing a **Move** import from an SD card is normally risky. Photon provides built-in hardware protection:

* **Automated Secondary Mirroring:** In **Preferences → Import Safety**, configure a secondary backup location (such as an external SSD or network share).
* **Safe Sequence:** When moving files, Photon writes the copy to the primary library, verifies the second copy to the backup destination, and only deletes the file from the SD card after **both copies pass full hash verification**.

---

## 3. Streaming Hash Deduplication

Photon computes streaming BLAKE3 content hashes for all imported media:
* **In-Place Duplicates:** If a photo is already registered in the database, Photon skips copying without re-reading the full file.
* **Same-Hash Protection:** If the exact same file is imported under a different name or from another folder, Photon recognizes the duplicate hash and records the reference without duplicating on-disk storage.

---

## 4. RAW + JPEG Shot Grouping

When shooting RAW+JPEG pairs:
* Photon groups pairs under a single `group_hash` based on timestamp, camera serial number, and capture metadata.
* In the timeline, the shot appears as a single unified card.
* In the 1-up viewer, a version switcher button (`V`) allows instant switching between the embedded camera JPEG and the raw sensor data.
* Any rating, color label, pick flag, or tag applied to the shot applies to both the RAW and JPEG sidecars simultaneously.
