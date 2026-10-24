#!/usr/bin/env bash
# Builds riddim inside a container (kumaOS is atomic: no host toolchain).
# Derives localhost/riddim-dev from kuma-dev-gcc on first use; rustup and
# the cargo registry live in named volumes so later builds are incremental.
#
#   ./scripts/build.sh build          # debug build, installs to ~/.local/bin
#   ./scripts/build.sh build --release
#   ./scripts/build.sh test
#   ./scripts/build.sh clippy --all-targets
#   ./scripts/build.sh fmt --all --check
#
# Set SPOTIFY_CLIENT_ID on the host to skip the setup screen at launch.
set -euo pipefail
cd "$(dirname "$0")/.."

IMAGE=localhost/riddim-dev

if ! podman image exists "$IMAGE"; then
  podman build -t "$IMAGE" -f containers/riddim-dev.containerfile containers/
fi

podman run --rm \
  -v "$PWD":/work:Z -w /work \
  -v riddim-cargo-home:/root/.cargo \
  -v riddim-rustup-home:/root/.rustup \
  --env SPOTIFY_CLIENT_ID \
  --env RIDDIM_UI_BASELINE \
  --env RUSTFLAGS='-C link-arg=-Wl,-rpath,$ORIGIN/riddim-libs' \
  "$IMAGE" \
  bash -c '
    if [ ! -x /root/.cargo/bin/cargo ]; then
      curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs |
        sh -s -- -y --profile minimal --default-toolchain 1.97.1
    fi
    export PATH=/root/.cargo/bin:$PATH
    cargo "$@"
  ' _ "$@"

# The binary runs on the host, where the Wayland session, Vulkan driver,
# audio stack, and secret service live. The container's library set is a
# superset of the host's: any shared library the binary needs that the host
# lacks (libxkbcommon-x11 on this Wayland-only OS, today) is copied from the
# image into riddim-libs/ beside the binary, which the rpath above resolves.
if [ "${1:-}" = "build" ]; then
  profile=debug
  for argument in "$@"; do
    case "$argument" in --release) profile=release ;; esac
  done
  binary="target/$profile/riddim"
  libs="target/$profile/riddim-libs"
  mkdir -p "$libs"
  missing=$(ldd "$binary" 2>/dev/null | awk '/not found/ {print $1}' | sort -u)
  if [ -n "$missing" ]; then
    podman run --rm -v "$PWD":/work:Z "$IMAGE" bash -c "
      for lib in $missing; do cp -L \"/usr/lib64/\$lib\" /work/$libs/; done
    "
  fi
  if ldd "$binary" 2>/dev/null | grep -q 'not found'; then
    echo "error: unresolved shared libraries:" >&2
    ldd "$binary" 2>/dev/null | grep 'not found' >&2
    exit 1
  fi
  # rename(2) over a running binary works where cp fails with Text file busy
  mkdir -p ~/.local/bin
  cp "$binary" ~/.local/bin/riddim.new
  mv -f ~/.local/bin/riddim.new ~/.local/bin/riddim
  rm -rf ~/.local/bin/riddim-libs
  cp -r "$libs" ~/.local/bin/riddim-libs
  # Desktop entry and icon, so launchers and the taskbar show riddim's mark
  # (the window's Wayland app_id / X11 WM_CLASS is the file's stem). The
  # entry's Exec is rewritten to the installed binary's absolute path,
  # since launchers do not expand ~ or rely on PATH.
  mkdir -p ~/.local/share/applications ~/.local/share/icons/hicolor/scalable/apps
  sed "s|^Exec=.*|Exec=$HOME/.local/bin/riddim|" \
    packaging/io.github.letdown2491.riddim.desktop \
    > ~/.local/share/applications/io.github.letdown2491.riddim.desktop
  cp -f assets/riddim-icon.svg \
    ~/.local/share/icons/hicolor/scalable/apps/io.github.letdown2491.riddim.svg
  update-desktop-database ~/.local/share/applications 2>/dev/null || true
fi
