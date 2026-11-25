#!/usr/bin/env bash
# Cross-compile the Windows version and pack it as a portable .zip:
#   scripts/build-windows.sh   -> dist/ytm-native-portable-<ver>.zip
# The name is the one the app's updater looks for (update.rs, asset_name), and it unpacks
# the zip expecting ytm-native.exe at the top, so the files aren't in a folder of their own:
# ytm-native.exe, WebView2Loader.dll, portable.txt, data/ (login, queue, history), README.
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
# The lyrics check's web view (WebView2) needs Microsoft's loader beside the exe: MinGW
# links it as a DLL, not statically.
loader=$(ls -d ~/.cargo/registry/src/*/webview2-com-sys-*/x64/WebView2Loader.dll | sort -V | tail -1)
cp "$loader" "$out/WebView2Loader.dll"
# Portable: settings and login in data\ beside the exe, and updates replace it in place.
echo "Portable build: everything is kept in the data folder beside ytm-native.exe." > "$out/portable.txt"
cat > "$out/README.txt" <<'TXT'
YT Music native (unofficial)

Portable: everything the app saves stays in this folder.
  data\   your login (encrypted, can only be opened by your Windows user on this PC),
          queue, settings and the list of recently played songs
Delete the whole folder to remove everything.

Sign in: start ytm-native.exe and follow the steps on screen.
Updates install themselves (Settings > Updates); unpacking a newer .zip over this folder works too.
TXT
archive="$PWD/dist/ytm-native-portable-$version.zip"
rm -f "$archive"
(cd "$out" && 7z a -tzip -mx=9 "$archive" . >/dev/null)
rm -r "$out"
ls -l "$archive"
