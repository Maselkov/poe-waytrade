#!/bin/sh
# Build dependencies for CI, in an archlinux container. Arch because the
# layer embeds its assets with C23 #embed, which needs GCC 15.
# With arguments, installs only those packages (e.g. `rust gcc` for the
# daemon's lint and test jobs); without, everything the full build needs.
set -eu
if [ $# -gt 0 ]; then
  pacman -Syu --noconfirm --needed "$@"
  exit 0
fi
pacman -Syu --noconfirm --needed \
  base-devel git cmake ninja vulkan-headers vulkan-icd-loader \
  wayland wayland-protocols shaderc freetype2 rust github-cli
# makepkg refuses to run as root.
id builder >/dev/null 2>&1 || useradd -m builder
chown -R builder "$GITHUB_WORKSPACE"
