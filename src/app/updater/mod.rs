//! In-app updates.
//!
//! On macOS the implementation is Sparkle ([`sparkle`]), driven by the
//! framework that `package-app.sh` embeds in the app bundle. Other platforms
//! have no in-app update channel yet: the module degrades to "this build does
//! not update itself", which hides the update card in settings and makes the
//! check-for-updates action inert.

use super::*;

/// What the updater last reported, watched by the settings page.
#[derive(Default)]
pub(crate) struct UpdateStatus {
    pub(crate) check: UpdateCheck,
}

/// The outcome of the most recent update check, by display version. Only the
/// macOS updater constructs variants; other platforms keep the shape so the
/// settings page renders against one type.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum UpdateCheck {
    #[default]
    Unknown,
    UpToDate,
    Available(String),
    Skipped(String),
}

#[cfg(target_os = "macos")]
pub(crate) use sparkle::*;

#[cfg(target_os = "macos")]
mod sparkle;

/// Starts the updater and reports whether it will drive the update flow.
///
/// Returns `false` on platforms without an update channel, which is not
/// fatal: the app simply goes without in-app updates.
#[cfg(not(target_os = "macos"))]
pub(crate) fn start(_cx: &mut App) -> bool {
    log::info!("startup: no in-app update channel on this platform");
    false
}

/// Runs a user-initiated update check. Inert without an update channel.
#[cfg(not(target_os = "macos"))]
pub(crate) fn check_for_updates(_cx: &mut App) {}

/// What the updater found, or `None` when this build does not update itself.
#[cfg(not(target_os = "macos"))]
pub(crate) fn status(_cx: &App) -> Option<Entity<UpdateStatus>> {
    None
}

/// Whether the updater checks for updates on its own, or `None` when this
/// build does not update itself.
#[cfg(not(target_os = "macos"))]
pub(crate) fn automatic_checks(_cx: &App) -> Option<bool> {
    None
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn set_automatic_checks(_enabled: bool, _cx: &mut App) {}
