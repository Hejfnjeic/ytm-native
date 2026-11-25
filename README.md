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

> **A lot of this app is copied from [ytm-rs](https://github.com/grapedjuice/ytm-rs) by
> [grapedjuice](https://github.com/grapedjuice)** (GPL-3.0): the whole interface, the synced
> lyrics and their karaoke display, and much of the YouTube Music and audio code. YT Music native
> leaves some of its features out and changes or adds others (see below). All credit for those
> parts goes to grapedjuice.

A native desktop YouTube Music player for your own account, written in Rust with
[egui](https://github.com/emilk/egui). No browser, no Electron.

> Not affiliated with YouTube or Google. It talks to YouTube with your own login, like the
> website does. Use it with your own account, at your own risk.

## Features

- **Playback**: Premium-quality Opus streams, the next song fetched ahead, crossfade and gapless
  playback, an equalizer, volume normalization and playback speed.
- **Your account**: Home feed, Explore, search, Library (playlists, albums, songs, artists,
  history, uploads), album, playlist and artist pages, likes, endless radio.
- **Synced lyrics**: word-by-word karaoke from Better Lyrics, Musixmatch, LRCLIB, YouTube Music
  and more, with translations; click a line to jump there.
- **Lyrics window**: the mini player (Ctrl+M) is a small window of its own with the song's
  lyrics, kept above other windows.
- **Music videos** (optional): the music video in Now playing, in time with the song, up to 720p.
- **Desktop**: media keys and system media controls, tray, system-wide shortcuts, your own title
  bar with rounded corners or the system's.
- **Updates itself** from GitHub releases; a "What's new" window shows the changes, and the app
  restarts only when you say so.

Not included on purpose: downloads, Discord status, scrobbling, listening together, song
recognition, AirPlay, and any cache on disk.

## Install

Downloads are on the [releases page](https://github.com/Hejfnjeic/ytm-native/releases).

| | |
|---|---|
| **Arch Linux** | `sudo pacman -U ytm-native-<version>-1-x86_64.pkg.tar.zst`. pacman owns the files, so the app tells you about new versions instead of updating itself. |
| **Other Linux** (x86_64) | Unpack `ytm-native-<version>-linux-x86_64.tar.gz` and run `./install.sh` (installs to `~/.local`, adds a menu entry; `~/.local/lib/ytm-native/install.sh --uninstall` removes it). Updates itself. |
| **Windows** (x86_64) | Unpack `ytm-native-portable-<version>.zip` into a folder of its own and start `ytm-native.exe`. Portable: everything stays in that folder. Updates itself. |
| **From source** | See [HACKING.md](HACKING.md): `cargo run --release`, or `scripts/install-linux.sh`. |

Linux needs a Secret Service provider (KWallet or GNOME Keyring) to keep the login's key,
`xdg-desktop-portal` for the file picker, and `webkit2gtk-4.1` (WebKitGTK) for the lyrics.
On Windows, keep `WebView2Loader.dll` next to `ytm-native.exe`.

## Signing in

YouTube Music has no sign-in API for apps like this, so the app uses a login exported from your
browser:

1. Open a **private window** and sign in at music.youtube.com.
2. Export that site's cookies with a "cookies.txt" extension (e.g. *Get cookies.txt LOCALLY*;
   *Export*, not *Export All*).
3. Close the private window **without logging out** (that would end the login).
4. In the app (Settings › Account), choose the file. It's checked with YouTube, stored encrypted (the key lives in your
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
| Ctrl+K | command palette |
| Ctrl+M | lyrics window |
| Esc | leave the search field |

## Privacy

- Your YouTube cookies are only sent to YouTube. Lyrics services get the song's title, artist
  and length, nothing else; the Better Lyrics API's Cloudflare check runs in a hidden WebKitGTK
  (Windows: WebView2) window about once a day. YouTube Music's own lyrics are fetched
  without your login. Update checks ask GitHub for the latest
  release.
- Stored on your computer: the encrypted login, the queue and position, and settings. Nothing
  is cached on disk. Linux: `~/.local/share/ytm-native`; Windows: the
  app's `data` folder. Sign out deletes the login and its key.

## Credits

- **[ytm-rs](https://github.com/grapedjuice/ytm-rs) by grapedjuice** (GPL-3.0): a lot of this
  app is copied from it, including the interface, the lyrics and much of the playback code.
- [yt-dlp](https://github.com/yt-dlp/yt-dlp)'s [EJS](https://github.com/yt-dlp/ejs) solver
  (Unlicense/MIT/ISC) for YouTube's stream challenges.
- [rusty_h264](https://github.com/remade-with-rust/rusty_h264) (BSD-2) decodes music videos.
- Lyrics: Better Lyrics, Musixmatch, LRCLIB, YouTube Music and the other Better Lyrics sources.
- Ideas from Metrolist, yt-dlp and [spicy-lyrics](https://github.com/Spikerko/spicy-lyrics).

## License

[GPL-3.0-only](LICENSE), as ytm-rs.
