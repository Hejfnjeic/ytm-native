#!/usr/bin/env bash
# Cross-compile the Windows version and pack it as a portable .7z:
#   scripts/build-windows.sh   -> dist/ytm-native-<ver>-windows-x86_64.7z
#                                 (+ dist/ytm-native-<ver>-windows-x86_64.exe, used by updates)
# The .7z holds "YT Music native/" with ytm-native.exe, data/ (login, queue, history), README.
# Needs: rustup target x86_64-pc-windows-gnu, mingw-w64-gcc, cmake, 7zip.
# YTM_RELEASE_BUILD=1 in the environment makes a build that updates itself (scripts/release.sh).
set -euo pipefail
cd "$(dirname "$0")/.."
version=$(grep -m1 '^version' app/Cargo.toml | cut -d'"' -f2)
cargo build --release -p ytm-native --target x86_64-pc-windows-gnu
exe=target/x86_64-pc-windows-gnu/release/ytm-native.exe
out="dist/YT Music native"
mkdir -p dist
rm -rf "$out"   # the unpacked folder is rebuilt from scratch every time
mkdir -p "$out/data"
cp "$exe" "$out/ytm-native.exe"
cp "$exe" "dist/ytm-native-$version-windows-x86_64.exe"
cat > "$out/README.txt" <<'TXT'
YT Music native (unofficial)

Portable: everything the app saves stays in this folder.
  data\   your login (encrypted, can only be opened by your Windows user on this PC),
          queue, settings and the list of recently played songs
  cache\  re-creatable files (created when needed)
Delete the whole folder to remove everything.

Sign in: start ytm-native.exe and follow the steps on screen.
Updates install themselves (Settings > Updates); unpacking a newer .7z over this folder works too.
TXT
archive="dist/ytm-native-$version-windows-x86_64.7z"
rm -f "$archive"
(cd dist && 7z a -t7z -mx=9 "$(basename "$archive")" "YT Music native" >/dev/null)
ls -l "$archive" "dist/ytm-native-$version-windows-x86_64.exe"
