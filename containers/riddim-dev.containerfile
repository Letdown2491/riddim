# Dev image — kumaOS is atomic and has no host toolchain, so cargo runs in
# this container. Derived from the same base as kumaui's dev image.
# Build:  podman build -t localhost/riddim-dev -f containers/riddim-dev.containerfile containers/
# Check:  podman run --rm -v "$PWD:/work:Z" -w /work localhost/riddim-dev cargo check
# (:Z relabels the mount for SELinux on atomic hosts.)
#
# The vulkan/wayland/xkb/fontconfig packages exist for GPUI; alsa-lib for
# cpal playback; openssl-devel for librespot and rspotify (native-tls).

FROM localhost/kuma-dev-gcc

RUN dnf -y install \
        pkgconf-pkg-config \
        git \
        clang \
        fontconfig-devel \
        freetype-devel \
        libxkbcommon-devel \
        libxkbcommon-x11-devel \
        wayland-devel \
        libxcb-devel \
        vulkan-devel \
        alsa-lib-devel \
        openssl-devel \
    && dnf clean all
