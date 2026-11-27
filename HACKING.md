# Working on ytm-native

> **Vibecoded:** this whole codebase is written and maintained by Claude (Anthropic's AI) through
> Claude Code; the owner directs it but doesn't write code. This guide is for whoever (human or AI)
> works on it next.

> **Much of `app/` is copied from [ytm-rs](https://github.com/grapedjuice/ytm-rs) by
> grapedjuice** (GPL-3.0, at commit 446c5b0): the egui interface, the lyrics and their karaoke
> renderer, and much of the YouTube and audio code. Its features this app leaves out are
> removed, not hidden (list in [README.md](README.md)); don't bring them back.

Everything you need to change the code: how it's built, how it fits together, and how to test
and release it. User-facing docs are in [README.md](README.md).

## Quick start

```bash
cargo run --release                      # build and start the app (uses your stored login)
cargo test                               # unit tests (fast, offline)
scripts/install-linux.sh                 # build and install for your user (~/.local)
```

Rust stable (edition 2024). On Arch: `alsa-lib`, `dbus`, `fontconfig`, `freetype2`, `gtk3`,
`libxkbcommon`, `openssl`, `wayland`, `webkit2gtk-4.1` (the same list as the `PKGBUILD`).

`cargo run`/`build`/`test` without `-p` mean the app (`default-members` in `Cargo.toml`). Debug
builds are usable: dependencies are optimized even there (`[profile.dev.package."*"]`).

## Repository layout

| Path | What |
|---|---|
| `app/` | The player (crate `ytm-native`): `src/` Rust, `locales/` its text in 20 languages, `assets/` icon, fonts, the one theme, yt-dlp's EJS solver (Unlicense/MIT/ISC) |
| `app/vendor/` | Patched copies of epaint (right-to-left text), souvlaki (MPRIS updates) and winit (X11 scale, Windows move/resize repaints), from ytm-rs, and eframe (a minimized window painted directly up to 60 fps, for the lyrics window on Windows; on Wayland a window the app minimized stays minimized until it loses and regains focus, so it isn't drawn when KWin asks for previews; look for `ytm-native:`); see `[patch.crates-io]` in `Cargo.toml` |
| `probe/` | Early research tools for YouTube's API (identities, formats); not part of the app |
| `scripts/` | install, Windows/Arch builds, release, headless test display |
| `packaging/` | `.desktop` file, Arch `PKGBUILD` |
| `TODO.md` | Ideas not done |

## Ground rules

- **No C++ compiled into the binary**, also not through dependencies (Slint's Qt backend was
  tried and removed). C is acceptable (QuickJS, aws-lc, libopus on Windows); mention new native
  dependencies in the commit. After adding one: `find target -name '*cpp*.o'` must print nothing.
- **No hardcoded song titles or artists** in code or tests: fixes are general rules, and tests
  use made-up names.
- **No disk cache** (covers, pages, lyrics, translations): only the login, settings, queue,
  history, the lyrics check's browser profile and downloaded updates are kept (`cache.rs`).
- **One theme** (`assets/themes/presets/ytm-rs.json`). ytm-rs's theme engine (`preset.rs`)
  stays, but the other themes and the picker are gone.
- **Performance is a feature.** egui re-lays-out the whole window every frame, so frames are
  the main cost: pages ask for none while idle and only what their animations need
  (`ui/pace.rs`, the end of `App::logic`). Re-measure CPU after UI or animation changes.
- **YouTube cookies only go to YouTube.** Never print or log them.
- **Every user-visible text is in `locales/`**, in all 20 languages; a test fails on an English
  id that's unused or missing.

## Architecture

```
 UI thread (eframe, egui + OpenGL)           backend thread (tokio)
 ┌───────────────────────────────┐  Req     ┌──────────────────────────────┐
 │ ui::App: logic() + ui()       │ ───────▶ │ backend.rs: YouTube requests, │
 │ every frame: drain Resp,      │ ◀─────── │ stream resolving, lyrics,     │
 │ apply, lay out, draw          │  Resp    │ images, uploads …            │
 └──────────────┬────────────────┘          └──────────────────────────────┘
                │ commands / status
      audio thread (audio.rs: rodio/cpal, crossfade, EQ, speed)
      stream download (stream.rs), video decoder (video.rs), solver (jsc.rs)
      lyrics window: a second window drawn in its own passes (ui/mini.rs, ui::Host)
```

- **One UI state.** `ui::App` (`ui/mod.rs`) holds everything the window shows. `logic()` runs
  every frame (also while minimized): it drains the backend's `Resp`s (`ui/responses.rs`),
  audio events and media keys, and handles `Action`s queued by clicks (`ui/actions.rs`).
  `ui()` lays out and draws: `ui/shell.rs` (sidebar, top bar, player bar), `ui/pages.rs`,
  `ui/nowplaying.rs`.
- **Backend** (`backend.rs`): the UI sends a `Req`, the tokio side answers with a `Resp` and
  wakes the UI. Playback resolution lives there too: resolve the stream, start the download,
  build the decoder, hand it to the audio engine.
- **eframe runs `ui::Host`**, which shares the `App` (an `Rc<RefCell<App>>`) with the lyrics
  window. On Linux that window is a deferred viewport with passes of its own (a minimized
  window gets no frames on Wayland). On Windows it's an immediate viewport drawn in the main
  window's passes: Windows hands out `WM_PAINT` one window at a time, and a second window
  asking for 60 fps starved the main one (1.0.1); painting the starved one directly broke the
  lyrics window's focus and input (1.0.2). `YTM_MINI_IMMEDIATE=1` tests the Windows way here.

### How YouTube is used

- **Pages, library, likes, search, radio**: `ytmapi.rs`/`ytm.rs`, as music.youtube.com (the
  `WEB_REMIX` client) with the imported cookies.
- **Streams**: the visionOS client (`innertube.rs`) gives plain URLs without challenges or a PO
  token; where a network cuts its file URLs at 1 MiB, its HLS (`hls.rs`). Premium-only, age-
  restricted songs and Premium's quality go through the signed-in web music client
  (`webmusic.rs`), whose challenges yt-dlp's EJS solver answers in QuickJS (`jsc.rs`; it falls
  back to the last player version that worked). Both mirror yt-dlp's client definitions:
  update them together when YouTube changes.
- **Music videos**: the video's H.264 (≤720p) decoded on the CPU by rusty_h264 (`video.rs`);
  a song's paired video comes from YouTube's counterpart (Premium) and is offset to the song.
- **Lyrics** (`lyrics.rs`, `ui/karaoke.rs`): every Better Lyrics source. The Better Lyrics API
  needs a Cloudflare Turnstile token: a hidden WebKitGTK (WebView2 on Windows) window solves it
  in a child process (`ytm-native --turnstile`, `turnstile.rs`); the token lasts 24 h and is
  kept in the vault.
- **Login** (`vault.rs`, `ui/account.rs`): a cookies.txt from the browser, checked with YouTube,
  stored with XChaCha20-Poly1305; the key in the Secret Service (Linux) or a DPAPI file
  (Windows; never Credential Manager). YouTube's cookie updates are saved.

### Window and rendering

- OpenGL (glow) only. `renderer::only_used_gl_driver` makes libglvnd load just NVIDIA's EGL
  driver when every card is NVIDIA's (Mesa's LLVM was ~100 MB of the app's memory otherwise).
- Linux, own title bar: the window is transparent and `corners.rs` cuts rounded corners each
  frame. The animated background and glass are GL shaders (`shader.rs`).
- KDE Wayland can't be asked for keep-above by the app: `keep_above.rs` loads a KWin script
  over D-Bus for the lyrics window.

## Dev switches

| Switch | Effect |
|---|---|
| `YTM_DATA_DIR=<dir>` | another data folder (a scratch profile; its settings are `<dir>/app.ron`) |
| `YTM_MUTE=1` | no sound |
| `YTM_SMOKE=<query>` / `id:<videoId>` | search and play the first result / play that video (`YTM_VIDEO=1`: as a music video) |
| `YTM_PAGE=settings` / `artist:<id>` | open on that page |
| `YTM_NOWPLAYING=1`, `YTM_NP_TAB=lyrics\|art` | open Now playing on that tab |
| `YTM_MINI=<secs>[,<secs>…]` | switch to the lyrics window and back at those times |
| `YTM_SCRIPT="<secs>:<step>;…"` + `YTM_SCRIPT_DIR` | scripted input: `c:x,y` click, `m:x,y` move, `t:text`, `k:esc`, `w:n` scroll, `s:name` screenshot, `q` quit at once, `x` close the window as a user would (the app's closing runs) |
| `YTM_UPDATE_FEED=<url>` | read the update release from there (a fake release; with `YTM_UPDATE_FORCE=1` any build counts as installed, `YTM_UPDATE_DRYRUN=1` logs instead of installing) |
| `RUST_LOG=ytm_native=debug` | logging |

The full list is in `ui/dev.rs` and `grep -r 'YTM_' app/src`.

### Testing on a headless display

Test windows go on a virtual KWin so they never appear on (or take focus from) the desktop:

```bash
scripts/headless.sh env YTM_DATA_DIR=/tmp/prof YTM_MUTE=1 YTM_SMOKE=id:<videoId> \
    YTM_SCRIPT="15:s:shot;16:q" YTM_SCRIPT_DIR=/tmp target/debug/ytm-native
scripts/headless.sh --stop      # end the virtual KWin
```

- The headless KWin shares the real session bus, so the lyrics window's keep-above request
  reaches the real KWin (harmless: it matches only that window's title).
- When measuring or stopping a test copy, pick it by its `YTM_DATA_DIR` in
  `/proc/<pid>/environ`, never by name: the user's own copy has the same name.
- CPU: sample `/proc/<pid>/stat` (utime+stime) over ~12 s. Reference (release, 60 Hz): idle
  ~0.1 %, Now playing with karaoke ~8 %, lyrics window ~6 %. Memory (NVIDIA): ~260 MB RSS, of
  which the app's own ~95 MB; the rest is shared driver code.

## Data on disk

| | Linux | Windows (portable) |
|---|---|---|
| login, Better Lyrics token (encrypted) | `~/.local/share/ytm-native/{session,lyrics-jwt}.enc`, key in the Secret Service | `data\…`, key in a DPAPI file |
| settings | `~/.local/share/ytm-native/app.ron` | `data\app.ron` |
| queue, recent plays | `state.json`, `history.json` there | `data\…` |
| solver, lyrics check profile, updates | `ejs/`, `webview-lyrics/`, `updates/` there | `data\…` |

## Builds and releases

```bash
scripts/install-linux.sh [--uninstall]     # build + install for your user (never updates itself)
scripts/build-windows.sh                   # dist/ytm-native-portable-<ver>.zip (cross-compiled)
scripts/build-arch.sh                      # dist/ytm-native-<ver>-1-x86_64.pkg.tar.zst
DRY_RUN=1 scripts/release.sh 1.0.1         # build every release file into dist/v1.0.1, don't publish
NOTES_FILE=notes.md scripts/release.sh 1.0.1   # the whole release (see the script's header)
```

- A release has three downloads, which are also what the app's updater (`update.rs`) fetches:
  - `ytm-native-<ver>-linux-x86_64.tar.gz`: `install.sh` puts the program in
    `~/.local/lib/ytm-native/` with `install.txt` beside it (so it replaces itself as it
    closes) and links `~/.local/bin/ytm-native` to it.
  - `ytm-native-portable-<ver>.zip` (Windows): `ytm-native.exe`, `WebView2Loader.dll` (MinGW
    links WebView2's loader as a DLL), `portable.txt` (data beside the exe, updates swap the
    exe) at the top level, where the updater looks.
  - `ytm-native-<ver>-1-x86_64.pkg.tar.zst` (Arch): the program in `/usr/lib/ytm-native/` with
    `pacman.txt`, so it only tells when a new version is out.
- Release notes are shown in the app ("What's new"), so write them for listeners, in plain
  words. The app stops reading at the `## Install` section `release.sh` adds.
- Windows cross-compiling needs `rustup target add x86_64-pc-windows-gnu`, `mingw-w64-gcc`,
  `cmake` and `7zip`. Windows builds are never run here: friends report problems.
- Before publishing, `release.sh` checks the binaries contain no paths from your home folder.
  After publishing it deletes `dist/v<ver>/` and the Arch build folders.
- **yt-dlp's solver** (`app/assets/ejs/`): update from yt-dlp-ejs releases when signed-in
  streams stop working.
