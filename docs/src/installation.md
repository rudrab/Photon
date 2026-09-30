# Installation & Build Guide

## System Requirements

* **Operating System:** Linux (Kernel 5.15+), Wayland or X11 session.
* **Rust Toolchain:** Rust 1.75 or newer (`rustup default stable`).
* **Hardware:** Any modern x86_64 or aarch64 CPU, 4 GB+ RAM.

---

## 1. Dependencies

### What each one is for

| Dependency | Kind | Needed for | If missing |
| :--- | :--- | :--- | :--- |
| Rust 1.75+, C compiler, `pkg-config` | Build | Compiling | Build fails |
| GTK 4 (4.12+), libadwaita (1.4+) | Build + run | The interface | Build fails |
| SQLite | Build + run | The catalogue (bundled by default) | (none) |
| LittleCMS 2 (`lcms2`) | Build + run | Colour management, ICC export | Build fails |
| libheif | Build + run | HEIC / HEIF / AVIF decoding | Build fails |
| GStreamer + plugins (`-good`, `-libav`) | Run | Video playback and thumbnails | Videos do not play |
| **OpenVINO 2026.x + oneTBB** | Run, optional | [AI focus and quality analysis](guide/ai_quality.md) | AI is shown as unavailable; everything else works |
| YuNet face model (about 230 KB) | Run, optional | Eye-focus scoring | Downloaded from Preferences → AI |
| Intel GPU / NPU drivers | Run, optional | Faster AI inference | The CPU is used |
| darktable (`darktable-cli`) | Run, optional | RAW export rendered with your darktable edits | Photon's built-in RAW renderer is used |
| `exiftool` | Run, optional | Fuller metadata in exports; video dates | A reduced built-in metadata writer is used |

RAW decoding uses the pure-Rust `rawler` crate, so **LibRaw is not required**. The Rust crates are fetched by Cargo.

>[!NOTE]
>Everything marked *optional* can be added later. Photon detects it at start-up and enables the matching feature.

---

## 2. Install System Dependencies

### Fedora / RHEL / CentOS Stream
```bash
sudo dnf install -y \
    gcc gcc-c++ pkgconf-pkg-config \
    gtk4-devel libadwaita-devel \
    sqlite-devel lcms2-devel libheif-devel \
    gstreamer1-devel gstreamer1-plugins-base-devel \
    gstreamer1-plugins-bad-free-devel
```

### Debian 12 / Ubuntu 23.10+ / Linux Mint
```bash
sudo apt update && sudo apt install -y \
    build-essential pkg-config \
    libgtk-4-dev libadwaita-1-dev \
    libsqlite3-dev liblcms2-dev libheif-dev \
    libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
    libgstreamer-plugins-bad1.0-dev
```

### Arch Linux / Manjaro
```bash
sudo pacman -S --needed \
    base-devel gtk4 libadwaita sqlite lcms2 \
    libheif gstreamer gst-plugins-base
```

---

## 3. Compile from Source

```bash
# 1. Clone the repository
git clone https://github.com/rudrab/Photon.git
cd Photon

# 2. Run the test suite
cargo test --release

# 3. Build the optimized release binary
cargo build --release -p photon-app

# 4. Run Photon
./target/release/photon
```

---

## 4. Optional: AI Analysis and Other Helpers

```bash
# Fedora: AI runtime (OpenVINO needs oneTBB, which it does not pull in)
sudo dnf install openvino tbb

# Fedora: optional helpers
sudo dnf install darktable perl-Image-ExifTool gstreamer1-plugins-good gstreamer1-libav
```

Then start Photon and open **Preferences → AI** to check the detected devices and download the face model. See [AI Focus & Quality Analysis](guide/ai_quality.md) for details, including GPU and NPU notes.

---

## 5. Desktop Integration (Optional)

To add Photon to your application launcher, use the desktop entry and icon shipped in `assets/`:

```bash
install -Dm755 target/release/photon ~/.local/bin/photon
install -Dm644 assets/org.mavensgroup.photon.desktop ~/.local/share/applications/org.mavensgroup.photon.desktop
install -Dm644 assets/org.mavensgroup.photon.svg ~/.local/share/icons/hicolor/scalable/apps/org.mavensgroup.photon.svg

update-desktop-database ~/.local/share/applications/
gtk-update-icon-cache -f ~/.local/share/icons/hicolor 2>/dev/null || true
```

Make sure `~/.local/bin` is on your `PATH`.
