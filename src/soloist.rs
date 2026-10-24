//! Supervisor and local WebSocket client for the Spotify Soloist daemon.
//!
//! Playback runs in Spotify's official headless client, [Soloist], instead of
//! an embedded librespot session: riddim spawns the `soloist` binary, drives
//! it through its local WebSocket API, and observes its event stream. The
//! daemon owns the Spotify Connect session (paired once from any Spotify
//! app) and the audio output; riddim is a remote control plus UI.
//!
//! [Soloist]: https://developer.spotify.com/documentation/soloist

use std::{
    io::Write as _,
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant, SystemTime},
};

use anyhow::{Context as _, Result, anyhow};
use flate2::read::GzDecoder;
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use tar::Archive;
use tokio::{
    process::{Child, Command as ProcessCommand},
    sync::{mpsc, watch},
};
use tokio_tungstenite::{connect_async, tungstenite::Message};

/// Soloist builds expire 90 days after their build date and exit with this
/// code (per the official docs); a dead daemon must not be respawned forever.
const EXPIRED_BUILD_EXIT_CODE: i32 = 10;
const EXPIRED_BUILD_MESSAGE: &str = "the Spotify Soloist build has expired";
/// How long to wait for the daemon to publish its WebSocket endpoint.
const ENDPOINT_TIMEOUT: Duration = Duration::from_secs(15);
/// Backoff bounds between WebSocket reconnect attempts.
const RECONNECT_MIN: Duration = Duration::from_millis(500);
const RECONNECT_MAX: Duration = Duration::from_secs(30);
/// Where Spotify publishes Soloist release archives.
const DOWNLOAD_BASE: &str = "https://soloist-builds.spotifycdn.com";
/// Managed builds are proactively refreshed well inside the 90-day expiry so
/// playback never dies of old age at runtime.
const REFRESH_AFTER: Duration = Duration::from_secs(60 * 24 * 60 * 60);
/// Minimum spacing between reactive refresh attempts, so a network outage on
/// the reconnect backoff does not hammer Spotify's download server.
const REFRESH_RETRY_INTERVAL: Duration = Duration::from_secs(5 * 60);
/// Whole-download budget for fetching a release archive.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);

/// A running Soloist daemon: either spawned by this process (owned, killed on
/// drop) or adopted from a previous run that died without cleaning up.
struct Daemon {
    child: Option<Child>,
    data_dir: PathBuf,
    api_key: String,
    binary_path: PathBuf,
    /// True when `binary_path` is riddim's managed copy, which can be
    /// replaced with a fresh download when it expires.
    managed_binary: bool,
    last_refresh: Option<Instant>,
}

impl Daemon {
    /// Starts (or adopts) the daemon and waits for its WebSocket endpoint.
    async fn start(api_key: &str) -> Result<Self> {
        let data_dir = crate::storage::data_dir()?.join("soloist");
        std::fs::create_dir_all(&data_dir)
            .context("could not create the Soloist data directory")?;
        // Resolving the binary may download one on first run; do it before
        // anything else so a missing daemon fails with a clear message.
        let binary_path = resolve_binary().await?;
        let managed_binary = managed_binary_path().is_ok_and(|managed| managed == binary_path);
        let mut daemon = Self {
            child: None,
            data_dir,
            api_key: api_key.to_owned(),
            binary_path,
            managed_binary,
            last_refresh: None,
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
        let child = ProcessCommand::new(&self.binary_path)
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
                    "the Spotify Soloist daemon ({}) does not exist or is not executable",
                    self.binary_path.display()
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
        if status.code() == Some(EXPIRED_BUILD_EXIT_CODE) {
            return anyhow!("{EXPIRED_BUILD_MESSAGE}");
        }
        anyhow!("the Spotify Soloist daemon exited unexpectedly ({status})")
    }

    /// Reacts to the daemon dying of build expiry by fetching a fresh managed
    /// build. Rate-limited so repeated attempts ride the reconnect backoff
    /// without hammering Spotify's download server.
    async fn refresh_after_expiry(&mut self) -> ExpiryOutcome {
        if !self.managed_binary {
            return ExpiryOutcome::Unrecoverable;
        }
        if self
            .last_refresh
            .is_some_and(|at| at.elapsed() < REFRESH_RETRY_INTERVAL)
        {
            return ExpiryOutcome::Waiting;
        }
        self.last_refresh = Some(Instant::now());
        match tokio::time::timeout(DOWNLOAD_TIMEOUT, refresh_managed(&self.binary_path)).await {
            Ok(Ok(())) => {
                log::info!("soloist: installed a fresh daemon build");
                ExpiryOutcome::Refreshed
            }
            Ok(Err(error)) => {
                log::warn!("soloist: could not refresh the expired daemon: {error:#}");
                ExpiryOutcome::Waiting
            }
            Err(_) => {
                log::warn!("soloist: daemon refresh timed out");
                ExpiryOutcome::Waiting
            }
        }
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
                if error.to_string().contains(EXPIRED_BUILD_MESSAGE) {
                    // An expired build is recoverable while riddim manages
                    // the binary: fetch a fresh one and come right back.
                    match daemon.refresh_after_expiry().await {
                        ExpiryOutcome::Refreshed => {
                            daemon.reset().await;
                            continue;
                        }
                        ExpiryOutcome::Waiting => daemon.reset().await,
                        ExpiryOutcome::Unrecoverable => {
                            let _ = events.send(Event::Error {
                                message: error.to_string(),
                            });
                            let _ = connected.send(false);
                            return;
                        }
                    }
                } else {
                    // Whatever answered (or not) at that endpoint is not
                    // usable: clear the way for a fresh spawn next attempt.
                    daemon.reset().await;
                }
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

/// What happened when an expired build was detected.
enum ExpiryOutcome {
    /// A fresh build was installed: reconnect immediately.
    Refreshed,
    /// The refresh could not run just now (rate limit or network): back off
    /// and try again on a later cycle.
    Waiting,
    /// The binary is not riddim-managed and cannot be self-updated.
    Unrecoverable,
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

// ---------------------------------------------------------------------------
// Managed daemon binary: riddim downloads and updates Soloist itself, so
// users never install one. Spotify's builds expire 90 days after their build
// date, so the managed copy is refreshed on age and on the expiry exit code.

/// The managed daemon binary. Lives beside, not inside, the daemon's data
/// directory: signing out resets the Connect session, not the program.
fn managed_binary_path() -> Result<PathBuf> {
    Ok(crate::storage::data_dir()?.join("bin").join("soloist"))
}

/// Which release archive Spotify publishes for this platform.
fn archive_name(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("linux", "x86_64") => Some("soloist_release_x86_64.tar.gz"),
        ("linux", "aarch64") => Some("soloist_release_arm64.tar.gz"),
        ("linux", "arm") => Some("soloist_release_arm32.tar.gz"),
        ("macos", "aarch64") => Some("soloist_release_osx_arm64.tar.gz"),
        _ => None,
    }
}

/// True when a managed binary last written before `modified` is old enough
/// to be worth proactively refreshing.
fn binary_is_stale(modified: Option<SystemTime>, now: SystemTime) -> bool {
    let Some(modified) = modified else {
        return true;
    };
    now.duration_since(modified)
        .is_ok_and(|age| age > REFRESH_AFTER)
}

fn binary_on_path() -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join("soloist"))
        .find(|candidate| candidate.is_file())
}

/// Decides which daemon binary to run: an explicit `RIDDIM_SOLOIST_BIN`
/// override wins, then riddim's managed copy (refreshed when stale), then
/// whatever is on `PATH`; downloads when nothing usable is present.
async fn resolve_binary() -> Result<PathBuf> {
    if let Some(override_path) = std::env::var_os("RIDDIM_SOLOIST_BIN") {
        return Ok(PathBuf::from(override_path));
    }
    let managed = managed_binary_path()?;
    if managed.is_file() {
        let modified = std::fs::metadata(&managed)
            .and_then(|metadata| metadata.modified())
            .ok();
        if binary_is_stale(modified, SystemTime::now()) {
            match tokio::time::timeout(DOWNLOAD_TIMEOUT, refresh_managed(&managed)).await {
                Ok(Ok(())) => return Ok(managed),
                Ok(Err(error)) => {
                    // Offline or the download server is unreachable: the
                    // existing build may still have lifetime left; the
                    // expiry exit code is the backstop.
                    log::warn!("soloist: could not refresh the managed daemon: {error:#}");
                }
                Err(_) => log::warn!("soloist: daemon refresh timed out"),
            }
        }
        return Ok(managed);
    }
    match tokio::time::timeout(DOWNLOAD_TIMEOUT, refresh_managed(&managed)).await {
        Ok(Ok(())) => Ok(managed),
        Ok(Err(error)) => binary_fallback(error).await,
        Err(_) => binary_fallback(anyhow!("the download timed out")).await,
    }
}

/// A fresh download was not possible: run a daemon the user installed
/// themselves, or fail with instructions.
async fn binary_fallback(error: anyhow::Error) -> Result<PathBuf> {
    if let Some(on_path) = binary_on_path() {
        log::warn!("soloist: managed download failed ({error:#}); using the daemon on PATH");
        return Ok(on_path);
    }
    Err(error.context(
        "could not install the Spotify Soloist daemon; see \
         https://developer.spotify.com/documentation/soloist or set RIDDIM_SOLOIST_BIN",
    ))
}

/// Downloads the current Soloist release and installs it at `managed`,
/// atomically: the archive streams to a sibling temporary file, the binary is
/// extracted to a sibling, smoke-checked, and only then renamed into place.
async fn refresh_managed(managed: &Path) -> Result<()> {
    let archive_name = archive_name(std::env::consts::OS, std::env::consts::ARCH)
        .context("Spotify publishes no Soloist build for this platform")?;
    let url = format!("{DOWNLOAD_BASE}/{archive_name}");
    log::info!("soloist: downloading the daemon from {url}");
    let response = reqwest::Client::new()
        .get(url)
        .send()
        .await
        .context("could not reach Spotify's Soloist download server")?;
    let response = response
        .error_for_status()
        .context("Spotify's Soloist download server reported an error")?;

    let bin_dir = managed
        .parent()
        .context("the managed binary has no parent directory")?;
    std::fs::create_dir_all(bin_dir)?;
    let archive_path = bin_dir.join("soloist.download");
    {
        let mut archive_file =
            std::fs::File::create(&archive_path).context("could not write the Soloist download")?;
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            archive_file.write_all(&chunk.context("the Soloist download failed mid-transfer")?)?;
        }
    }

    let staged = match extract_binary(&archive_path, managed) {
        Ok(staged) => staged,
        Err(error) => {
            let _ = std::fs::remove_file(&archive_path);
            return Err(error);
        }
    };
    let _ = std::fs::remove_file(&archive_path);
    smoke_check(&staged).await?;
    std::fs::rename(&staged, managed).context("could not install the downloaded daemon")?;
    Ok(())
}

/// Extracts the `soloist` entry from the release archive to a staging path
/// next to `dest`, runnable. Entries are top-level; anything else in the
/// archive (notices, licenses) is ignored.
fn extract_binary(archive_path: &Path, dest: &Path) -> Result<PathBuf> {
    let staging = dest.with_file_name("soloist.staging");
    let _ = std::fs::remove_file(&staging);
    let archive_file = std::fs::File::open(archive_path)
        .context("could not read the downloaded Soloist archive")?;
    let mut archive = Archive::new(GzDecoder::new(archive_file));
    let mut extracted = false;
    for entry in archive
        .entries()
        .context("the Soloist archive could not be unpacked")?
    {
        let mut entry = entry.context("the Soloist archive could not be unpacked")?;
        if entry
            .path()
            .context("the Soloist archive has unreadable entry names")?
            .file_name()
            .is_some_and(|name| name == "soloist")
        {
            entry
                .unpack(&staging)
                .context("could not extract the Soloist daemon")?;
            make_executable(&staging)?;
            extracted = true;
            break;
        }
    }
    if !extracted {
        return Err(anyhow!(
            "the Soloist archive did not contain a daemon binary"
        ));
    }
    Ok(staging)
}

/// Runs the staged binary's version probe: proves it executes on this system
/// (a missing runtime such as libatomic fails here with a clear message)
/// before it replaces the current build.
async fn smoke_check(staged: &Path) -> Result<()> {
    let probe = tokio::time::timeout(
        Duration::from_secs(10),
        ProcessCommand::new(staged).arg("--version").output(),
    )
    .await
    .context("the downloaded Soloist daemon did not answer --version")?;
    let output = probe.map_err(|error| {
        anyhow!(
            "the downloaded Soloist daemon failed to run ({error}); a system \
             runtime such as libatomic may be missing"
        )
    })?;
    if !output.status.success() {
        return Err(anyhow!(
            "the downloaded Soloist daemon exited with {:?} on --version",
            output.status
        ));
    }
    Ok(())
}

fn make_executable(path: &Path) -> Result<()> {
    let mut permissions = std::fs::metadata(path)?.permissions();
    permissions.set_mode(permissions.mode() | 0o755);
    std::fs::set_permissions(path, permissions)?;
    Ok(())
}

/// Describes which daemon binary riddim would run, for diagnostics.
pub fn binary_hint() -> String {
    if let Some(override_path) = std::env::var_os("RIDDIM_SOLOIST_BIN") {
        return format!("override at {}", override_path.to_string_lossy());
    }
    match managed_binary_path() {
        Ok(managed) if managed.is_file() => format!("managed at {}", managed.display()),
        Ok(_) => "not managed yet; a PATH copy would be used".to_owned(),
        Err(_) => "data directory unavailable".to_owned(),
    }
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

    #[test]
    fn release_archives_map_to_supported_platforms() {
        assert_eq!(
            archive_name("linux", "x86_64"),
            Some("soloist_release_x86_64.tar.gz")
        );
        assert_eq!(
            archive_name("linux", "aarch64"),
            Some("soloist_release_arm64.tar.gz")
        );
        assert_eq!(
            archive_name("macos", "aarch64"),
            Some("soloist_release_osx_arm64.tar.gz")
        );
        assert_eq!(archive_name("linux", "wasm32"), None);
        assert_eq!(archive_name("windows", "x86_64"), None);
    }

    #[test]
    fn managed_binaries_go_stale_after_the_refresh_window() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        assert!(binary_is_stale(None, now));
        assert!(!binary_is_stale(Some(now - Duration::from_secs(10)), now));
        // The window sits well inside the 90-day build expiry.
        assert!(!binary_is_stale(
            Some(now - REFRESH_AFTER + Duration::from_secs(1)),
            now
        ));
        assert!(binary_is_stale(
            Some(now - REFRESH_AFTER - Duration::from_secs(1)),
            now
        ));
    }

    #[test]
    fn the_daemon_is_extracted_runnable_from_the_release_archive() {
        // A stand-in release archive: the `soloist` binary plus a notice file
        // that must be ignored.
        let mut archive_buffer = Vec::new();
        {
            let encoder =
                flate2::write::GzEncoder::new(&mut archive_buffer, flate2::Compression::default());
            let mut builder = tar::Builder::new(encoder);
            let payload = b"#!/bin/sh\necho soloist 0.0.0\n";
            let mut header = tar::Header::new_gnu();
            header.set_size(payload.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, "soloist", payload.as_slice())
                .unwrap();
            let mut header = tar::Header::new_gnu();
            header.set_size(4);
            header.set_cksum();
            builder
                .append_data(&mut header, "NOTICE", &b"SPOT"[..])
                .unwrap();
            builder.into_inner().unwrap().finish().unwrap();
        }
        let dir = std::env::temp_dir().join(format!("riddim-soloist-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let archive_path = dir.join("release.tar.gz");
        std::fs::write(&archive_path, &archive_buffer).unwrap();
        let dest = dir.join("bin").join("soloist");
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();

        let staged = extract_binary(&archive_path, &dest).unwrap();
        assert_eq!(staged, dest.with_file_name("soloist.staging"));
        assert!(!staged.exists() || std::fs::metadata(&staged).is_ok());
        let contents = std::fs::read(&staged).unwrap();
        assert_eq!(contents, b"#!/bin/sh\necho soloist 0.0.0\n");
        // The archive shipped a non-executable mode; the staged copy must be
        // made runnable before it is installed.
        use std::os::unix::fs::PermissionsExt as _;
        assert_ne!(
            std::fs::metadata(&staged).unwrap().permissions().mode() & 0o111,
            0
        );
        // The notice file did not leak into the install directory.
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2); // release.tar.gz, bin/
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Live check against Spotify's download server; run explicitly with
    /// `cargo test -- --ignored soloist_downloads_and_installs`.
    #[tokio::test]
    #[ignore = "downloads Spotify's release archive"]
    async fn soloist_downloads_and_installs() {
        let dir = std::env::temp_dir().join(format!("riddim-soloist-live-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let managed = dir.join("soloist");
        refresh_managed(&managed).await.unwrap();
        assert!(managed.is_file());
        let version = tokio::process::Command::new(&managed)
            .arg("--version")
            .output()
            .await
            .unwrap();
        assert!(version.status.success());
        println!("installed: {}", String::from_utf8_lossy(&version.stdout));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
