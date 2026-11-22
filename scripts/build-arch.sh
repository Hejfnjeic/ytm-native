#!/usr/bin/env bash
# Build an Arch package (.pkg.tar.zst) from the current checkout (committed files only):
#   scripts/build-arch.sh   -> dist/ytm-native-<ver>-<rel>-x86_64.pkg.tar.zst
# Install it with: sudo pacman -U dist/ytm-native-*.pkg.tar.zst
set -euo pipefail
repo="$(cd "$(dirname "$0")/.." && pwd)"
ver=$(grep -m1 '^version' "$repo/app/Cargo.toml" | cut -d'"' -f2)
work="$repo/target/arch-pkg"
mkdir -p "$work" "$repo/dist"
sed "s/^pkgver=.*/pkgver=$ver/" "$repo/packaging/arch/PKGBUILD" > "$work/PKGBUILD"
# Same layout as GitHub's tag tarball (ytm-native-<ver>/...), so the PKGBUILD needs no changes.
git -C "$repo" archive --format=tar.gz --prefix="ytm-native-$ver/" -o "$work/ytm-native-$ver.tar.gz" HEAD
cd "$work"
# Reuse compiled dependencies between package builds.
CARGO_TARGET_DIR="$repo/target/arch-build" makepkg -f --skipchecksums --noconfirm
mv -f ytm-native-"$ver"-*.pkg.tar.zst "$repo/dist/"
ls -l "$repo"/dist/ytm-native-"$ver"-*.pkg.tar.zst
