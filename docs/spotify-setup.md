# Spotify setup

Riddim needs Spotify Premium. Setup has two parts: creating your own Spotify
developer app (once), and approving Riddim in the browser (every sign-in).

## Why two approvals

Riddim talks to Spotify through two separate connections, each authorized on
its own:

1. **Library and search** — the Spotify Web API, under *your* developer app.
   Grants reading your library and playlists and searching the catalog.
2. **Playback** — the Spotify audio protocol, spoken by librespot under the
   hood. Playback runs under Riddim's built-in player identity, not your
   developer app, so this approval asks for the `streaming` scope only.

The app opens your browser twice on first sign-in: first for your library,
then for playback. On each page, choose **Agree** (or **Cancel** to abort).
The URLs look like:

```
https://accounts.spotify.com/authorize?response_type=code&client_id=…&state=…&code_challenge=…&code_challenge_method=S256&redirect_uri=http%3A%2F%2F127.0.0.1%3A8888%2Fcallback&scope=…
```

`client_id`, `redirect_uri`, and `scope` are fixed per connection; the
`state` and `code_challenge` values are fresh random tokens every login, so
your URL will not look exactly like anyone else's. Both redirects point at
`127.0.0.1` — a tiny local server Riddim runs only while it waits for
Spotify to send you back. Nothing is served to the network.

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
4. Copy the **Client ID** from Basic Information. The Client Secret is never
   needed — Riddim uses PKCE, which works without secrets.

Playback needs no dashboard setup: its approval page runs under Riddim's
built-in player identity.

## First launch

1. Start Riddim. The sign-in window asks for your Client ID; paste it.
2. Your browser opens the **library approval** for your developer app.
   Approve it. Spotify redirects to `127.0.0.1:8888/callback`, which Riddim
   catches and closes out.
3. The browser opens a second time for **playback** (scope `streaming`).
   Approve it; the redirect goes to `127.0.0.1:8898/login`.
4. Riddim connects and your library loads.

Later sign-ins reuse stored tokens and open no browser until something
expires or you sign out.

## Where your data lives

- **Client ID** — app preferences, in SQLite under `~/.local/share/riddim`.
- **Login and playback tokens** — the desktop keyring (Secret Service:
  GNOME Keyring, KWallet, or another `org.freedesktop.se` provider). Riddim
  never writes tokens to plain files, and never sees your password.
- **Library cache** — SQLite, same data directory.

Signing out inside Riddim removes the tokens from the keyring. To revoke
Riddim entirely, use [Spotify's app management page](https://www.spotify.com/us/account/apps/)
— remove both your developer app and the built-in player entry.

## Troubleshooting

**The browser did not open.** Riddim opens URLs with your desktop's default
browser (`xdg-open`). Check `xdg-settings get default-web-browser`. If your
session has no default, set one, then sign in again.

**Spotify shows "REDIRECT URI mismatch" or "INVALID_CLIENT".** The library
approval only works if `http://127.0.0.1:8888/callback` is listed in your
developer app's redirect URIs, exactly, including the trailing `/callback`.

**"Could not listen for the … callback" at startup.** Ports 8888 and 8898
must be free. A stuck previous instance usually holds them: `ss -tlnp | grep
-E '8888|8898'` and end that process.

**Login fails with a keyring or "Secret Service" error.** No keyring daemon
is answering on the `org.freedesktop.se` D-Bus name. Start one (GNOME
Keyring: `gnome-keyring-daemon --start --components=secrets`; KDE unlocks
KWallet automatically) and sign in again.

**Approved everything but no sound.** Authorization and playback are
separate problems: check that a normal browser or `spotifyd` can play, then
look for an ALSA/PipeWire default sink (`pactl info`). Playback goes through
ALSA to your sound server.
