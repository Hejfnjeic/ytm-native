#!/usr/bin/env bash
# Make a release on GitHub:   scripts/release.sh <version>      (e.g. scripts/release.sh 0.2.0)
#
#  1. sets the version in app/Cargo.toml and commits it ("Release v<version>")
#  2. builds self-updating release binaries (YTM_RELEASE_BUILD=1): Linux + Windows (cross)
#  3. packs them: Linux .tar.gz (program + install.sh), Windows portable .zip, Arch .pkg.tar.zst.
#     These three are the release's downloads, and what the app's updater fetches.
#  4. tags, pushes, and creates the GitHub release with them and notes from the git log
#
# Needs: gh (logged in), the Windows toolchain (see scripts/build-windows.sh), makepkg, 7z.
# Everything goes to dist/v<version>/ first; nothing is published until step 4 (after which
# dist/v<version>/ is removed: it's all on GitHub). Set
# DRY_RUN=1 to stop before step 4 (to look at the files), NOTES_FILE=<md> for hand-written notes.
set -euo pipefail
cd "$(dirname "$0")/.."

version="${1:?usage: scripts/release.sh <version>  (e.g. 0.2.0)}"
tag="v$version"
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "version must look like 1.2.3" >&2; exit 1; }
git diff --quiet && git diff --cached --quiet || { echo "commit or stash your changes first" >&2; exit 1; }
git rev-parse -q --verify "refs/tags/$tag" >/dev/null && { echo "$tag already exists" >&2; exit 1; }
if [[ -z "${DRY_RUN:-}" ]]; then
    gh auth status >/dev/null 2>&1 || { echo "gh isn't logged in: run 'gh auth login'" >&2; exit 1; }
fi
out="dist/$tag"
[[ -e "$out" ]] && { echo "$out already exists; move it away first" >&2; exit 1; }
mkdir -p "$out"

echo "== 1. version $version"
current=$(grep -m1 '^version' app/Cargo.toml | cut -d'"' -f2)
if [[ "$current" != "$version" ]]; then
    sed -i "0,/^version = \".*\"/s//version = \"$version\"/" app/Cargo.toml
    cargo update -p ytm-native --offline >/dev/null 2>&1 || cargo check -q -p ytm-native
    git commit -qam "Release $tag"
fi

echo "== 2. builds"
# Source paths end up in the binaries (panic locations): don't publish this machine's home
# directory (it holds the user name). Checked after the builds.
export RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=$HOME=/build"
# Same for the bundled C code (QuickJS; libopus on Windows): __FILE__ in its assertions.
export CFLAGS="${CFLAGS:-} -ffile-prefix-map=$HOME=/build"
# rquickjs-sys sets CFLAGS itself for Windows targets (dropping ours); the target-specific
# variable wins over it in the cc crate, so it repeats rquickjs' flags.
export CFLAGS_x86_64_pc_windows_gnu="-DWIN32_LEAN_AND_MEAN -std=c11 -ffile-prefix-map=$HOME=/build"
no_home() {
    if grep -qaF -e "$HOME" -e "/$(id -un)/" "$1"; then
        echo "$1 still contains paths from $HOME: not publishing" >&2
        exit 1
    fi
}
export YTM_RELEASE_BUILD=1
cargo test -q --release -p ytm-native
cargo build --release -p ytm-native
linux="$out/.linux-bin"
cp target/release/ytm-native "$linux"
scripts/build-windows.sh
mv "dist/ytm-native-portable-$version.zip" "$out/"
unset YTM_RELEASE_BUILD
scripts/build-arch.sh
mv dist/ytm-native-"$version"-*-x86_64.pkg.tar.zst "$out/"
no_home "$linux"
winexe="$out/.win-exe"
unzip -p "$out/ytm-native-portable-$version.zip" ytm-native.exe > "$winexe"
no_home "$winexe"
rm "$winexe"
pkgbin="$out/.pkg-bin"
tar -xOf "$out"/ytm-native-"$version"-*-x86_64.pkg.tar.zst usr/bin/ytm-native > "$pkgbin"
no_home "$pkgbin"
rm "$pkgbin"

echo "== 3. Linux tarball"
stage="$out/ytm-native-$version"
mkdir -p "$stage"
cp "$linux" "$stage/ytm-native"
cp scripts/install-linux.sh "$stage/install.sh"
cp packaging/ytm-native.desktop app/assets/icon/icon.svg app/assets/icon/icon-small.svg README.md LICENSE "$stage/"
tar -C "$out" -czf "$out/ytm-native-$version-linux-x86_64.tar.gz" "ytm-native-$version"
rm -r "$stage" "$linux"

prev=$(git describe --tags --abbrev=0 "HEAD^" 2>/dev/null || true)
{
    if [[ -n "${NOTES_FILE:-}" ]]; then
        cat "$NOTES_FILE"   # hand-written changes instead of the commit list
    else
    echo "## Changes"
    git log --no-merges --format='- %s' ${prev:+"$prev..HEAD"} | grep -v "^- Release v" | grep -v "^- Docs:" || true
    fi
    echo
    echo "## Install"
    echo "- **Arch Linux:** \`sudo pacman -U ytm-native-$version-*-x86_64.pkg.tar.zst\` (pacman handles updates; the app tells you when one is out)."
    echo "- **Other Linux:** unpack \`ytm-native-$version-linux-x86_64.tar.gz\` and run \`./install.sh\` (installs to ~/.local; updates itself)."
    echo "- **Windows:** unpack \`ytm-native-portable-$version.zip\` into a folder of its own and run \`ytm-native.exe\` (portable; updates itself). Keep \`WebView2Loader.dll\` beside it."
} > "$out/notes.md"

files=("$out"/ytm-native-"$version"-linux-x86_64.tar.gz "$out"/ytm-native-portable-"$version".zip \
       "$out"/ytm-native-"$version"-*-x86_64.pkg.tar.zst)
ls -l "${files[@]}"
if [[ -n "${DRY_RUN:-}" ]]; then
    echo "DRY_RUN: stopping before publishing. Notes: $out/notes.md"
    exit 0
fi

echo "== 4. publish $tag"
git tag -a "$tag" -m "ytm-native $version"
git push origin HEAD --follow-tags
gh release create "$tag" "${files[@]}" --title "ytm-native $version" --notes-file "$out/notes.md"

# Everything is on GitHub now: the files here and the Arch package's own build (a whole
# second compile, ~7 GB) would only fill the disk. The notes are kept beside the scratch files.
mkdir -p target/scratch
cp "$out/notes.md" "target/scratch/notes-$version.md"
rm -r "$out" target/arch-build target/arch-pkg
echo "Published $tag; removed $out and the Arch build folders."
