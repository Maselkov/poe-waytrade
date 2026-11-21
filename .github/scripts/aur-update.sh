#!/bin/sh
# Updates a clone of the AUR package repo to a release:
#   .github/scripts/aur-update.sh 1.2.0 path/to/aur-clone
# Copies packaging/aur's PKGBUILD and install file in, sets pkgver (pkgrel 1)
# and the release tarball's checksum, and regenerates .SRCINFO. It doesn't
# build, commit or push: release.yml does that, and locally you can look at
# `git -C path/to/aur-clone diff` first.
#
# Only the tarball's checksum is updated. The OCR models stay pinned to the
# hashes in packaging/aur/PKGBUILD, so a changed upload fails the build
# instead of being shipped (updpkgsums would accept it).
set -eu
version=$1
aur=$2
here=$(dirname "$0")/../..

if ! printf '%s\n' "$version" | grep -Eqx '[0-9]+\.[0-9]+\.[0-9]+'; then
  echo "$version isn't a version like 22.0.0" >&2
  exit 1
fi

cp "$here/packaging/aur/PKGBUILD" "$here/packaging/aur/poe-waytrade.install" "$aur/"

url=$(sed -n 's/^url="\(.*\)"$/\1/p' "$aur/PKGBUILD")
tarball=$(mktemp)
trap 'rm -f "$tarball"' EXIT
curl -fsSL --retry 3 -o "$tarball" "$url/archive/v$version.tar.gz"
sum=$(sha256sum "$tarball" | cut -d' ' -f1)

# The tarball is the first entry of sha256sums, on the line that opens it.
sed -i -e "s/^pkgver=.*/pkgver=$version/" -e 's/^pkgrel=.*/pkgrel=1/' \
  -e "s/^sha256sums=('[0-9a-f]*'/sha256sums=('$sum'/" "$aur/PKGBUILD"
grep -q "^sha256sums=('$sum'" "$aur/PKGBUILD" || {
  echo "couldn't set the tarball's checksum in $aur/PKGBUILD" >&2
  exit 1
}

(cd "$aur" && makepkg --printsrcinfo > .SRCINFO)
grep -E '^\s*(pkgver|pkgrel) =' "$aur/.SRCINFO"
