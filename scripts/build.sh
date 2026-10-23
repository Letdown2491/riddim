#!/usr/bin/env bash
# Builds riddim inside a container (kumaOS is atomic: no host toolchain).
# Derives localhost/riddim-dev from kuma-dev-gcc on first use; rustup and
# the cargo registry live in named volumes so later builds are incremental.
#
#   ./scripts/build.sh build
#   ./scripts/build.sh test
#   ./scripts/build.sh run
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
  "$IMAGE" \
  bash -c '
    if [ ! -x /root/.cargo/bin/cargo ]; then
      curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs |
        sh -s -- -y --profile minimal --default-toolchain 1.97.1
    fi
    export PATH=/root/.cargo/bin:$PATH
    cargo "$@"
  ' _ "$@"
