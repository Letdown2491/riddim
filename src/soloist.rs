//! Supervisor and local WebSocket client for the Spotify Soloist daemon.
//!
//! Playback runs in Spotify's official headless client, [Soloist], instead of
//! an embedded librespot session: riddim spawns the `soloist` binary, drives
//! it through its local WebSocket API, and observes its event stream. The
//! daemon owns the Spotify Connect session (paired once from any Spotify
//! app) and the audio output; riddim is a remote control plus UI.
//!
//! [Soloist]: https://developer.spotify.com/documentation/soloist

use std::{path::PathBuf, process::Stdio, time::Duration};

use anyhow::{Context as _, Result, anyhow};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use tokio::{
    process::{Child, Command as ProcessCommand},
    sync::{mpsc, watch},
};
use tokio_tungstenite::{connect_async, tungstenite::Message};

/// Soloist builds expire 90 days after their build date and exit with this
/// code (per the official docs); a dead daemon must not be respawned forever.
const EXPIRED_BUILD_EXIT_CODE: i32 = 10;
/// How long to wait for the daemon to publish its WebSocket endpoint.
const ENDPOINT_TIMEOUT: Duration = Duration::from_secs(15);
/// Backoff bounds between WebSocket reconnect attempts.
const RECONNECT_MIN: Duration = Duration::from_millis(500);
const RECONNECT_MAX: Duration = Duration::from_secs(30);

/// A running Soloist daemon: either spawned by this process (owned, killed on
/// drop) or adopted from a previous run that died without cleaning up.
struct Daemon {
    child: Option<Child>,
    data_dir: PathBuf,
    api_key: String,
}

impl Daemon {
    /// Starts (or adopts) the daemon and waits for its WebSocket endpoint.
    async fn start(api_key: &str) -> Result<Self> {
        let data_dir = crate::storage::data_dir()?.join("soloist");
        std::fs::create_dir_all(&data_dir)
            .context("could not create the Soloist data directory")?;
        let mut daemon = Self {
            child: None,
            data_dir,
            api_key: api_key.to_owned(),
        };
        if daemon.endpoint_files().is_some() {
            log::info!("soloist: adopting already-running daemon");
            return Ok(daemon);
        }
        daemon.spawn().await?;
        daemon.wait_for_endpoint().await?;
        Ok(daemon)
    }

    async fn spawn(&mut self) -> Result<()> {
        let binary = std::env::var("RIDDIM_SOLOIST_BIN").unwrap_or_else(|_| "soloist".to_owned());
        let child = ProcessCommand::new(&binary)
            .arg("--device-name")
            .arg("riddim")
            .arg("--api-key")
            .arg(&self.api_key)
            // Port 0 lets the OS pick a free port; the daemon writes the real
            // endpoint into the data directory for discovery.
            .arg("--ws")
            .arg("127.0.0.1:0")
            .arg("--data-dir")
            .arg(&self.data_dir)
            .arg("--cache-dir")
            .arg(self.data_dir.join("cache"))
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| match error.kind() {
                std::io::ErrorKind::NotFound => anyhow!(
                    "the Spotify Soloist daemon ({binary}) is not installed; see \
                     https://developer.spotify.com/documentation/soloist"
                ),
                _ => anyhow!("could not start the Spotify Soloist daemon: {error}"),
            })?;
        log::info!("soloist: spawned daemon (pid {:?})", child.id());
        self.child = Some(child);
        Ok(())
    }

    /// Reads `ws.addr` and `ws.port`, which the daemon writes once its
    /// WebSocket server is up and removes on shutdown.
    fn endpoint_files(&self) -> Option<(String, u16)> {
        let addr = std::fs::read_to_string(self.data_dir.join("ws.addr")).ok()?;
        let port = std::fs::read_to_string(self.data_dir.join("ws.port")).ok()?;
        Some((addr.trim().to_owned(), port.trim().parse().ok()?))
    }

    async fn wait_for_endpoint(&mut self) -> Result<()> {
        let deadline = tokio::time::Instant::now() + ENDPOINT_TIMEOUT;
        loop {
            if self.endpoint_files().is_some() {
                return Ok(());
            }
            if let Some(status) = self.try_wait_child().await? {
                return Err(self.exit_error(status));
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(anyhow!(
                    "the Spotify Soloist daemon did not report a WebSocket endpoint"
                ));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Non-blocking child exit check; `None` while still running or adopted.
    async fn try_wait_child(&mut self) -> Result<Option<std::process::ExitStatus>> {
        match self.child.as_mut() {
            Some(child) => Ok(child.try_wait()?),
            None => Ok(None),
        }
    }

    fn exit_error(&self, status: std::process::ExitStatus) -> anyhow::Error {
        let code = status.code();
        if code == Some(EXPIRED_BUILD_EXIT_CODE) {
            return anyhow!(
                "the Spotify Soloist build has expired; install a newer one from \
                 https://developer.spotify.com/documentation/soloist"
            );
        }
        anyhow!("the Spotify Soloist daemon exited unexpectedly ({status})")
    }

    /// Connects to the daemon's WebSocket API, respawning the daemon first if
    /// it died since the last connection.
    async fn connect(&mut self) -> Result<tokio_tungstenite::WebSocketStream<MaybeTlsStream>> {
        if let Some(status) = self.try_wait_child().await? {
            log::warn!("soloist: daemon died ({status}); respawning");
            self.child = None;
            // A stale endpoint pair would point at the dead daemon.
            let _ = std::fs::remove_file(self.data_dir.join("ws.addr"));
            let _ = std::fs::remove_file(self.data_dir.join("ws.port"));
            self.spawn().await?;
            self.wait_for_endpoint().await?;
        } else if self.child.is_none() && self.endpoint_files().is_none() {
            // Nothing to adopt and nothing alive: start from scratch.
            self.spawn().await?;
            self.wait_for_endpoint().await?;
        }
        let (addr, port) = self
            .endpoint_files()
            .context("the Spotify Soloist daemon is not publishing a WebSocket endpoint")?;
        let (stream, _) = connect_async(format!("ws://{addr}:{port}"))
            .await
            .with_context(|| {
                format!("could not reach the Soloist WebSocket API at {addr}:{port}")
            })?;
        Ok(stream)
    }

    /// Stops the daemon. Nothing in the WebSocket API logs out or shuts down,
    /// so the process is the only lever.
    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
        }
        // An adopted daemon cannot be killed as a child; it keeps running and
        // the next launch adopts it again.
    }

    /// Tears down all daemon state so the next [`Self::connect`] spawns fresh.
    /// A daemon killed without cleanup leaves its endpoint files behind, and
    /// connecting against them fails forever unless they are removed.
    async fn reset(&mut self) {
        self.stop();
        let _ = std::fs::remove_file(self.data_dir.join("ws.addr"));
        let _ = std::fs::remove_file(self.data_dir.join("ws.port"));
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

type MaybeTlsStream = tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>;

/// A control connection to the Soloist daemon: send [`Command`]s, receive
/// [`Event`]s, and watch liveness. Clones share one connection.
#[derive(Clone)]
pub struct Soloist {
    commands: mpsc::UnboundedSender<Command>,
    connected: watch::Receiver<bool>,
}

impl Soloist {
    /// Starts the daemon and its connection task, returning the event stream.
    /// The task lives until every `Soloist` handle is dropped, then kills the
    /// daemon.
    pub async fn start(api_key: &str) -> Result<(Self, mpsc::UnboundedReceiver<Event>)> {
        let daemon = Daemon::start(api_key).await?;
        let (commands, command_receiver) = mpsc::unbounded_channel();
        let (events, event_receiver) = mpsc::unbounded_channel();
        let (connected, connected_receiver) = watch::channel(false);
        tokio::spawn(connection_loop(daemon, command_receiver, events, connected));
        Ok((
            Self {
                commands,
                connected: connected_receiver,
            },
            event_receiver,
        ))
    }

    /// True while the WebSocket to the daemon is up, whether or not the
    /// Spotify Connect session is paired yet.
    pub fn is_connected(&self) -> bool {
        *self.connected.borrow()
    }

    pub fn command(&self, command: Command) {
        // A failed send means the connection task is gone; the watch already
        // reports the connection as lost.
        let _ = self.commands.send(command);
    }
}

impl Drop for Soloist {
    fn drop(&mut self) {
        // Dropping the command channel ends the connection loop, whose
        // Daemon drop kills the spawned process.
        let _ = self.commands;
    }
}

async fn connection_loop(
    mut daemon: Daemon,
    mut commands: mpsc::UnboundedReceiver<Command>,
    events: mpsc::UnboundedSender<Event>,
    connected: watch::Sender<bool>,
) {
    let mut backoff = RECONNECT_MIN;
    loop {
        match daemon.connect().await {
            Ok(stream) => {
                log::info!("soloist: WebSocket connected");
                backoff = RECONNECT_MIN;
                let (mut sink, mut stream) = futures::StreamExt::split(stream);
                let closed = loop {
                    tokio::select! {
                        command = commands.recv() => {
                            let Some(command) = command else {
                                // Every handle is gone: shut the daemon down.
                                break ConnectionEnd::Shutdown;
                            };
                            if let Err(error) = sink.send(Message::text(command.to_message())).await {
                                log::warn!("soloist: command send failed: {error}");
                                break ConnectionEnd::Lost;
                            }
                        }
                        message = stream.next() => {
                            let Some(message) = message else {
                                break ConnectionEnd::Lost;
                            };
                            match message {
                                Ok(Message::Text(text)) => {
                                    if let Some(event) = parse_event(&text)
                                        && events.send(event).is_err()
                                    {
                                        // No listeners left; keep serving commands.
                                    }
                                }
                                Ok(Message::Close(_)) => break ConnectionEnd::Lost,
                                Ok(_) => {}
                                Err(error) => {
                                    log::warn!("soloist: WebSocket error: {error}");
                                    break ConnectionEnd::Lost;
                                }
                            }
                        }
                        _ = tokio::time::sleep(Duration::from_secs(20)) => {
                            // Liveness probe: the API has no ping of its own;
                            // a state query fails when the socket is dead.
                            if sink.send(Message::text(Command::GetState.to_message())).await.is_err() {
                                break ConnectionEnd::Lost;
                            }
                        }
                    }
                    // Live means the daemon is reachable and paired accounts
                    // can stream; pairing state is reported separately via
                    // auth_state events. An unpaired-but-running daemon must
                    // not trip the backend's respawn logic.
                    let _ = connected.send(true);
                };
                let _ = connected.send(false);
                match closed {
                    ConnectionEnd::Shutdown => {
                        daemon.stop();
                        return;
                    }
                    ConnectionEnd::Lost => {}
                }
            }
            Err(error) => {
                log::warn!("soloist: {error:#}");
                // An expired build must not be respawned; surface it and stop.
                if error.to_string().contains("expired") {
                    let _ = events.send(Event::Error {
                        message: error.to_string(),
                    });
                    let _ = connected.send(false);
                    return;
                }
                // Whatever answered (or not) at that endpoint is not usable:
                // clear the way for a fresh spawn on the next attempt.
                daemon.reset().await;
            }
        }
        log::info!("soloist: reconnecting in {backoff:?}");
        tokio::select! {
            command = commands.recv() => {
                if command.is_none() {
                    daemon.stop();
                    return;
                }
                // Commands arriving during the outage are dropped; the
                // backend's is_connected watch keeps it from relying on them.
                continue;
            }
            _ = tokio::time::sleep(backoff) => {
                backoff = (backoff * 2).min(RECONNECT_MAX);
            }
        }
    }
}

enum ConnectionEnd {
    Shutdown,
    Lost,
}

/// A control message for the daemon. Serialized per the WebSocket API's
/// `{ "type": "command", "command": … }` envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Play { uri: Option<String> },
    Pause,
    SkipNext,
    SkipPrev,
    Seek { position_ms: u32 },
    SetVolume { volume: u8 },
    AddToQueue { uri: String },
    Activate,
    GetState,
    GetQueue { limit: Option<u32> },
}

impl Command {
    fn to_message(&self) -> String {
        let mut message = json!({ "type": "command" });
        let object = message.as_object_mut().expect("fresh json object");
        match self {
            Self::Play { uri } => {
                object.insert("command".into(), "play".into());
                if let Some(uri) = uri {
                    object.insert("uri".into(), uri.clone().into());
                }
            }
            Self::Pause => {
                object.insert("command".into(), "pause".into());
            }
            Self::SkipNext => {
                object.insert("command".into(), "skip_next".into());
            }
            Self::SkipPrev => {
                object.insert("command".into(), "skip_prev".into());
            }
            Self::Seek { position_ms } => {
                object.insert("command".into(), "seek".into());
                object.insert("position_ms".into(), (*position_ms).into());
            }
            Self::SetVolume { volume } => {
                object.insert("command".into(), "set_volume".into());
                object.insert("volume".into(), (*volume).into());
            }
            Self::AddToQueue { uri } => {
                object.insert("command".into(), "add_to_queue".into());
                object.insert("uri".into(), uri.clone().into());
            }
            Self::Activate => {
                object.insert("command".into(), "activate".into());
            }
            Self::GetState => {
                object.insert("command".into(), "get_state".into());
            }
            Self::GetQueue { limit } => {
                object.insert("command".into(), "get_queue".into());
                if let Some(limit) = limit {
                    object.insert("limit".into(), (*limit).into());
                }
            }
        }
        message.to_string()
    }
}

/// An entity in Soloist's common envelope: a playable item plus optional
/// decorations. Only what riddim renders is parsed; the rest is ignored.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Entity {
    #[serde(default)]
    pub uri: String,
    #[serde(default)]
    pub entity_type: String,
    #[serde(default)]
    pub decorations: Decorations,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct Decorations {
    #[serde(default)]
    pub identity: Option<Identity>,
    #[serde(default)]
    pub playback: Option<PlaybackDecorations>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Identity {
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct PlaybackDecorations {
    #[serde(default)]
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum PlaybackStatus {
    Idle,
    Playing,
    Paused,
    Buffering,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq)]
pub struct Position {
    pub position_ms: u64,
    pub timestamp_ms: u64,
    pub speed: f64,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct PlaybackState {
    #[serde(default)]
    pub status: Option<PlaybackStatus>,
    #[serde(default)]
    pub item: Option<Entity>,
    #[serde(default)]
    pub position: Option<Position>,
    #[serde(default)]
    pub volume: Option<u8>,
    #[serde(default)]
    pub is_active: bool,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct QueueEntry {
    #[serde(default)]
    pub uid: String,
    pub item: Entity,
}

/// One server-to-client message. Built dynamically because the API grows
/// event types over time: unknown types are dropped rather than fatal.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    AuthState {
        logged_in: bool,
        is_active: bool,
        device_name: Option<String>,
    },
    PlaybackState(PlaybackState),
    TrackChanged {
        item: Entity,
    },
    PlaybackChanged {
        status: PlaybackStatus,
    },
    VolumeChanged {
        volume: u8,
    },
    DeviceChanged {
        is_active: bool,
        device_name: Option<String>,
    },
    ContextChanged {
        context: Entity,
    },
    OptionsChanged {
        options: serde_json::Value,
    },
    PositionSync {
        position: Position,
    },
    QueueChanged {
        previous: Vec<QueueEntry>,
        upcoming: Vec<QueueEntry>,
    },
    CommandResult {
        command: String,
    },
    Error {
        message: String,
    },
}

fn parse_event(text: &str) -> Option<Event> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let kind = value.get("type")?.as_str()?;
    let event = match kind {
        "auth_state" => Event::AuthState {
            logged_in: value.get("logged_in")?.as_bool()?,
            is_active: value.get("is_active").and_then(bool_ok).unwrap_or(false),
            device_name: value
                .get("device_name")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
        },
        "playback_state" => Event::PlaybackState(serde_json::from_value(value).ok()?),
        "track_changed" => Event::TrackChanged {
            item: serde_json::from_value(value.get("item").cloned().unwrap_or_default()).ok()?,
        },
        "playback_changed" => Event::PlaybackChanged {
            status: serde_json::from_value(value.get("status").cloned().unwrap_or_default())
                .ok()?,
        },
        "volume_changed" => Event::VolumeChanged {
            volume: value.get("volume")?.as_u64()? as u8,
        },
        "device_changed" => Event::DeviceChanged {
            is_active: value.get("is_active").and_then(bool_ok).unwrap_or(false),
            device_name: value
                .get("device_name")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
        },
        "context_changed" => Event::ContextChanged {
            context: serde_json::from_value(value.get("context").cloned().unwrap_or_default())
                .ok()?,
        },
        "options_changed" => Event::OptionsChanged {
            options: value.get("options").cloned().unwrap_or_default(),
        },
        "position_sync" => Event::PositionSync {
            position: serde_json::from_value(value.get("position").cloned().unwrap_or_default())
                .ok()?,
        },
        "queue_changed" => Event::QueueChanged {
            previous: value
                .get("previous")
                .map(parse_queue_entries)
                .unwrap_or_default(),
            upcoming: value
                .get("upcoming")
                .map(parse_queue_entries)
                .unwrap_or_default(),
        },
        "command_result" => Event::CommandResult {
            command: value
                .get("command")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        },
        "error" => Event::Error {
            message: value
                .get("message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown Soloist error")
                .to_owned(),
        },
        // Forward compatibility: ignore events riddim does not know yet.
        _ => {
            log::debug!("soloist: ignoring unknown event type {kind}");
            return None;
        }
    };
    Some(event)
}

fn parse_queue_entries(value: &serde_json::Value) -> Vec<QueueEntry> {
    value
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| serde_json::from_value(entry.clone()).ok())
                .collect()
        })
        .unwrap_or_default()
}

fn bool_ok(value: &serde_json::Value) -> Option<bool> {
    value.as_bool()
}

/// The path of the Soloist binary, for diagnostics and tests.
pub fn binary_hint() -> String {
    std::env::var("RIDDIM_SOLOIST_BIN").unwrap_or_else(|_| "soloist".to_owned())
}

/// Removes the daemon's data directory, ending its stored Spotify Connect
/// session. Call while the daemon is stopped (sign-out drops it first).
pub fn clear_daemon_data() -> Result<()> {
    let dir = crate::storage::data_dir()?.join("soloist");
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => Ok(()),
        // Nothing stored yet is as good as cleared.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(anyhow::Error::new(error).context("could not clear the Soloist data directory"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_serialize_to_the_documented_envelope() {
        assert_eq!(
            Command::Pause.to_message(),
            r#"{"type":"command","command":"pause"}"#
        );
        assert_eq!(
            Command::Seek {
                position_ms: 30_000
            }
            .to_message(),
            r#"{"type":"command","command":"seek","position_ms":30000}"#
        );
        assert_eq!(
            Command::Play {
                uri: Some("spotify:track:6rqhFgbbKwnb9MLmUQDhG6".into())
            }
            .to_message(),
            r#"{"type":"command","command":"play","uri":"spotify:track:6rqhFgbbKwnb9MLmUQDhG6"}"#
        );
        assert_eq!(
            Command::SetVolume { volume: 42 }.to_message(),
            r#"{"type":"command","command":"set_volume","volume":42}"#
        );
        assert_eq!(
            Command::GetQueue { limit: Some(10) }.to_message(),
            r#"{"type":"command","command":"get_queue","limit":10}"#
        );
    }

    #[test]
    fn documented_event_samples_parse() {
        let auth = parse_event(
            r#"{"type":"auth_state","logged_in":true,"is_active":true,"device_name":"Kitchen speaker"}"#,
        )
        .unwrap();
        assert!(matches!(
            auth,
            Event::AuthState {
                logged_in: true,
                is_active: true,
                device_name: Some(_)
            }
        ));

        let state = parse_event(
            r#"{"type":"playback_state","status":"playing",
                "item":{"uri":"spotify:track:2JRo0gjbX4GrCqBYdRohoo","entity_type":"track",
                        "decorations":{"identity":{"name":"My Song"},"playback":{"duration_ms":210000}}},
                "position":{"position_ms":45000,"timestamp_ms":1747654321000,"speed":1.0},
                "volume":65,"is_active":true,
                "options":{"shuffle":false,"repeat":"off"}}"#,
        )
        .unwrap();
        let Event::PlaybackState(state) = state else {
            panic!("expected playback state");
        };
        assert_eq!(state.status, Some(PlaybackStatus::Playing));
        assert_eq!(
            state
                .item
                .unwrap()
                .decorations
                .playback
                .unwrap()
                .duration_ms,
            Some(210000)
        );
        assert_eq!(state.volume, Some(65));

        let position = parse_event(
            r#"{"type":"position_sync","position":{"position_ms":45000,"timestamp_ms":1747654321000,"speed":1.0}}"#,
        )
        .unwrap();
        assert!(matches!(position, Event::PositionSync { .. }));

        let queue = parse_event(
            r#"{"type":"queue_changed","previous":[],"upcoming":[{"uid":"spotify:track:upcoming","source":"queue","item":{"uri":"spotify:track:upcoming","entity_type":"track","decorations":{}}}]}"#,
        )
        .unwrap();
        assert!(matches!(
            queue,
            Event::QueueChanged {
                previous: _,
                upcoming: _
            }
        ));

        let error =
            parse_event(r#"{"type":"error","message":"command requires authentication"}"#).unwrap();
        assert!(matches!(error, Event::Error { message } if message.contains("authentication")));
    }

    #[test]
    fn unknown_event_types_are_dropped_not_fatal() {
        assert!(parse_event(r#"{"type":"brand_new_event","x":1}"#).is_none());
        assert!(parse_event("not json at all").is_none());
    }

    #[test]
    fn entity_uri_defaults_when_missing() {
        let entity: Entity = serde_json::from_value(json!({
            "entity_type": "ad",
            "decorations": {}
        }))
        .unwrap();
        assert_eq!(entity.uri, "");
        assert_eq!(entity.decorations.identity, None);
    }
}
