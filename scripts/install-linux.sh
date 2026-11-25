#!/usr/bin/env bash
# Install ytm-native for the current user (no root): program, menu entry, icons.
#
#   scripts/install-linux.sh              build from source, then install
#   ./install.sh                          (in a release tarball) install the included program
#   ... --uninstall                       remove them again (login and settings stay)
#
# The program goes to ~/.local/lib/ytm-native/, with ~/.local/bin/ytm-native pointing at it.
# A release tarball's copy also gets install.txt there: the app then updates itself from
# GitHub releases (update.rs), replacing ~/.local/lib/ytm-native/ytm-native as it closes.
# A copy built from source doesn't: it never updates itself.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
lib="$HOME/.local/lib/ytm-native"
bin="$HOME/.local/bin/ytm-native"
desktop="$HOME/.local/share/applications/ytm-native.desktop"
icons="$HOME/.local/share/icons/hicolor"

# Release tarball: the program and assets sit next to this script. Source tree: one level up.
if [[ -x "$here/ytm-native" && "$here" != "$lib" ]]; then
    src_bin="$here/ytm-native"; assets="$here"
elif [[ "$here" == "$lib" ]]; then
    src_bin=""; assets=""   # the copy kept beside the installed program: only --uninstall
else
    root="$(cd "$here/.." && pwd)"; src_bin="$root/target/release/ytm-native"; assets=""
fi

if [[ "${1:-}" == "--uninstall" ]]; then
    rm -f "$bin" "$desktop" "$icons/scalable/apps/ytm-native.svg"
    for n in 16 22 24; do rm -f "$icons/${n}x${n}/apps/ytm-native.svg"; done
    rm -rf "$lib"
    echo "removed (your login and settings in ~/.local/share/ytm-native stay)"
    exit 0
fi
[[ -n "$src_bin" ]] || { echo "run the install.sh from the downloaded folder" >&2; exit 1; }

if [[ -z "$assets" ]]; then
    (cd "$root" && cargo build --release -p ytm-native)
    desk_src="$root/packaging/ytm-native.desktop"; icon_src="$root/app/assets/icon/icon.svg"; small_src="$root/app/assets/icon/icon-small.svg"
else
    desk_src="$assets/ytm-native.desktop"; icon_src="$assets/icon.svg"; small_src="$assets/icon-small.svg"
fi
mkdir -p "$lib" "$(dirname "$bin")"
# A new file renamed into place: a running copy keeps its old one until it closes.
install -m755 "$src_bin" "$lib/.ytm-native.new"
mv -f "$lib/.ytm-native.new" "$lib/ytm-native"
cp "$here/$(basename "$0")" "$lib/install.sh" 2>/dev/null || true
if [[ -n "$assets" ]]; then
    echo "Installed by install.sh from a release: updates itself." > "$lib/install.txt"
else
    rm -f "$lib/install.txt"
fi
# Before 1.0 the program itself was here; now it points at the one in ~/.local/lib.
ln -sfn "$lib/ytm-native" "$bin"
# Full path: the Plasma session's PATH doesn't include ~/.local/bin (only shells add it).
install -Dm644 "$desk_src" "$desktop"
sed -i "s|^Exec=ytm-native\$|Exec=$bin|" "$desktop"
# Scalable only: with fixed-size PNGs next to it, KDE stretches the nearest one to the
# panel's size (blurry); the SVG is rendered at exactly the size needed.
install -Dm644 "$icon_src" "$icons/scalable/apps/ytm-native.svg"
# Simplified version for tiny sizes (title bar, 16-24 px); still vector.
for n in 16 22 24; do install -Dm644 "$small_src" "$icons/${n}x${n}/apps/ytm-native.svg"; done
command -v update-desktop-database >/dev/null && update-desktop-database -q "$HOME/.local/share/applications" || true
command -v kbuildsycoca6 >/dev/null && kbuildsycoca6 >/dev/null 2>&1 || true
echo "done"
