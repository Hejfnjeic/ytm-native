#!/usr/bin/env bash
# Install ytm-native for the current user (no root): binary, menu entry, icons.
#
#   scripts/install-linux.sh              build from source, then install
#   ./install.sh                          (in a release tarball) install the included binary
#   ... --uninstall                       remove them again (login and settings stay)
#
# Release binaries update themselves; one built from source only says when a new version is out.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
bin="$HOME/.local/bin/ytm-native"
desktop="$HOME/.local/share/applications/ytm-native.desktop"
icons="$HOME/.local/share/icons/hicolor"

# Release tarball: binary and assets sit next to this script. Source tree: one level up.
if [[ -x "$here/ytm-native" ]]; then
    src_bin="$here/ytm-native"; assets="$here"
else
    root="$(cd "$here/.." && pwd)"; src_bin="$root/target/release/ytm-native"; assets=""
fi

if [[ "${1:-}" == "--uninstall" ]]; then
    rm -f "$bin" "$desktop" "$icons/scalable/apps/ytm-native.svg"
    for n in 16 22 24; do rm -f "$icons/${n}x${n}/apps/ytm-native.svg"; done
else
    if [[ -z "$assets" ]]; then
        (cd "$root" && cargo build --release -p ytm-native)
        desk_src="$root/packaging/ytm-native.desktop"; icon_src="$root/app/assets/icon/icon.svg"; small_src="$root/app/assets/icon/icon-small.svg"
    else
        desk_src="$assets/ytm-native.desktop"; icon_src="$assets/icon.svg"; small_src="$assets/icon-small.svg"
    fi
    install -Dm755 "$src_bin" "$bin"
    # Full path: the Plasma session's PATH doesn't include ~/.local/bin (only shells add it).
    install -Dm644 "$desk_src" "$desktop"
    sed -i "s|^Exec=ytm-native\$|Exec=$bin|" "$desktop"
    # Scalable only: with fixed-size PNGs next to it, KDE stretches the nearest one to the
    # panel's size (blurry); the SVG is rendered at exactly the size needed.
    install -Dm644 "$icon_src" "$icons/scalable/apps/ytm-native.svg"
    # Simplified version for tiny sizes (title bar, 16-24 px); still vector.
    for n in 16 22 24; do install -Dm644 "$small_src" "$icons/${n}x${n}/apps/ytm-native.svg"; done
fi
command -v update-desktop-database >/dev/null && update-desktop-database -q "$HOME/.local/share/applications" || true
command -v kbuildsycoca6 >/dev/null && kbuildsycoca6 >/dev/null 2>&1 || true
echo "done"
