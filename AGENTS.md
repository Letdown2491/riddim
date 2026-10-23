# Agent Instructions

## Build & Test

kumaOS is atomic and has no host toolchain: cargo runs inside the
`localhost/riddim-dev` podman container via `./scripts/build.sh`. The script
builds the image on first use, keeps rustup and the cargo registry in named
volumes for incremental builds, and installs the built binary to
`~/.local/bin/riddim` on `build`. Set `SPOTIFY_CLIENT_ID` on the host to skip
the setup screen at launch.

CI runs all five; run them before committing:

```bash
./scripts/build.sh build
./scripts/build.sh fmt --all --check
./scripts/build.sh clippy --all-targets
./scripts/build.sh test
./scripts/build.sh test --test ui_rendering
```

### Visual regression checks

`cargo test --test ui_rendering` renders whole windows and compares them pixel
by pixel against `tests/visual/baseline`. Any change to a view's layout or
pixels fails it by design, including changes that only alter an element's size.

The baseline is captured in the `riddim-dev` container on Linux (provenance in
`tests/visual/baseline/manifest.json`). Renders are software-rasterized
(lavapipe), so they are repeatable across machines but not identical to
GPU-composited output — judge a change by *where* the pixels differ rather
than by the pass or fail: an unintended layout shift moves controls that the
change should not have touched.

To accept an intended visual change, review the actual renders in
`target/ui-artifacts`, re-record the baseline, update `manifest.json`
provenance, and commit:

```bash
RIDDIM_UI_BASELINE="$PWD/tests/visual/baseline" \
  ./scripts/build.sh test --test ui_rendering -- --record
```

(The env var must reach the test inside the container; pass it through
`podman run -e` when invoking directly.)

## Platform Notes

Riddim is a Linux port of cadence (macOS). Code paths that only exist on macOS
are `cfg`-gated, not deleted:

- `src/app/updater/` — Sparkle on macOS; a no-op stub elsewhere. Linux has no
  in-app update channel yet; distribute via desktop packaging when that comes.
- `src/app/windows.rs` — the macOS-only AppKit window pinning (onboarding
  window locked over main) simply does not run elsewhere.
- Media keys and Now Playing go through `souvlaki`: MPRIS on Linux (zbus),
  MPNowPlayingInfoCenter on macOS.
- Credentials go through `keyring`'s `v1` mode: macOS Keychain, Windows
  credential store, or the Linux Secret Service (zbus), all from one API.

Workspace keybindings follow platform convention: Cmd on macOS, Ctrl
everywhere else. Tests pick the modifier with `primary_modifier()`.

## Naming

The app is riddim. The word "cadence" also means a beat pattern — a few
comments use it in that English sense (for example "refresh cadence"); do not
rename those. The logo and screenshots still carry cadence's mark and
branding; replace them with riddim art when available.

## Non-Interactive Shell Commands

**ALWAYS use non-interactive flags** with file operations to avoid hanging on
confirmation prompts.

Shell commands like `cp`, `mv`, and `rm` may be aliased to include `-i`
(interactive) mode on some systems, causing the agent to hang indefinitely
waiting for y/n input.

**Use these forms instead:**
```bash
# Force overwrite without prompting
cp -f source dest           # NOT: cp source dest
mv -f source dest           # NOT: mv source dest
rm -f file                  # NOT: rm file

# For recursive operations
rm -rf directory            # NOT: rm -r directory
cp -rf source dest          # NOT: cp -rf source dest
```

**Other commands that may prompt:**
- `scp` - use `-o BatchMode=yes` for non-interactive
- `ssh` - use `-o BatchMode=yes` to fail instead of prompting
- `dnf` - use `-y` flag
