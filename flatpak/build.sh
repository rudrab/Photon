#!/bin/sh
# Build the Photon Flatpak locally, the same way the release workflow does.
#
#   flatpak/build.sh            build and install for this user
#   flatpak/build.sh --bundle   also write photon.flatpak (a single-file bundle)
#
# Needs flatpak-builder (or `flatpak install flathub org.flatpak.Builder`) and
# Python 3 with aiohttp, PyYAML and tomlkit for the Cargo source list.
set -eu
cd "$(dirname "$0")"

# The pinned flatpak-builder-tools commit whose cargo generator is used.
TOOLS_COMMIT=9d86d336a39b9af38b86f114b6ce5820fe260f91

# Every crate in Cargo.lock as a flatpak-builder source, for the offline build.
if [ ! -f cargo-sources.json ] || [ ../Cargo.lock -nt cargo-sources.json ]; then
    curl -fsSL -o flatpak-cargo-generator.py \
        "https://raw.githubusercontent.com/flatpak/flatpak-builder-tools/$TOOLS_COMMIT/cargo/flatpak-cargo-generator.py"
    python3 flatpak-cargo-generator.py ../Cargo.lock -o cargo-sources.json
fi

if command -v flatpak-builder >/dev/null 2>&1; then
    BUILDER=flatpak-builder
else
    BUILDER="flatpak run --command=flatpak-builder org.flatpak.Builder"
fi

$BUILDER --user --install-deps-from=flathub --force-clean --repo=repo --install \
    build-dir org.mavensgroup.photon.yml

if [ "${1:-}" = "--bundle" ]; then
    flatpak build-bundle repo ../photon.flatpak org.mavensgroup.photon \
        --runtime-repo=https://dl.flathub.org/repo/flathub.flatpakrepo
    echo "Wrote photon.flatpak"
fi
