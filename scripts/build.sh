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
# audio stack, and secret service live.
if [ "${1:-}" = "build" ]; then
  mkdir -p ~/.local/bin
  profile=debug
  for argument in "$@"; do
    case "$argument" in --release) profile=release ;; esac
  done
  # rename(2) over a running binary works where cp fails with Text file busy
  cp "target/$profile/riddim" ~/.local/bin/riddim.new
  mv -f ~/.local/bin/riddim.new ~/.local/bin/riddim
fi
