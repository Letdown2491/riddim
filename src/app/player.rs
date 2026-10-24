use super::*;
use std::time::Instant;

/// How far playback may drift from the saved position before it is written back.
const POSITION_SAVE_INTERVAL_MS: u32 = 5_000;

/// How far a reported track end may sit before the extrapolated stopped
/// position and still count as the track running out. An engine session torn
/// down by a Connect transfer reports Idle mid-track instead.
const END_OF_TRACK_SLACK_MS: u32 = 3_000;

/// Whether a track the engine stopped playing had actually run out at
/// `position_ms`, as opposed to being aborted mid-track by a takeover.
/// No live track counts as reached, so a stalled advance cannot park playback.
fn track_reached_end(track: Option<&model::Track>, position_ms: u32) -> bool {
    track.is_none_or(|track| position_ms + END_OF_TRACK_SLACK_MS >= track.duration_ms)
}

/// Reported when a playback command could not be delivered to the backend.
pub(super) struct PlaybackUnavailable;

impl EventEmitter<PlaybackUnavailable> for Player {}

/// Playback state that belongs to the process rather than to a window.
pub(super) struct Player {
    backend: BackendHandle,
    now_playing: Option<model::Track>,
    context: Arc<[model::Track]>,
    queue: Arc<[model::Track]>,
    playing: bool,
    loading: bool,
    /// The Spotify context URI riddim handed to the daemon as a whole
    /// (a playlist), while the daemon is still progressing through it.
    /// Drives the playlists pane's play/pause buttons.
    context_uri: Option<String>,
    /// Position and play state to reapply once a reconnected player is ready.
    restore: Option<(u32, bool)>,
    /// The current track still owes a history entry once it plays.
    history_pending: bool,
    clock: playback_clock::PlaybackClock,
    saved_position_ms: u32,
    volume: f32,
    volume_before_mute: f32,
    volume_dragging: bool,
    error: Option<SharedString>,
}

impl Player {
    pub(super) fn new(backend: BackendHandle, now: Instant) -> Self {
        Self {
            backend,
            now_playing: None,
            context: Arc::default(),
            queue: Arc::default(),
            playing: false,
            loading: false,
            context_uri: None,
            restore: None,
            history_pending: false,
            clock: playback_clock::PlaybackClock::new(now),
            saved_position_ms: 0,
            volume: 0.72,
            volume_before_mute: 0.72,
            volume_dragging: false,
            error: None,
        }
    }

    pub(super) fn now_playing(&self) -> Option<&model::Track> {
        self.now_playing.as_ref()
    }

    pub(super) fn context(&self) -> &Arc<[model::Track]> {
        &self.context
    }

    pub(super) fn queue(&self) -> &Arc<[model::Track]> {
        &self.queue
    }

    pub(super) fn playing(&self) -> bool {
        self.playing
    }

    pub(super) fn loading(&self) -> bool {
        self.loading
    }

    /// Where playback has reached, extrapolated between backend reports.
    pub(super) fn position_ms(&self, now: Instant) -> u32 {
        self.clock.position_ms(now)
    }

    /// The last confirmed or requested position, without extrapolation.
    pub(super) fn anchor_ms(&self) -> u32 {
        self.clock.anchor_ms()
    }

    /// Moves play state and re-anchors the clock together, so the extrapolated
    /// position cannot keep running after playback stops.
    fn set_playback(&mut self, playing: bool, cx: &App) {
        self.playing = playing;
        self.clock
            .set_playing(playing, cx.background_executor().now());
    }

    pub(super) fn volume(&self) -> f32 {
        self.volume
    }

    pub(super) fn volume_dragging(&self) -> bool {
        self.volume_dragging
    }

    pub(super) fn error(&self) -> Option<&SharedString> {
        self.error.as_ref()
    }

    pub(super) fn is_current_track(&self, track: &model::Track) -> bool {
        self.now_playing.as_ref().is_some_and(|playing| {
            playing.provider == track.provider && playing.source_id == track.source_id
        })
    }

    /// True while the daemon is progressing through `spotify_uri` as a whole
    /// context riddim handed it (paused or playing).
    pub(super) fn is_context(&self, spotify_uri: &str) -> bool {
        self.context_uri.as_deref() == Some(spotify_uri)
    }

    /// [`Self::is_context`], and the context is actively playing.
    pub(super) fn is_playing_context(&self, spotify_uri: &str) -> bool {
        self.is_context(spotify_uri) && self.playing
    }

    fn live_track_matches(&self, spotify_uri: &str) -> bool {
        self.now_playing
            .as_ref()
            .and_then(|track| track.spotify_uri.as_deref())
            == Some(spotify_uri)
    }

    /// History records what actually played, not what was loaded: librespot
    /// only confirms a track opened once it reports Playing.
    fn record_first_play(&mut self) {
        if !self.history_pending {
            return;
        }
        if let Some(track) = self.now_playing.clone() {
            self.backend.send(BackendCommand::MarkPlayed(track));
        }
        self.history_pending = false;
    }

    /// Playback commands are dropped while a restore is in flight so they cannot
    /// race the position the backend is about to reapply.
    fn send(&self, command: BackendCommand, cx: &mut Context<Self>) -> bool {
        if self.restore.is_some() {
            return false;
        }
        self.deliver(command, cx)
    }

    /// Sends `command` and reports the failure when the backend cannot take it,
    /// so a dead or saturated worker does not leave controls silently inert.
    fn deliver(&self, command: BackendCommand, cx: &mut Context<Self>) -> bool {
        if self.backend.send(command) {
            return true;
        }
        cx.emit(PlaybackUnavailable);
        false
    }

    pub(super) fn toggle(&mut self, cx: &mut Context<Self>) {
        if self.now_playing.is_none() {
            return;
        }
        let playing = !self.playing;
        if self.send(
            if self.playing {
                BackendCommand::Pause
            } else {
                BackendCommand::Resume
            },
            cx,
        ) {
            self.set_playback(playing, cx);
            self.loading = playing;
        }
        cx.notify();
    }

    /// Moves to `playing`, doing nothing if playback is already in that state.
    pub(super) fn set_playing(&mut self, playing: bool, cx: &mut Context<Self>) {
        if self.playing != playing {
            self.toggle(cx);
        }
    }

    pub(super) fn next(&mut self, cx: &mut Context<Self>) {
        if self.now_playing.is_some() && self.send(BackendCommand::Next, cx) {
            self.loading = true;
        }
        cx.notify();
    }

    pub(super) fn previous(&mut self, cx: &mut Context<Self>) {
        if self.now_playing.is_some() && self.send(BackendCommand::Previous, cx) {
            self.loading = true;
        }
        cx.notify();
    }

    pub(super) fn seek(&mut self, position_ms: u32, cx: &mut Context<Self>) {
        if self.send(BackendCommand::Seek(position_ms), cx) {
            self.clock.seek(position_ms, cx.background_executor().now());
        } else if self.restore.is_some() {
            // A reconnect drops commands silently. Left unreported, the scrub
            // the listener just finished would simply spring back.
            cx.emit(PlaybackUnavailable);
        }
        cx.notify();
    }

    pub(super) fn play_context(
        &mut self,
        tracks: Vec<model::Track>,
        index: usize,
        cx: &mut Context<Self>,
    ) -> bool {
        let started = self.send(BackendCommand::PlayContext { tracks, index }, cx);
        if started {
            self.context_uri = None;
            self.clock.reset(0, cx.background_executor().now());
            self.set_playback(false, cx);
            self.loading = true;
        }
        cx.notify();
        started
    }

    /// Plays a whole Spotify context (playlist, album) by URI when its track
    /// list is unavailable: the daemon resolves and drives it, and riddim
    /// adopts whatever the daemon reports as now playing.
    pub(super) fn play_spotify_context(
        &mut self,
        spotify_uri: String,
        cx: &mut Context<Self>,
    ) -> bool {
        let started = self.send(
            BackendCommand::PlaySpotifyContext {
                spotify_uri: spotify_uri.clone(),
            },
            cx,
        );
        if started {
            self.context_uri = Some(spotify_uri);
            self.now_playing = None;
            self.clock.reset(0, cx.background_executor().now());
            self.set_playback(false, cx);
            self.loading = true;
            self.history_pending = false;
        }
        cx.notify();
        started
    }

    pub(super) fn play_next(&mut self, track: model::Track, cx: &mut Context<Self>) -> bool {
        self.deliver(BackendCommand::PlayNext(track), cx)
    }

    pub(super) fn append_to_queue(&mut self, track: model::Track, cx: &mut Context<Self>) -> bool {
        self.deliver(BackendCommand::AppendToQueue(track), cx)
    }

    pub(super) fn start_radio(
        &mut self,
        request_id: u64,
        seed: model::Track,
        cx: &mut Context<Self>,
    ) -> bool {
        self.deliver(BackendCommand::StartRadio { request_id, seed }, cx)
    }

    pub(super) fn set_loading(&mut self, loading: bool, cx: &mut Context<Self>) {
        self.loading = loading;
        cx.notify();
    }

    pub(super) fn toggle_mute(&mut self, cx: &mut Context<Self>) {
        if self.volume > 0. {
            self.volume_before_mute = self.volume;
            self.volume = 0.;
        } else {
            self.volume = self.volume_before_mute.max(0.2);
        }
        self.deliver(BackendCommand::SetVolume(self.volume), cx);
        cx.notify();
    }

    pub(super) fn begin_volume_drag(
        &mut self,
        pointer_x: Pixels,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        self.volume_dragging = true;
        self.drag_volume(pointer_x, window, cx);
    }

    pub(super) fn drag_volume(
        &mut self,
        pointer_x: Pixels,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let window_width = window.window_bounds().get_bounds().size.width;
        self.volume = volume_for_pointer(pointer_x, window_width, window.rem_size());
        if self.volume > 0. {
            self.volume_before_mute = self.volume;
        }
        self.deliver(BackendCommand::SetVolume(self.volume), cx);
        cx.notify();
    }

    pub(super) fn end_volume_drag(&mut self, cx: &mut Context<Self>) {
        if self.volume_dragging {
            self.volume_dragging = false;
            cx.notify();
        }
    }

    /// Writes the live position back so a restart resumes where the listener left off.
    /// The position to persist for the current track, if one is playing.
    pub(super) fn position_snapshot(&self) -> Option<u32> {
        self.now_playing
            .as_ref()
            .and_then(|track| track.spotify_uri.as_ref())
            .map(|_| self.clock.anchor_ms())
    }

    pub(super) fn save_position(&self) {
        if let Some(spotify_uri) = self
            .now_playing
            .as_ref()
            .and_then(|track| track.spotify_uri.clone())
        {
            self.backend.send(BackendCommand::SavePlaybackPosition {
                spotify_uri,
                position_ms: self.clock.anchor_ms(),
            });
        }
    }

    fn save_position_if_moved(&mut self, spotify_uri: String, position_ms: u32) {
        if position_ms.abs_diff(self.saved_position_ms) < POSITION_SAVE_INTERVAL_MS {
            return;
        }
        self.backend.send(BackendCommand::SavePlaybackPosition {
            spotify_uri,
            position_ms,
        });
        self.saved_position_ms = position_ms;
    }

    fn adopt_context(&mut self, current: model::Track, next: Vec<model::Track>) {
        self.context = std::iter::once(current.clone())
            .chain(next.iter().cloned())
            .collect::<Vec<_>>()
            .into();
        self.now_playing = Some(current);
        self.queue = next.into();
    }

    pub(super) fn clear(&mut self, cx: &mut Context<Self>) {
        self.now_playing = None;
        self.context = Arc::default();
        self.queue = Arc::default();
        self.context_uri = None;
        self.set_playback(false, cx);
        self.loading = false;
        self.restore = None;
        self.history_pending = false;
        self.clock.reset(0, cx.background_executor().now());
        self.saved_position_ms = 0;
        self.error = None;
        cx.notify();
    }

    /// Applies the playback half of a backend event, returning the event when the
    /// surrounding app still has its own work to do for it.
    pub(super) fn handle_backend_event(
        &mut self,
        event: BackendEvent,
        cx: &mut Context<Self>,
    ) -> Option<BackendEvent> {
        match event {
            BackendEvent::PlaybackReady => {
                self.error = None;
                self.backend.send(BackendCommand::SetVolume(self.volume));
            }
            BackendEvent::PlaybackReconnecting => {
                if self.restore.is_none() && self.now_playing.is_some() {
                    self.restore = Some((self.clock.anchor_ms(), self.playing));
                }
                self.loading = true;
            }
            BackendEvent::PlaybackReconnected => {
                self.error = None;
                self.backend.send(BackendCommand::SetVolume(self.volume));
                if let Some((position_ms, playing)) = self.restore {
                    self.backend.send(BackendCommand::RestorePlayback {
                        position_ms,
                        playing,
                    });
                } else {
                    self.loading = false;
                }
            }
            BackendEvent::PlaybackRestored {
                position_ms,
                playing,
            } => {
                self.clock
                    .reset(position_ms, cx.background_executor().now());
                self.saved_position_ms = position_ms;
                self.set_playback(playing, cx);
                self.loading = false;
                self.restore = None;
            }
            BackendEvent::PlaybackSettled => {
                self.loading = false;
                self.restore = None;
            }
            BackendEvent::QueueEnded => {
                self.set_playback(false, cx);
                self.loading = false;
                // A full bar with an armed Play button would lie: Play
                // starts this track over (or from wherever the seeker goes).
                self.clock.reset(0, cx.background_executor().now());
                self.saved_position_ms = 0;
                // The daemon-owned context ran out of music.
                self.context_uri = None;
            }
            BackendEvent::Playing { spotify_uri } => {
                if self.restore.is_none() && self.live_track_matches(&spotify_uri) {
                    self.set_playback(true, cx);
                    self.loading = false;
                    self.record_first_play();
                }
            }
            BackendEvent::Loading { spotify_uri } => {
                if self.restore.is_none() && self.live_track_matches(&spotify_uri) {
                    self.loading = true;
                }
            }
            BackendEvent::Paused { spotify_uri } => {
                if self.restore.is_none() && self.live_track_matches(&spotify_uri) {
                    self.set_playback(false, cx);
                    self.loading = false;
                    if self.clock.anchor_ms() != self.saved_position_ms {
                        let position_ms = self.clock.anchor_ms();
                        self.backend.send(BackendCommand::SavePlaybackPosition {
                            spotify_uri,
                            position_ms,
                        });
                        self.saved_position_ms = position_ms;
                    }
                }
            }
            BackendEvent::EndOfTrack { spotify_uri } => {
                if self.restore.is_none() && self.live_track_matches(&spotify_uri) {
                    self.set_playback(false, cx);
                    self.loading = false;
                    // The engine also reports Idle when a Connect transfer
                    // tears its session down mid-track; that is a takeover,
                    // not a finished track, and advancing here would
                    // steamroll the transferred session right back. The
                    // re-anchored clock says where playback audibly stopped
                    // (pauses included), so only a track that actually ran
                    // out moves the queue on.
                    if track_reached_end(self.now_playing.as_ref(), self.clock.anchor_ms()) {
                        self.backend.send(BackendCommand::Next);
                    }
                }
            }
            BackendEvent::PositionChanged {
                spotify_uri,
                position_ms,
            } => {
                if self.restore.is_none() && self.live_track_matches(&spotify_uri) {
                    self.clock
                        .report(position_ms, cx.background_executor().now());
                    self.save_position_if_moved(spotify_uri, position_ms);
                }
            }
            BackendEvent::PlaybackSnapshotLoaded {
                current,
                next,
                position_ms,
            } => {
                self.adopt_context(current, next);
                self.context_uri = None;
                self.clock
                    .reset(position_ms, cx.background_executor().now());
                self.saved_position_ms = position_ms;
                self.set_playback(false, cx);
                self.loading = false;
                self.history_pending = true;
            }
            BackendEvent::PlaybackContext { current, next } => {
                let changed = self.now_playing.as_ref().is_none_or(|track| {
                    track.provider != current.provider || track.source_id != current.source_id
                });
                self.adopt_context(current, next);
                if changed {
                    self.loading = true;
                    self.history_pending = true;
                    self.clock.reset(0, cx.background_executor().now());
                    self.saved_position_ms = 0;
                    self.restore = None;
                }
            }
            BackendEvent::PlaybackFailed(error) => {
                self.error = Some(error.into());
            }
            BackendEvent::TrackFailed { spotify_uri, error } => {
                // librespot parks on a track it cannot open rather than
                // skipping it, so move on the way a finished track does and
                // keep the queue the backend still holds.
                if self.restore.is_none() && self.live_track_matches(&spotify_uri) {
                    self.set_playback(false, cx);
                    self.loading = false;
                    self.backend.send(BackendCommand::Next);
                }
                cx.notify();
                return Some(BackendEvent::TrackFailed { spotify_uri, error });
            }
            event => return Some(event),
        }
        cx.notify();
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(duration_ms: u32) -> model::Track {
        model::Track {
            provider: model::Provider::Spotify,
            source_id: "3USOTQdPtZlgVurwaqsdI5".into(),
            spotify_uri: Some("spotify:track:3USOTQdPtZlgVurwaqsdI5".into()),
            isrc: None,
            title: "Fuck Her Gently".into(),
            artist: "Tenacious D".into(),
            artists: Vec::new(),
            album: "Tenacious D".into(),
            album_ref: None,
            duration_ms,
            artwork_url: None,
            kind: model::MediaKind::default(),
            release_date: None,
        }
    }

    #[test]
    fn a_track_stopped_far_from_its_end_is_an_aborted_takeover() {
        // The Fuck Her Gently transfer: 8 seconds into a 2:04 track, the
        // engine's session was torn down by the phone taking over.
        assert!(!track_reached_end(Some(&track(124_000)), 8_000));
    }

    #[test]
    fn a_track_stopped_at_its_end_advances() {
        assert!(track_reached_end(Some(&track(124_000)), 124_000));
        assert!(track_reached_end(Some(&track(124_000)), 123_000));
    }

    #[test]
    fn a_stale_position_past_the_end_still_counts_as_ended() {
        // The position is extrapolated while the engine plays; an engine that
        // stops syncing before the end reports late.
        assert!(track_reached_end(Some(&track(124_000)), 140_000));
    }

    #[test]
    fn without_a_live_track_the_advance_cannot_park() {
        assert!(track_reached_end(None, 0));
    }
}
