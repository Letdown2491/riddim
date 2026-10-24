//! Playback through the Spotify Soloist daemon.
//!
//! Soloist owns the Spotify Connect session and the audio output; riddim
//! supervises the daemon, sends it control commands over its local WebSocket
//! API, and translates its event stream into the [`PlaybackEvent`]s the rest
//! of the app consumes. Setup requires a Soloist API key from the Spotify
//! developer dashboard (stored in the keychain) and a one-time pairing from
//! any Spotify app; no separate playback OAuth flow exists.

use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use keyring::Entry;
use tokio::sync::mpsc;

use crate::{credential_worker, soloist};

const KEYCHAIN_SERVICE: &str = "io.github.letdown2491.riddim";
const KEYCHAIN_ACCOUNT_API_KEY: &str = "soloist-api-key";

/// The control surface the queue machine in `backend.rs` drives.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PlaybackCommand {
    Load {
        spotify_uri: String,
        playing: bool,
        position_ms: u32,
    },
    Play,
    Pause,
    Seek {
        position_ms: u32,
    },
    SetVolume(f32),
    Stop,
}

/// The player events the rest of the app reacts to. Mirrors the librespot
/// event set the backend was built on; Soloist signals are translated.
#[derive(Debug, Clone, PartialEq)]
pub enum PlaybackEvent {
    Loading {
        spotify_uri: String,
    },
    Playing {
        spotify_uri: String,
        position_ms: u32,
    },
    Paused {
        spotify_uri: String,
        position_ms: u32,
    },
    EndOfTrack {
        spotify_uri: String,
    },
    PositionChanged {
        spotify_uri: String,
        position_ms: u32,
    },
}

#[derive(Clone)]
pub struct Playback {
    client: soloist::Soloist,
    commands: mpsc::UnboundedSender<PlaybackCommand>,
    events: std::sync::Arc<std::sync::Mutex<Option<mpsc::UnboundedReceiver<PlaybackEvent>>>>,
}

impl Playback {
    /// Starts the Soloist daemon with the stored API key and connects.
    pub(crate) async fn connect() -> Result<Self> {
        let api_key = load_soloist_api_key().await?;
        let api_key = api_key.ok_or_else(|| {
            anyhow!("no Soloist API key is stored; generate one in the Spotify developer dashboard")
        })?;
        Self::start(&api_key).await
    }

    /// Reconnects from persisted state; identical to connecting.
    pub async fn reconnect() -> Result<Self> {
        Self::connect().await
    }

    pub(crate) async fn start(api_key: &str) -> Result<Self> {
        let (client, soloist_events) = soloist::Soloist::start(api_key).await?;
        let (commands, command_receiver) = mpsc::unbounded_channel();
        let (event_sender, event_receiver) = mpsc::unbounded_channel();
        tokio::spawn(controller(
            client.clone(),
            command_receiver,
            soloist_events,
            event_sender,
        ));
        Ok(Self {
            client,
            commands,
            events: std::sync::Arc::new(std::sync::Mutex::new(Some(event_receiver))),
        })
    }

    /// The event stream. Yields exactly one receiver per connection lifetime.
    pub fn events(&self) -> Option<mpsc::UnboundedReceiver<PlaybackEvent>> {
        self.events.lock().expect("soloist event lock").take()
    }

    pub fn load(&self, spotify_uri: impl Into<String>, playing: bool, position_ms: u32) {
        let _ = self.commands.send(PlaybackCommand::Load {
            spotify_uri: spotify_uri.into(),
            playing,
            position_ms,
        });
    }

    pub fn play(&self) {
        let _ = self.commands.send(PlaybackCommand::Play);
    }

    pub fn pause(&self) {
        let _ = self.commands.send(PlaybackCommand::Pause);
    }

    pub fn seek(&self, position_ms: u32) {
        let _ = self.commands.send(PlaybackCommand::Seek { position_ms });
    }

    /// Volume as a 0..1 fraction; Soloist wants a percentage.
    pub fn set_volume(&self, volume: f32) {
        let _ = self.commands.send(PlaybackCommand::SetVolume(volume));
    }

    pub fn stop(&self) {
        let _ = self.commands.send(PlaybackCommand::Stop);
    }

    /// True while the WebSocket is up and the daemon has a logged-in Spotify
    /// Connect session.
    pub fn is_connected(&self) -> bool {
        self.client.is_connected()
    }
}

/// Translates app-level commands into Soloist commands and Soloist events
/// into [`PlaybackEvent`]s, correlating the two so a track change caused by
/// riddim's own load command is not mistaken for the current track ending.
async fn controller(
    client: soloist::Soloist,
    mut commands: mpsc::UnboundedReceiver<PlaybackCommand>,
    mut soloist_events: mpsc::UnboundedReceiver<soloist::Event>,
    playback_events: mpsc::UnboundedSender<PlaybackEvent>,
) {
    // The last play command riddim issued itself, for correlating the
    // daemon's track-changed events against riddim's own queue moves.
    let mut last_play: Option<(String, Instant)> = None;
    // The track the daemon is currently on, as far as the event stream shows.
    let mut current_uri: Option<String> = None;
    let mut status = soloist::PlaybackStatus::Idle;
    // The last known position anchor, for interpolating between syncs.
    let mut anchor: Option<PositionAnchor> = None;
    let mut position_tick = tokio::time::interval(Duration::from_millis(500));
    position_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            command = commands.recv() => {
                let Some(command) = command else {
                    // Every `Playback` handle is gone; dropping the soloist
                    // client kills the daemon we spawned.
                    return;
                };
                match command {
                    PlaybackCommand::Load { spotify_uri, playing, position_ms } => {
                        last_play = Some((spotify_uri.clone(), Instant::now()));
                        client.command(soloist::Command::Play { uri: Some(soloist_uri(spotify_uri.clone())) });
                        if !playing {
                            client.command(soloist::Command::Pause);
                        }
                        if position_ms > 0 {
                            client.command(soloist::Command::Seek { position_ms });
                        }
                        current_uri = Some(spotify_uri);
                    }
                    PlaybackCommand::Play => client.command(soloist::Command::Play { uri: None }),
                    PlaybackCommand::Pause => client.command(soloist::Command::Pause),
                    PlaybackCommand::Seek { position_ms } => {
                        client.command(soloist::Command::Seek { position_ms });
                        anchor = Some(PositionAnchor::now(position_ms as u64));
                    }
                    PlaybackCommand::SetVolume(volume) => client.command(soloist::Command::SetVolume {
                        volume: (volume.clamp(0., 1.) * 100.).round() as u8,
                    }),
                    // The WebSocket API cannot clear the queue; pausing is the
                    // closest stop, and riddim loads the next track explicitly.
                    PlaybackCommand::Stop => client.command(soloist::Command::Pause),
                }
            }
            event = soloist_events.recv() => {
                let Some(event) = event else { return; };
                match event {
                    soloist::Event::AuthState { .. } => {}
                    soloist::Event::PlaybackState(state) => {
                        if let Some(position) = state.position {
                            anchor = Some(PositionAnchor::from(position));
                        }
                        let item_uri = state
                            .item
                            .map(|item| item.uri)
                            .filter(|uri| !uri.is_empty());
                        match item_uri {
                            Some(uri) if current_uri.as_deref() != Some(uri.as_str()) => {
                                // The engine moved to a different track. When
                                // riddim commanded it, the backend already
                                // knows the old track ended; otherwise it must
                                // be told.
                                if !recent_self_play(&last_play, &uri) {
                                    if let Some(old) = current_uri.take() {
                                        emit(&playback_events, PlaybackEvent::EndOfTrack { spotify_uri: old });
                                    }
                                } else {
                                    current_uri.take();
                                }
                                current_uri = Some(uri.clone());
                            }
                            _ => {}
                        }
                        let Some(uri) = current_uri.clone() else {
                            continue;
                        };
                        match state.status {
                            Some(soloist::PlaybackStatus::Playing) => emit(&playback_events, PlaybackEvent::Playing {
                                position_ms: interpolated(&anchor),
                                spotify_uri: uri,
                            }),
                            Some(soloist::PlaybackStatus::Paused) => emit(&playback_events, PlaybackEvent::Paused {
                                position_ms: interpolated(&anchor),
                                spotify_uri: uri,
                            }),
                            Some(soloist::PlaybackStatus::Buffering) => emit(&playback_events, PlaybackEvent::Loading { spotify_uri: uri }),
                            Some(soloist::PlaybackStatus::Idle) | None => {}
                        }
                    }
                    soloist::Event::TrackChanged { item } => {
                        let uri = item.uri;
                        if uri.is_empty() || current_uri.as_deref() == Some(uri.as_str()) {
                            continue;
                        }
                        if !recent_self_play(&last_play, &uri) {
                            // The engine advanced on its own (queue or
                            // autoplay): the current track just ended.
                            if let Some(old) = current_uri.take() {
                                emit(&playback_events, PlaybackEvent::EndOfTrack { spotify_uri: old });
                            }
                        }
                        current_uri = Some(uri.clone());
                        emit(&playback_events, PlaybackEvent::Loading { spotify_uri: uri });
                    }
                    soloist::Event::PlaybackChanged { status: new } => {
                        status = new;
                        match (new, current_uri.clone()) {
                            (soloist::PlaybackStatus::Playing, Some(uri)) => emit(&playback_events, PlaybackEvent::Playing {
                                position_ms: interpolated(&anchor),
                                spotify_uri: uri,
                            }),
                            (soloist::PlaybackStatus::Paused, Some(uri)) => emit(&playback_events, PlaybackEvent::Paused {
                                position_ms: interpolated(&anchor),
                                spotify_uri: uri,
                            }),
                            (soloist::PlaybackStatus::Buffering, Some(uri)) => emit(&playback_events, PlaybackEvent::Loading { spotify_uri: uri }),
                            // The engine ran out of music: the track ended.
                            (soloist::PlaybackStatus::Idle, Some(uri)) => {
                                current_uri = None;
                                emit(&playback_events, PlaybackEvent::EndOfTrack { spotify_uri: uri });
                            }
                            (_, None) => {}
                        }
                    }
                    soloist::Event::PositionSync { position } => {
                        anchor = Some(PositionAnchor::from(position));
                        if status == soloist::PlaybackStatus::Playing
                            && let Some(uri) = current_uri.clone()
                        {
                            emit(&playback_events, PlaybackEvent::PositionChanged {
                                position_ms: interpolated(&anchor),
                                spotify_uri: uri,
                            });
                        }
                    }
                    // riddim's own volume watch is the source of truth, and
                    // queue/context/options changes drive nothing in the app
                    // yet; command results are advisory, errors are logged.
                    soloist::Event::VolumeChanged { .. }
                    | soloist::Event::DeviceChanged { .. }
                    | soloist::Event::ContextChanged { .. }
                    | soloist::Event::OptionsChanged { .. }
                    | soloist::Event::QueueChanged { .. }
                    | soloist::Event::CommandResult { .. } => {}
                    soloist::Event::Error { message } => log::warn!("soloist: {message}"),
                }
            }
            _ = position_tick.tick(), if status == soloist::PlaybackStatus::Playing => {
                if let Some(uri) = current_uri.clone() {
                    emit(&playback_events, PlaybackEvent::PositionChanged {
                        position_ms: interpolated(&anchor),
                        spotify_uri: uri,
                    });
                }
            }
        }
    }
}
fn recent_self_play(last_play: &Option<(String, Instant)>, uri: &str) -> bool {
    last_play
        .as_ref()
        .is_some_and(|(played, at)| played == uri && at.elapsed() < Duration::from_secs(30))
}

/// Soloist URIs must be plain `spotify:` URIs; the app stores them as such.
fn soloist_uri(spotify_uri: String) -> String {
    spotify_uri
}

/// The last known position anchor, for interpolating between syncs.
#[derive(Clone, Copy)]
struct PositionAnchor {
    position_ms: u64,
    at: std::time::SystemTime,
    speed: f64,
}

impl PositionAnchor {
    fn from(position: soloist::Position) -> Self {
        Self {
            position_ms: position.position_ms,
            at: std::time::SystemTime::UNIX_EPOCH + Duration::from_millis(position.timestamp_ms),
            speed: position.speed,
        }
    }

    fn now(position_ms: u64) -> Self {
        Self {
            position_ms,
            at: std::time::SystemTime::now(),
            speed: 1.,
        }
    }

    /// The position now, extrapolated from the anchor.
    fn interpolated(&self) -> u32 {
        let elapsed = self.at.elapsed().unwrap_or_default().as_millis() as f64 * self.speed;
        ((self.position_ms as f64) + elapsed).max(0.) as u32
    }
}

fn interpolated(anchor: &Option<PositionAnchor>) -> u32 {
    anchor.map_or(0, |a| a.interpolated())
}

fn emit(events: &mpsc::UnboundedSender<PlaybackEvent>, event: PlaybackEvent) {
    let _ = events.send(event);
}

// ---------------------------------------------------------------------------
// Keychain storage for the Soloist API key.

fn api_key_entry() -> Result<Entry> {
    Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT_API_KEY).map_err(Into::into)
}

pub(crate) async fn save_soloist_api_key(api_key: String) -> Result<()> {
    let trimmed = api_key.trim().to_owned();
    if trimmed.is_empty() {
        return Err(anyhow!("the Soloist API key is empty"));
    }
    credential_worker::run(move || api_key_entry()?.set_password(&trimmed).map_err(Into::into))
        .await
}

pub(crate) async fn load_soloist_api_key() -> Result<Option<String>> {
    credential_worker::run(|| match api_key_entry()?.get_password() {
        Ok(key) if key.is_empty() => Ok(None),
        Ok(key) => Ok(Some(key)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(error) => Err(anyhow!(error)),
    })
    .await
}

pub(crate) async fn delete_soloist_api_key() -> Result<()> {
    credential_worker::run(|| match api_key_entry()?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(anyhow!(error)),
    })
    .await
}
