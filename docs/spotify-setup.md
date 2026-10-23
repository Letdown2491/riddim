# Spotify setup

Riddim needs Spotify Premium. Setup has two parts: creating your own Spotify
developer app (once), and pairing Riddim's playback device (once, from any
Spotify app you already use).

## The two connections

Riddim talks to Spotify through two separate connections:

1. **Library and search** — the Spotify Web API, under *your* developer app.
   Grants reading your library and playlists and searching the catalog. This
   is the browser approval you see at sign-in.
2. **Playback** — [Soloist](https://developer.spotify.com/documentation/soloist),
   Spotify's official headless player. Riddim starts the Soloist daemon and
   drives it over a local WebSocket on `127.0.0.1`; the daemon owns the
   Spotify Connect session and the audio output. Playback authorizes through
   Spotify Connect pairing, not the browser.

## Create your developer app

The library connection needs a Client ID that belongs to you:

1. Open the [Spotify Developer Dashboard](https://developer.spotify.com/dashboard)
   and sign in. Accept the developer terms if asked.
2. **Create app**: any name and description.
3. In the app's **Settings → Edit**, add the redirect URI:

   ```
   http://127.0.0.1:8888/callback
   ```

   Which API/SDKs are toggled does not matter.
4. Under **Web Playback** (or the app's API settings), generate a **Soloist
   API key**. It starts with `spak_`.
5. Copy the **Client ID** from Basic Information. The Client Secret is never
   needed — Riddim uses PKCE, which works without secrets.

## First launch

1. Start Riddim. The setup window asks for two things; paste both:
   the **Client ID** and the **Soloist API key**.
2. Your browser opens the **library approval** for your developer app.
   Approve it. Spotify redirects to `127.0.0.1:8888/callback`, which Riddim
   catches and closes out.
3. Your library loads. Riddim starts the Soloist daemon in the background;
   it registers itself as a Spotify Connect device named **riddim**.
4. Pair it once: in any Spotify app (phone, desktop, web), start playing a
   track and choose **riddim** in the device picker. Playback then follows
   Riddim's queue.

The pairing is remembered in Riddim's data directory, so later launches
connect without the device-picker step. Soloist API keys are tied to your
account and never expire unless you revoke them.

Later sign-ins reuse stored tokens and open no browser until something
expires or you sign out.

## Where your data lives

- **Client ID** — app preferences, in SQLite under `~/.local/share/riddim`.
- **Soloist API key** — the desktop keyring (Secret Service: GNOME Keyring,
  KWallet, or another `org.freedesktop.se` provider). Riddim never writes
  keys or tokens to plain files, and never sees your password.
- **Library cache** — SQLite, same data directory.
- **Soloist daemon state** — `~/.local/share/riddim/soloist`, holding the
  Spotify Connect session and audio cache. Riddim deletes this directory
  when you sign out.

Signing out inside Riddim removes the API key from the keyring, stops the
daemon, and clears its pairing. To revoke Riddim entirely, use
[Spotify's app management page](https://www.spotify.com/us/account/apps/)
— remove your developer app and revoke its Soloist key in the dashboard.

## Installing the Soloist daemon

Riddim looks for a `soloist` binary on your `PATH`; set `RIDDIM_SOLOIST_BIN`
to point at a specific one. Soloist ships from Spotify's developer site for
x86-64 Linux and Apple Silicon — download the archive for your platform and
put the binary somewhere on `PATH`. Soloist builds expire 90 days after
their build date; an expired daemon refuses to start and Riddim tells you to
update it.

## Troubleshooting

**The browser did not open.** Riddim opens URLs with your desktop's default
browser (`xdg-open`). Check `xdg-settings get default-web-browser`. If your
session has no default, set one, then sign in again.

**Spotify shows "REDIRECT URI mismatch" or "INVALID_CLIENT".** The library
approval only works if `http://127.0.0.1:8888/callback` is listed in your
developer app's redirect URIs, exactly, including the trailing `/callback`.

**"Could not listen for the … callback" at startup.** Port 8888 must be
free. A stuck previous instance usually holds it: `ss -tlnp | grep 8888`
and end that process.

**Login fails with a keyring or "Secret Service" error.** No keyring daemon
is answering on the `org.freedesktop.se` D-Bus name. Start one (GNOME
Keyring: `gnome-keyring-daemon --start --components=secrets`; KDE unlocks
KWallet automatically) and sign in again.

**riddim never appears in the Spotify device picker.** The daemon registers
itself over local network discovery, and its port changes on every start:

- **Firewall:** allow local TCP ports 40000–50000, e.g.
  `sudo firewall-cmd --add-port=40000-50000/tcp --permanent && sudo firewall-cmd --reload`.
  The discovery port is random per daemon start, so a range is required.
- **Same network:** phone-side pairing needs the Spotify app and the daemon
  on a network segment that allows local traffic (no guest-network client
  isolation). Pairing from the desktop Spotify app on the same machine
  always works.
- **Daemon up:** `RIDDIM_SOLOIST_BIN=/path/to/soloist riddim` and check the
  logs for `soloist:` lines; a daemon that exits with code 10 has an expired
  build and needs updating.

**The daemon fails to start with a missing `libatomic.so.1`.** Some Soloist
builds need it on distributions that ship GCC's runtime split out. Install
`libatomic` (Fedora: `sudo dnf install libatomic`) or point
`LD_LIBRARY_PATH` at a copy of the library.

**Approved everything but no sound.** Check that a normal browser or the
desktop Spotify app can play, then look for an ALSA/PipeWire default sink
(`pactl info`). Soloist outputs through ALSA to your sound server, at the
volume Riddim's slider sets.
