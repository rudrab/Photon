# Installation & Build Guide

There are two ways to run Photon on Linux:

* **The Flatpak** from the [releases page](https://github.com/rudrab/Photon/releases): one file for any distribution (x86_64). This is the only binary package; there are no RPM or DEB packages any more, because distribution packages kept breaking on library versions.
* **A build from source**, for other architectures, for development, or to use the Intel GPU or NPU for AI.

Both use the same library folders, so you can switch between them.

---

## Install the Flatpak

```bash
# Once: Flathub provides the GNOME runtime Photon runs on
flatpak remote-add --user --if-not-exists flathub https://dl.flathub.org/repo/flathub.flatpakrepo

# Install the downloaded bundle, then start it (or use your app launcher)
flatpak install --user ./photon.flatpak
flatpak run org.mavensgroup.photon
```

To update, install the newer `photon.flatpak` the same way. To remove it: `flatpak uninstall org.mavensgroup.photon` (your library stays; see below).

### How the Flatpak differs from a sandboxed app

Photon is a front end to your own photos and tools, so its Flatpak is deliberately **not** a tight sandbox:

| What | How | Why |
| :--- | :--- | :--- |
| Your files | Full access to your home folder and drives (`--filesystem=host`) | Ratings, tags and rejects go to XMP sidecars next to the photos, wherever they are, including archives on external drives |
| Library folders | `~/.local/share/photon` (catalogue, models, backups), `~/.cache/photon` (thumbnails), `~/.config/photon` | The same as a build from source, not `~/.var/app/…`: one library whichever way you run Photon |
| Your programs | `darktable-cli`, `exiftool`, the video thumbnailers (`gst-video-thumbnailer`, `ffmpegthumbnailer` or `gst-launch-1.0`) and your editors run **on your system** (through `flatpak-spawn --host`) | RAW exports with your darktable edits, full metadata, and *Open With* your own darktable or GIMP. Install them with your distribution's packages; Photon reports any that are missing |
| AI | OpenVINO 2026.0 (Apache-2.0, CPU only) is **included** | Face, eye-focus and aesthetic analysis work out of the box; download the models in **Preferences → AI**. The Intel GPU and NPU need a build from source |
| Cameras and phones | Through GVfs, as in Files | Import from device |
| Network | Only to download AI models you approve | |

>[!NOTE]
>Because of these permissions (in particular running host programs), this Flatpak is published on GitHub, not on Flathub.

### Build the Flatpak yourself

```bash
git clone https://github.com/rudrab/Photon.git
cd Photon
flatpak/build.sh            # builds and installs it for your user
flatpak/build.sh --bundle   # also writes photon.flatpak
```

This needs `flatpak-builder` (from your distribution, or `flatpak install flathub org.flatpak.Builder`) and Python 3 with `aiohttp`, `PyYAML` and `tomlkit`, which turn `Cargo.lock` into an offline source list. The manifest is `flatpak/org.mavensgroup.photon.yml`; it builds libheif with HEIC support (the GNOME runtime's lacks it) and takes OpenVINO from its Apache-2.0 PyPI package.

---

# Building from Source

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
| Aesthetic model (about 612 MB) | Run, optional | Aesthetic score | Downloaded from Preferences → AI or on first analysis |
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

(The Flatpak does this by itself.)

To add Photon to your application launcher, use the desktop entry and icon shipped in `assets/`:

```bash
install -Dm755 target/release/photon ~/.local/bin/photon
install -Dm644 assets/org.mavensgroup.photon.desktop ~/.local/share/applications/org.mavensgroup.photon.desktop
install -Dm644 assets/org.mavensgroup.photon.svg ~/.local/share/icons/hicolor/scalable/apps/org.mavensgroup.photon.svg

update-desktop-database ~/.local/share/applications/
gtk-update-icon-cache -f ~/.local/share/icons/hicolor 2>/dev/null || true
```

Make sure `~/.local/bin` is on your `PATH`.
