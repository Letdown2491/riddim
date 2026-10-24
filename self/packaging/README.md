# self: flatpak packaging + Orchard publishing

Everything flatpak lives here. The app id is `io.github.Letdown2491.riddim`
(reverse-DNS from the GitHub repo).

## Layout

- `io.github.Letdown2491.riddim.yml`: the flatpak manifest (freedesktop
  25.08 runtime, rust-stable sdk extension).
- `io.github.Letdown2491.riddim.desktop`, `*.metainfo.xml`, `*.png`:
  appstream + launcher metadata, installed by the manifest. The scalable
  launcher icon is `assets/riddim-icon.svg`; the committed 512px PNG here
  is its raster (appstream compose needs a raster source) and doubles as
  the Orchard listing icon.
- `io.github.Letdown2491.riddim.publish.toml`: the Orchard listing.
- `description.md`: the listing's long form.
- `tree/`: flatpak-builder's work area (gitignored).

## Dependencies of the build

One-time, on the host (the host has no compiler; the SDK does the build):

```
flatpak install -y --system flathub \
  org.freedesktop.Platform//25.08 \
  org.freedesktop.Sdk//25.08 \
  org.freedesktop.Sdk.Extension.rust-stable//25.08 \
  org.flatpak.Builder
```

Crate sources are vendored at the repo root (`vendor/`, gitignored): the
manifest's build step appends a vendored-sources replacement to
`.cargo/config.toml` and builds `--offline`. Locally, generate it before
building:

```
cargo vendor vendor
```

(In CI the release workflow vendors instead.)

## Build, install, run (local test)

```
flatpak run --command=flatpak-builder org.flatpak.Builder \
  --user --install --force-clean self/tree \
  self/packaging/io.github.Letdown2491.riddim.yml

flatpak run io.github.Letdown2491.riddim
```

Inside the sandbox the app keeps its state under
`~/.var/app/io.github.Letdown2491.riddim/` (flatpak's sandboxed HOME), one
level deeper than a bare run. First launch inside the sandbox deserves a
real pass: the OAuth browser callback on `127.0.0.1:8888`, the keyring
write, MPRIS media keys, and Soloist playback through ALSA are the four
things a bare-metal run exercises differently.

## Permissions (finish-args, why each exists)

- `--socket=wayland` / `--socket=x11`: windowing (wayland primary, X11
  fallback), `--device=dri` for Vulkan rendering.
- `--share=network`: the Spotify Web API, the OAuth callback server on
  `127.0.0.1:8888`, and the Soloist daemon download.
- `--device=alsa`: Soloist plays through ALSA, which PipeWire and
  PulseAudio servers provide.
- `--talk-name=org.freedesktop.secrets`: the desktop keyring holds the
  OAuth token and the Soloist API key; nothing is written to plain files.
- `--own-name=org.mpris.MediaPlayer2.riddim`: MPRIS for media keys and
  Now Playing.
- `--talk-name=org.freedesktop.portal.Desktop`: portal access for desktop
  settings (cursor theme, dark mode).

## Publishing to Orchard

Automated: pushing a tag `v*` runs [.github/workflows/release.yml](../../.github/workflows/release.yml),
which builds the flatpak on the freedesktop 25.08 runtime, attaches the
bundle to the GitHub release, unpacks it into an app tree, and publishes to
Orchard with `orchard-publish` (built from Orchard's pinned v0.1.2): the
app tree goes to `ghcr.io/letdown2491/riddim-flatpak` as an OCI image and
three Nostr events are signed and sent (listing 32267, release 30063, asset
3063 pinning the image digest). Secrets it needs:

- `ORCHARD_NSEC`: the publisher's Nostr key. Never in the repo.
- ghcr pushes use the workflow's `GITHUB_TOKEN` (packages:write); the first
  push creates the package and it must be flipped to **public** once (Orchard
  cannot pull a private image).

`orchard.txt` at the repo root is written by `--prove` on the x86_64
publish leg and names the publisher npub, so Orchard shows the repository
link as verified: keep it in step with the signing key. It needs the
repository public.

Manual (any machine with flatpak + ostree, for re-publishing without a tag):

```
flatpak run --command=flatpak-builder org.flatpak.Builder \
  --force-clean --repo=self/tree/export self/tree/stage \
  self/packaging/io.github.Letdown2491.riddim.yml

# single-file bundle alternative:
flatpak build-bundle self/tree/export \
  riddim-0.1.0.flatpak io.github.Letdown2491.riddim stable
```
