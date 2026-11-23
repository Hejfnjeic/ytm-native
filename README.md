> [!WARNING]
> # ⚠️ THIS PROJECT IS VIBECODED ⚠️
>
> **All of it was written by [Claude](https://claude.com/claude-code) (Anthropic's AI), not by a human.**
> Every line of code, every design decision, the tests, the docs, the builds and the releases
> are Claude's work. The repository owner only says what they want and reports what's broken.
> They don't write or review the code and don't know how it works inside.
>
> So please don't ask the owner to explain or debug the code. Open an issue that describes
> **what happens and what you expected**; fixes come from Claude. Use the app as it is, at your
> own risk.

# YT Music native (unofficial)

A native desktop YouTube Music player for your own account, written in Rust with a
[Slint](https://slint.dev) UI. No browser, no Electron: about 150 MB of RAM while playing.

It plays what YouTube Music plays (Premium: Opus 256 kbps), keeps your account in sync, and
looks like the rest of a KDE desktop. Linux first; Windows builds are made too.

> Not affiliated with YouTube or Google. It talks to YouTube with your own login, like the
> website does. Use it with your own account, at your own risk.

## Features

- **Playback**: Premium-quality Opus streams that start while the song downloads, the next song
  fetched ahead of time, playback speed 0.1–3× with pitch preserved.
- **Radio that doesn't stop**: radios keep refilling, and songs you heard recently are skipped.
  Albums and playlists continue with a radio when they end.
- **Your account**: Home feed, Liked Music, your Library (saved playlists, albums, songs and
  artists), albums, playlists, artist pages (with Shuffle and Radio), likes and dislikes, plays
  reported to your YouTube history (or not: *Pause watch history*).
- **Queue**: right-click any song for Play next, Add to queue, Start radio, Go to artist/album;
  shuffle lists; remove songs.
- **Synced lyrics**: word-by-word karaoke, line-by-line, or plain, from BetterLyrics, lrc.red,
  LRCLIB and YouTube Music itself; optional translations; "Wrong lyrics?" to pick another version or search by hand.
- **Desktop**: media keys and KDE's media controls (MPRIS; on Windows the system media
  controls), colors, font and icons from your KDE
  settings (follows changes live), keyboard shortcuts, remembers the queue and position.
- **Updates itself** with small binary patches (a few MB) checked against SHA-256 sums; a
  "What's new" window shows the changes, and the app restarts only when you say so.

## Install

Downloads are on the [releases page](https://github.com/Hejfnjeic/ytm-native/releases).

| | |
|---|---|
| **Arch Linux** | `sudo pacman -U ytm-native-<version>-1-x86_64.pkg.tar.zst`. pacman owns the files, so the app tells you about new versions instead of updating itself. |
| **Other Linux** (x86_64) | Unpack `ytm-native-<version>-linux-x86_64.tar.gz` and run `./install.sh` (installs to `~/.local`, adds a menu entry; `./install.sh --uninstall` removes it). Updates itself. |
| **Windows** (x86_64) | Unpack `ytm-native-<version>-windows-x86_64.7z` anywhere and start `ytm-native.exe`. Portable: everything stays in its folder. Updates itself. |
| **From source** | See [HACKING.md](HACKING.md): `cargo run --release`, or `scripts/install-linux.sh`. |

Linux needs a Secret Service provider (KWallet or GNOME Keyring) to keep the login's key, and
`xdg-desktop-portal` for the file picker.

## Signing in

YouTube Music has no sign-in API for apps like this, so the app uses a login exported from your
browser:

1. Open a **private window** and sign in at music.youtube.com.
2. Export that site's cookies with a "cookies.txt" extension (e.g. *Get cookies.txt LOCALLY*;
   *Export*, not *Export All*).
3. Close the private window **without logging out** (that would end the login).
4. In the app, choose the file. It's checked with YouTube, stored encrypted (the key lives in your
   system keyring, or with Windows' DPAPI), and the original file is deleted.

A login from a private window keeps working; one exported from your everyday browser session
tends to be logged out within hours, because the browser keeps rotating it.

## Keyboard shortcuts

| Key | |
|---|---|
| Space | play / pause |
| ← / → | back / forward 5 s |
| Ctrl+← / Ctrl+→ | previous / next song |
| Ctrl+↑ / Ctrl+↓ | volume |
| Ctrl+F | search |
| Esc | leave the search field |

## Privacy

- Your YouTube cookies are only sent to YouTube. Lyrics sites (BetterLyrics, lrc.red, LRCLIB) get
  the song's title, artist and length, nothing else. YouTube Music's own lyrics are fetched
  without your login. Update checks ask GitHub for the latest
  release.
- Stored on your computer: the encrypted login, the queue and position, settings, and the IDs of
  your last 500 songs (to keep radios fresh). Linux: `~/.local/share/ytm-native`; Windows: the
  app's `data` folder. Sign out deletes the login and its key.

## Credits

- [yt-dlp](https://github.com/yt-dlp/yt-dlp)'s [EJS](https://github.com/yt-dlp/ejs) solver
  (Unlicense/MIT/ISC, vendored in `vendor/ejs`) for YouTube's stream challenges.
- [Breeze icons](https://invent.kde.org/frameworks/breeze-icons) by KDE (LGPL), bundled as a
  fallback.
- Lyrics: BetterLyrics, [lrc.red](https://lrc.red), [LRCLIB](https://lrclib.net), YouTube Music.
- Ideas and protocol knowledge from Metrolist, yt-dlp and
  [spicy-lyrics](https://github.com/Spikerko/spicy-lyrics).

## License

[GPL-3.0-or-later](LICENSE).
