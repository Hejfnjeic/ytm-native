# TODO

## Requested

### Lyrics (synced)
- [x] Per-word/syllable synced lyrics when available (karaoke sweep), else per line, else plain.
      Sources: BetterLyrics + lrc.red (TTML), LRCLIB (LRC); own HTTP client (no YouTube cookies).
- [x] Lyrics tab follows playback; click a line to seek.
- [x] lrc.red translations as an optional second line ("Translation" toggle in the Lyrics header,
      remembered; English preferred; transliterations ignored).
- ~~Romanization~~: not planned.
- ~~Cache lyrics on disk~~: not wanted.
- [x] "Wrong lyrics?" picker: every version found (deduplicated), "No lyrics", search by hand
      (title/artist; any length, labelled). Choice is kept in memory only (no disk cache, by request).
- [ ] Reference: **spicy-lyrics** (https://github.com/Spikerko/spicy-lyrics).
      What I found reading it (commit 1c02f47, 2026-11-14):
  - Three lyric types: `Syllable` (per-syllable/character timing), `Line`, `Static`. The same
    fallback order we want. Word timing comes from TTML (Apple-Music-style), parsed in
    `src/utils/Lyrics/ttml/parser.ts`. Extras worth copying as *ideas*: Japanese/Korean
    romanization, background vocals, duet sides, smooth scroll-to-active-line.
  - The **data** comes from their own server, `api.spicylyrics.org/query`, keyed by **Spotify
    track ID** and authenticated with the user's **Spotify access token**. Sources behind it
    include Apple Music lyrics via lrc.red and community-made lyrics.
    So we can't just call it: we'd need a Spotify track match (title/artist/duration search)
    and a Spotify token. Check their "Developers Platform" (developers.spicylyrics.org) for
    a sanctioned public API before doing anything with it.
  - License is **AGPL-3.0**; this app is GPL-3.0-or-later. GPLv3 §13 allows combining with
    AGPLv3 code, but copied parts stay AGPL. Reimplementing from the TTML format and their
    behaviour as reference avoids that.
- [ ] Other sources to compare: YT Music's own lyrics (Metrolist fetches them with an
      `ANDROID_MUSIC` identity, see innertubex `YouTubeLyricsProvider`; mostly unsynced or
      line-synced), LRCLIB (line-synced, some word-synced "enhanced LRC"), lrc.red.

### Sign-in without hand-exporting cookies
Currently: import from the GUI (below). Dev override: `cookies.txt` argument or `$YTM_COOKIES`.

- [x] **Import the cookies file from the GUI** (done 2026-11-21): native file picker (portal),
      login checked with YouTube, stored encrypted (XChaCha20-Poly1305, key in the Secret
      Service / KWallet), original deleted, "Sign out" wipes file + key.
- [ ] Real drag-and-drop onto the window: blocked. Slint 1.19 has `DropArea`, but its winit
      backend doesn't deliver external file drops (and winit has no Wayland file DnD).
      Revisit when slint-ui/slint#1967 lands.
- [ ] **Session lifetime.** A cookies.txt exported from a browser that stays logged in died
      after ~1.5 h (all requests 401, 2026-11-21). Likely cause: the browser keeps rotating the
      same session. Test a private-window export (yt-dlp's advice) for how long it lasts; if
      sessions still die, the in-app login (app owns the session alone) becomes a must.
      The app already follows Set-Cookie updates in memory and stops cleanly on 401.
- [x] **Persist cookie rotation** (saved to the encrypted store when YouTube updates them).
- ~~**In-app login window**~~: not planned. Notes kept for reference:
  - A local `.html` page *can't* do this. Browsers keep each site's cookies separate, Google's
    login cookies are HttpOnly, and a page that asks for a Google password is, technically,
    a phishing page however honest its labels. Google also blocks such logins.
  - What can work: open the *real* `accounts.google.com` in an embedded webview (`wry`:
    WebKitGTK on Linux, WebView2 on Windows). Show a clear in-app banner like "Signing in to
    YouTube Music for ytm-native: this is Google's own page", let the user log in there, then
    read the `youtube.com` cookies from the webview's cookie store and encrypt them as above.
    That's what Metrolist does with Android's WebView.
  - Risk: Google may refuse embedded webviews ("This browser or app may not be secure").
    Test it on both platforms before building UI around it.
  - Fallback: "Import from my browser" (like yt-dlp `--cookies-from-browser`). Firefox's
    cookies are a plain SQLite file. Chromium-based browsers (Helium) on Linux encrypt with a
    key from the keyring, which is doable. Chrome on Windows (127+) uses app-bound encryption,
    which blocks this on purpose, so don't work around it there.

## Playback / API
- [x] Report plays to YouTube (playback + watch-time pings, like the website). Verified: plays
      show up in YT Music history.
- [x] Send the final watch-time ping when the app is closed mid-song.
- [x] Settings page; "Pause watch history" stops play reporting (the local repeat filter stays).
- [x] Start playback while still downloading (sound ~0.2 s after the stream URL; the WebM index
      is at the start, so the demuxer doesn't wait for the end).
- [ ] Non-Premium accounts: decide what happens with ads (requirement: ads still play if needed).
- [x] HTTP/2: cause found. YouTube resets HTTP/2 requests with a full Firefox User-Agent (its h2
      fingerprint isn't Firefox's). Chrome UA now; HTTP/1.1 fallback on protocol errors.
- [ ] If the TV identity stops serving streams: fall back to other identities (the probe shows
      which formats each one offers); keep `vendor/ejs` updated from yt-dlp-ejs releases.
- [x] Like / dislike the current song.
- [ ] Add to playlist / save to library (not planned yet). ("Start radio" from any song: done.)
- [x] Library: Home feed (personal shelves), Liked Music, album and playlist pages; lists play
      in order and a radio continues from the last song.
- [x] Artist pages: top songs, albums, singles/EPs, videos, playlists, similar artists; Shuffle and
      Radio (YT's artist Mix, not repeat-filtered), All songs. Reached from Home artist cards, the
      player bar's artist name, "Go to artist" in row menus. Back button walks a history stack.
- [x] Library page: saved playlists, albums, songs (play in order) and artists (followed, then
      the artists of saved songs); reloaded each time it's opened.
- [x] Right-click menu on songs/albums/playlists/queue rows: Play next, Add to queue (before the
      radio, like YT Music), Remove, Start radio, Play/Shuffle. Shuffle button on lists (all pages
      load at once and get mixed in).
- [x] Load more Home shelves on scroll (first 2 pages, then more near the end of the list).
- [x] Remember volume, queue, current song, position and queue source between launches
      (restored paused).
- [x] MPRIS: media keys, KDE media controls / lock screen, KDE Connect.
- [ ] Windows: SMTC (low priority).

## Builds
- [x] Windows build: portable .7z via `scripts/build-windows.sh` (MinGW cross-compile, DPAPI key file).
- [x] Arch package: `packaging/arch/PKGBUILD`, `scripts/build-arch.sh`.
- [x] Self-updates from GitHub releases with bsdiff patches (release builds only).
- [x] Release tooling: `scripts/release.sh`, `cargo xtask release-files`; README, HACKING.md.
- [ ] AUR package (the PKGBUILD is ready; needs an AUR account and a checksum per release).
- [ ] Windows: test the self-update swap on a real machine (renames the running .exe aside).

## UI polish
- [x] KDE/Breeze look: colors, font and icon theme from kdeglobals; System Settings-style
      sidebar, Prism-style toolbar and side panel. Pure Slint (no Qt/C++).
- [x] Follow color-scheme / font / icon-theme changes live (kdeglobals checked every 2 s).
- [x] Keyboard shortcuts: Space, ←/→ ±5 s, Ctrl+←/→ prev/next, Ctrl+↑/↓ volume, Ctrl+F, Esc.
- [x] Scroll the queue to the current song on advance.
