# Native visual checks

Run inside the container, with the same rendering environment as the baseline:

```sh
./scripts/build.sh test --test ui_rendering
```

The runner renders production views twice per case and requires identical RGBA pixels. Failures write expected, actual and difference images to `target/ui-artifacts`. Diagnostic PNGs use lossless compression and retain every pixel.

The 48 full-window cases cover library and queue states, search text and focus, empty and filled setup inputs, autoplay on and off, and the closed and open mascot selector. Each runs in light and dark appearances at 1280 × 820 and 900 × 820 points. Queue cases cover closed and open states with idle, hover and pointer-down input.

Eight additional cases cover the focused queue button, closed and open, in both appearances and layouts. These capture the 40-point control with four points of surrounding space. Keyboard focus adds a two-point circular border using `focus_ring`, preserving the icon, geometry and selected background. They do not replace any full-window reference.

The references in `baseline` are gpui-kit 0.7.0 captures from the `riddim-dev` podman container (Fedora 44, Vulkan headless via lavapipe), rendered with bundled Inter 3.19, reduced motion, active windows, a deterministic clock, local artwork placeholders and paused playback. All 56 repeat exactly. The manifest records the source revision, capture environment, lockfile hash and font hashes.

Software rasterization (lavapipe) is deterministic across machines, which is why the baseline lives in the container: GPU-composited output on a real desktop can differ in anti-aliasing detail without any layout change. Compare against the baseline as rendered, not as seen on screen.

To record references in a separate directory from an approved source revision:

```sh
RIDDIM_UI_BASELINE=/tmp/riddim-reference ./scripts/build.sh test --test ui_rendering -- --record
```

Review the images before replacing `tests/visual/baseline/*.png`, update `manifest.json` with the recorded provenance, then commit the baseline through the usual review process. Approved images stay in the repository. Regeneration does not automatically approve a visual change.
