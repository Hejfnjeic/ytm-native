#!/usr/bin/env bash
# Make a release on GitHub:   scripts/release.sh <version>      (e.g. scripts/release.sh 0.2.0)
#
#  1. sets the version in app/Cargo.toml and commits it ("Release v<version>")
#  2. builds self-updating release binaries (YTM_RELEASE_BUILD=1): Linux + Windows (cross)
#  3. packs them: Linux .tar.gz (binary + install.sh), Windows portable .7z, Arch .pkg.tar.zst
#  4. downloads the last 5 releases' executables and makes update patches from each of them,
#     plus update-manifest.json (cargo xtask release-files)
#  5. tags, pushes, and creates the GitHub release with all files and notes from the git log
#
# Needs: gh (logged in), the Windows toolchain (see scripts/build-windows.sh), makepkg, 7z.
# Everything goes to dist/v<version>/ first; nothing is published until step 5. Set
# DRY_RUN=1 to stop before step 5 (to look at the files), NOTES_FILE=<md> for hand-written notes.
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
mkdir -p "$out/previous"

echo "== 1. version $version"
current=$(grep -m1 '^version' app/Cargo.toml | cut -d'"' -f2)
if [[ "$current" != "$version" ]]; then
    sed -i "0,/^version = \".*\"/s//version = \"$version\"/" app/Cargo.toml
    cargo update -p ytm-native --offline >/dev/null 2>&1 || cargo check -q -p ytm-native
    git commit -qam "Release $tag"
fi

echo "== 2. builds"
export YTM_RELEASE_BUILD=1
cargo test -q --release -p ytm-native
cargo build --release -p ytm-native
linux="$out/ytm-native-$version-linux-x86_64"
cp target/release/ytm-native "$linux"
scripts/build-windows.sh
mv "dist/ytm-native-$version-windows-x86_64.exe" "dist/ytm-native-$version-windows-x86_64.7z" "$out/"
unset YTM_RELEASE_BUILD
scripts/build-arch.sh
mv dist/ytm-native-"$version"-*-x86_64.pkg.tar.zst "$out/"

echo "== 3. Linux tarball"
stage="$out/ytm-native-$version"
mkdir -p "$stage"
cp "$linux" "$stage/ytm-native"
cp scripts/install-linux.sh "$stage/install.sh"
cp packaging/ytm-native.desktop app/assets/icon.svg app/assets/icon-small.svg README.md LICENSE "$stage/"
tar -C "$out" -czf "$out/ytm-native-$version-linux-x86_64.tar.gz" "ytm-native-$version"
mv "$stage" "$out/.staging-linux"

echo "== 4. update patches"
if command -v gh >/dev/null && gh release list --limit 1 >/dev/null 2>&1; then
    for t in $(gh release list --limit 5 --json tagName -q '.[].tagName'); do
        gh release download "$t" -D "$out/previous" -p 'ytm-native-*-linux-x86_64' -p 'ytm-native-*-windows-x86_64.exe' 2>/dev/null || true
    done
fi
cargo xtask release-files "$out" "$version"

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
    echo "- **Windows:** unpack \`ytm-native-$version-windows-x86_64.7z\` anywhere and run \`ytm-native.exe\` (portable; updates itself)."
    echo
    echo "The \`.bsdiff\` files and \`update-manifest.json\` are for the app's own updater."
} > "$out/notes.md"

files=("$out"/ytm-native-"$version"-linux-x86_64 "$out"/ytm-native-"$version"-linux-x86_64.tar.gz \
       "$out"/ytm-native-"$version"-windows-x86_64.exe "$out"/ytm-native-"$version"-windows-x86_64.7z \
       "$out"/ytm-native-"$version"-*-x86_64.pkg.tar.zst "$out"/update-manifest.json)
shopt -s nullglob
files+=("$out"/*.bsdiff)
ls -l "${files[@]}"
if [[ -n "${DRY_RUN:-}" ]]; then
    echo "DRY_RUN: stopping before publishing. Notes: $out/notes.md"
    exit 0
fi

echo "== 5. publish $tag"
git tag -a "$tag" -m "ytm-native $version"
git push origin HEAD --follow-tags
gh release create "$tag" "${files[@]}" --title "ytm-native $version" --notes-file "$out/notes.md"
