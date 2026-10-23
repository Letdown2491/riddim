# Agent Instructions

## Build & Test

CI runs all five; run them before committing:

```bash
cargo build
cargo fmt --all --check
cargo clippy --all-targets
cargo test
cargo test --test ui_rendering
```

### Visual regression checks

`cargo test --test ui_rendering` renders whole windows and compares them pixel by
pixel against `tests/visual/baseline`. Any change to a view's layout or pixels
fails it by design, including changes that only alter an element's size.

The baseline is captured on the CI platform, currently macOS 15. Native
rasterization varies between macOS versions, so running it on a newer machine
reports differences that are not regressions. Judge a change by *where* the
pixels differ rather than by the pass or fail: an unintended layout shift moves
controls that the change should not have touched.

To accept an intended visual change, capture on the CI platform rather than
locally: run **Actions → Regenerate screenshots** on the branch, download the
artifact, review the images, copy them over `tests/visual/baseline/*.png`, update
`manifest.json` provenance, then commit. `tests/visual/README.md` has the detail.

## Releases

Pushing a `v*` tag triggers `.github/workflows/release.yml`, which builds the app and runs `gh release create` for that tag. Do NOT create the GitHub release manually (`gh release create` or the web UI) after pushing a tag; the release already exists by the time CI finishes publishing, so the workflow fails with "a release with the same tag name already exists".

To cut a release: bump the version, commit, tag `vX.Y.Z`, push the tag, and let CI publish. Versions must be plain `MAJOR.MINOR.PATCH`: packaging derives Sparkle's numeric `CFBundleVersion` from it and refuses pre-release suffixes.

The workflow also signs the DMG with the Sparkle key from the `SPARKLE_PRIVATE_KEY` secret and publishes `appcast.xml` as a release asset. Installed copies read `https://github.com/infomiho/cadence/releases/latest/download/appcast.xml` and update themselves through the embedded Sparkle framework. `scripts/setup-sparkle-key.sh` creates the key, stores the secret and writes the public key into `assets/Info.plist`. Back the private key up: without it no installed copy accepts an update. The tag message becomes the release notes shown in the update window.

### Release notes format

Put the full notes in the annotated tag message **before** pushing the tag: the workflow copies the tag message into both the GitHub release body (`release.yml`) and the Sparkle `appcast.xml` (`generate-appcast.sh`). `gh release edit` only fixes the GitHub page, never the update window. Check the voice with `gh release view` on a previous release.

- Heading `Cadence X.Y.Z`, then bullets, then `**Full Changelog**: <compare URL>` for the previous tag.
- One bolded lead-in per user-visible change: `**Music no longer stays paused after dictation.**` Then the behavior, not the code: what the user experienced before, what happens now. No module names, no function names, no jargon.
- Internal fixes with no user-visible behavior still get a user-facing sentence; keep it to one line.

## Non-Interactive Shell Commands

**ALWAYS use non-interactive flags** with file operations to avoid hanging on confirmation prompts.

Shell commands like `cp`, `mv`, and `rm` may be aliased to include `-i` (interactive) mode on some systems, causing the agent to hang indefinitely waiting for y/n input.

**Use these forms instead:**
```bash
# Force overwrite without prompting
cp -f source dest           # NOT: cp source dest
mv -f source dest           # NOT: mv source dest
rm -f file                  # NOT: rm file

# For recursive operations
rm -rf directory            # NOT: rm -r directory
cp -rf source dest          # NOT: cp -r source dest
```

**Other commands that may prompt:**
- `scp` - use `-o BatchMode=yes` for non-interactive
- `ssh` - use `-o BatchMode=yes` to fail instead of prompting
- `apt-get` - use `-y` flag
- `brew` - use `HOMEBREW_NO_AUTO_UPDATE=1` env var

