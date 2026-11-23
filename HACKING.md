# Working on ytm-native

> **Vibecoded:** this whole codebase is written and maintained by Claude (Anthropic's AI) through
> Claude Code; the owner directs it but doesn't write code. This guide is for whoever (human or AI)
> works on it next.

Everything you need to change the code: how it's built, how it fits together, where to make
common changes, and how to test and release. User-facing docs are in [README.md](README.md).

## Quick start

```bash
cargo run --release                      # build and start the app (uses your stored login)
cargo test --release                     # unit tests (fast, offline)
scripts/install-linux.sh                 # build and install for your user (~/.local)
```

Rust stable (edition 2024). System packages on Arch: `opus` (linked), `alsa-lib`, `fontconfig`,
`freetype2`, `libxkbcommon`, `wayland`. No cmake needed on Linux (libopus comes from the system).

`cargo run`/`build`/`test` without `-p` mean the app (`default-members` in `Cargo.toml`). The
binary is `target/release/ytm-native`. Debug builds work but are slow; use `--release`.

## Repository layout

| Path | What |
|---|---|
| `app/` | The player (crate `ytm-native`): `src/` Rust, `ui/app.slint` the whole UI, `assets/` icons |
| `xtask/` | Release helpers, run as `cargo xtask …` (update patches + manifest) |
| `probe/` | Early research tools for YouTube's API (identities, formats); not part of the app |
| `vendor/ejs/` | yt-dlp's JS challenge solver (Unlicense/MIT/ISC), run inside the app |
| `scripts/` | install, Windows/Arch builds, release, headless test display |
| `packaging/` | `.desktop` file, Arch `PKGBUILD` |
| `TODO.md` | Open and done items |

## Ground rules

- **No C++ compiled into the binary**, also not via dependencies (Slint's Qt backend was tried and
  removed). C is acceptable (libopus, QuickJS, aws-lc) but mention new native dependencies in the
  commit. After adding one: `find target -name '*cpp*.o'` must print nothing.
- **No hardcoded song titles or artists** in code or tests: fixes are general rules. Unit tests
  use made-up names; the live lyrics test reads its cases from `$YTM_LYRICS_CASES`.
- **Performance is a feature.** The UI uses the software renderer at the monitor's refresh rate
  (180 Hz on the main machine). Per-frame work scales with the number of elements; see
  [Rendering](#rendering-and-performance). Re-measure CPU after UI/animation changes.
- **YouTube cookies only go to YouTube.** Lyrics and updates use their own HTTP clients.
- Never print or log cookies.

## Architecture

```
 UI thread (Slint)                    tokio runtime
 ┌──────────────┐  callbacks → Ev    ┌───────────────────────────────┐
 │ app.slint    │ ─────────────────▶ │ App::handle(Ev)  (main.rs)     │
 │ MainWindow   │ ◀───────────────── │  one task, events in order     │
 └──────────────┘  self.ui(|ui| …)   │  spawns work → sends Ev back   │
                                      └──────┬─────────────┬──────────┘
            audio-decode thread ◀── Audio ───┘             │
            audio-out thread (cpal)                 yt.rs / lyrics.rs / update.rs
            solver thread (QuickJS)                 (async HTTP)
            mpris thread (Linux)
```

- **One controller.** `main.rs` has `enum Ev` and `struct App`. Every UI callback and every
  finished background job becomes an `Ev`, handled one at a time by `App::handle`. Background
  work is `tokio::spawn`ed and reports back with another `Ev`, often tagged with a generation
  number (`play_gen`, `browse_gen`, …) so stale results are ignored.
- **Rust → UI**: `self.ui(move |ui| ui.set_…(…))` runs a closure on the UI thread. Slint models
  aren't `Send`, so build plain data first and the model inside the closure (see `push_lyrics`).
  `sync_model` updates list models in place (keeps scroll positions).
- **UI → Rust**: callbacks declared in `MainWindow` are wired near the end of `main()`
  (`ui.on_…(move |…| t.send(Ev::…))`).

### Files

| File | Responsibility |
|---|---|
| `main.rs` | Controller: queue, radio/list sources, views and back stack, art cache, prefetch, sign-in, lyrics choice, settings, updates, state saving, smoke mode, dev flags |
| `yt.rs` | InnerTube client: cookie session (SAPISIDHASH), search, radio (`next`), Home/album/playlist/artist parsing, TV-identity player + stream URL, chunked download, likes, play-report endpoints. 401 ⇒ `SIGNED_OUT` |
| `solver.rs` | Solves stream `sig`/`n` challenges with yt-dlp's EJS in QuickJS (own thread, per-player cache) |
| `audio.rs` | WebM demux (symphonia) → Opus (libopus) → optional WSOLA time-stretch → resample → cpal. `Stream`: a track's bytes while they download; the decoder waits for missing bytes |
| `queue.rs` | Queue editing rules (Play next / Add to queue / Remove), unit-tested |
| `lyrics.rs` | Lyrics from BetterLyrics, lrc.red (TTML, word timing, translations) LRCLIB (LRC) and YouTube Music (by video id, Android identity for timing, no cookies); ranking, translation detection, search by hand |
| `report.rs` | Reports plays to YouTube like the website (watch history) |
| `update.rs` | Self-updates from GitHub releases (bsdiff patches, SHA-256 checked) |
| `settings.rs` / `state.rs` | `settings.json` (Settings page) / `state.json` (queue, volume, position) |
| `store.rs` | Encrypted login (XChaCha20-Poly1305); key in Secret Service/KWallet, or a DPAPI file on Windows |
| `paths.rs` | Data/cache dirs; portable mode (a `data/` folder next to the exe) |
| `kde.rs` | Colors, font and icons from `~/.config/kdeglobals` (re-read when it changes) |
| `mpris.rs` / `smtc.rs` / `mpris_stub.rs` | Media keys and desktop media controls: MPRIS (Linux) / SMTC (Windows, same interface) / no-op elsewhere |
| `ui/app.slint` | All UI: Breeze-style controls (buttons, check box, slider, menus), lists, lyrics, Settings |

### How YouTube is used (why it's built this way)

- **Search, radio, library, likes, history**: the `WEB_REMIX` identity with your cookies.
  Mobile identities reject cookie logins.
- **Playback**: the TV identity (`TVHTML5`) with cookies gives Premium's Opus 256k (itag 774)
  without a PO token, but stream URLs need YouTube's `sig`/`n` challenges solved: that's the JS
  solver. The first song after YouTube ships a new player takes ~10 s; later ones are cached.
- **Fallback identities** (`yt::IDENTITIES`): if the TV identity gives no stream, or its URL
  won't download, `mweb` and then `web_creator` (logged in, Opus 128k) are tried. Identities
  without the login get URLs that stop at 1 MiB (they'd need a PO token), so none are listed.
  `ytm-native --streams <video id>` checks every identity (`YTM_PROBE=<offset>,<len>` picks
  the second range it reads); `YTM_IDENTITY=<n>` makes the app start at identity n.
- **Endless radio**: the API keeps going where the website stops (~50); the queue refills when
  fewer than 15 songs are left, and songs heard in the last 500 plays are skipped.
- **HTTP/2** with a Chrome User-Agent: YouTube resets HTTP/2 requests that claim to be Firefox
  (fingerprint mismatch). Any HTTP/2 protocol error switches the client to HTTP/1.1.

## Common changes

**A new UI property or callback.** Declare it in `MainWindow` (`app.slint`): `in property <T> foo;`
or `callback bar(int);`. Slint generates `ui.set_foo(..)` / `ui.on_bar(..)`. Set it from the
controller via `self.ui(..)`; for a callback, add an `Ev` variant, wire `ui.on_bar` in `main()`,
and handle it in `App::handle`.

**A new background job.** In `App`, clone what it needs (`self.yt.clone()`, `self.tx.clone()`),
`tokio::spawn` it, and send the result as an `Ev`. Tag it with a generation if a newer request
can supersede it.

**A YouTube response format.** Save a real response with the dev tool and read it:
`ytm-native --dump <browseId> out.json` (`cont:<token>` for continuations; uses the stored
login, never prints cookies). Parsers live in `yt.rs`; `parse_dumps` (ignored test) runs them
over saved responses in `$YTM_FIXTURES`.

**A lyrics source.** Add a method on `lyrics::Finder` returning `Lyrics` (parse with
`parse_ttml`/`parse_lrc`), call it in `find_all` with the same 6 s deadline, and let the ranking
in `find_all` order it. Use the finder's own HTTP client.

**A setting.** Add a field to `settings::Settings` (with a default), a check box on the Settings
page bound to an `in-out property`, include it in the `on_settings_changed` wiring, and act on it
in `Ev::SettingsChanged`.

## Dev switches

| Switch | Effect |
|---|---|
| `ytm-native <cookies.txt>` / `YTM_COOKIES=<file>` | use a plain cookies file instead of the stored login |
| `ytm-native --dump <browseId> <out.json>` | save a raw browse response |
| `ytm-native --version` | version, platform, update mode |
| `ytm-native --streams <video id>` | which player identities give a working stream |
| `YTM_IDENTITY=<n>` | start at that player identity (test the fallbacks) |
| `YTM_HTTP1=1` | HTTP/1.1 only |
| `YTM_UPDATE_API=<url>` | GitHub API base for update checks (tests serve a fake release) |
| `SLINT_BACKEND=…` | another Slint backend/renderer (default: winit + software) |
| `XDG_CONFIG_HOME` / `XDG_DATA_HOME` | test with other KDE settings / data dirs |

### Smoke tests (the real app, scripted)

Smoke mode runs the GUI muted, without play reporting and without reading or saving state,
takes screenshots with Slint's own renderer and quits ~30 s after its search.

| Variable | Effect |
|---|---|
| `YTM_SMOKE=<query>` | search, start a radio from the first result, seek near the end (auto-advance) |
| `YTM_SMOKE=lyrics:<query>` / `lightbox:<query>` | same, then open Lyrics / the image viewer |
| `YTM_SMOKE=home` / `liked` / `library` / `resume` | Home / play Liked Music / Library / restore the session and play |
| `YTM_SMOKE_SHOT=<file.png>` | final screenshot path |
| `YTM_SMOKE_BURST=<n>` | also save n frames 120 ms apart (animation checks) |
| `YTM_SMOKE_SIZE=<w>x<h>` | window size (the minimum width is 960) |
| `YTM_SMOKE_MINIMIZE=<secs>` | minimize after n seconds (idle CPU) |
| `YTM_SMOKE_INPUT="<secs>:<action>:<arg>; …"` | scripted input via Slint's event dispatch: `click:x,y`, `rclick:x,y`, `wheel:x,y,dy`, `key:space\|left\|right\|up\|down\|esc\|<letter>` (`ctrl+` prefix), `shot:<name>`; disables the near-end seek |
| `YTM_SMOKE_STATE=1` | let smoke mode use saved state (pair with `XDG_DATA_HOME=<tmp>`) |

Run it on a virtual display so windows don't appear on your desktop:

```bash
scripts/headless.sh env YTM_SMOKE='lyrics:some song' YTM_SMOKE_SHOT=/tmp/shot.png target/release/ytm-native
scripts/headless.sh --stop      # end the virtual KWin
```

CPU: sample `/proc/<pid>/stat` (utime+stime) over ~12 s. Reference at 180 Hz: normal view ~7 %,
lyrics ~7–9 %, lyrics plus a long scrolling title 16–22 %, minimized ~1 %. RAM ~150 MB playing.

## Rendering and performance

The Slint **software renderer** on winit (the GPU renderer pulled ~300 MB of driver libraries
into the process). Lessons learned:

- Per-frame cost scales with element count. Lyrics are a `ListView` (only visible lines exist);
  only pieces near "now" read the per-frame clock (`animation-tick()`), line state uses a 50 ms
  clock, the rest the 250 ms app clock. The marquee moves 1 px per 28 ms timer tick. Everything
  frame-driven stops while minimized (`Runtime.awake`).
- The software renderer doesn't draw `transform-scale` on text: the lyric "grow" changes the font
  size inside a fixed-width slot. All lyric lines use the same piece layout so wrapping never
  changes when a line becomes current.
- **Never `animate content-y` on a ListView whose rows differ in height**: the ListView rewrites
  `content-y` as its row estimates change, restarting the animation forever. Lyrics scroll with
  a small follower (the active line re-measures itself every 8 ms).
- Slint: gradient stop positions must be constant; no transforms on a component's root; a
  `PopupWindow` has no "closed" callback; elements inside a popup can't be read from outside.

## Data on disk

| | Linux | Windows (portable) |
|---|---|---|
| login (encrypted) | `~/.local/share/ytm-native/session.enc`, key in KWallet | `data\session.enc` + `data\session.key` (DPAPI) |
| queue, settings, recent plays | `~/.local/share/ytm-native/{state,settings,history}.json` | `data\…` |
| player cache | `~/.cache/ytm-native/` | `cache\` |

## Builds and releases

```bash
scripts/install-linux.sh [--uninstall]     # build + install for your user
scripts/build-windows.sh                   # dist/ytm-native-<ver>-windows-x86_64.7z (cross-compiled)
scripts/build-arch.sh                      # dist/ytm-native-<ver>-1-x86_64.pkg.tar.zst
DRY_RUN=1 scripts/release.sh 0.3.0         # build every release file into dist/v0.3.0, don't publish
scripts/release.sh 0.3.0                   # the whole release (see the script's header)
```

- Windows cross-compiling needs `rustup target add x86_64-pc-windows-gnu`, `mingw-w64-gcc`,
  `cmake` (libopus from source there) and `7zip`. The .exe imports only Windows system DLLs.
- **Updates**: a release carries `update-manifest.json` and bsdiff patches from the last five
  releases (`cargo xtask release-files`). Release builds (`YTM_RELEASE_BUILD=1`, set by
  `release.sh`) download the patch for their version, check the SHA-256 of the result and
  replace themselves; anything else only announces the new version. A day of changes made a
  ~3 MB patch versus a 48 MB executable. See `app/src/update.rs`.
- **Arch**: the `PKGBUILD` builds from the release tarball with `YTM_PACKAGED=pacman` (pacman
  owns the files, so the app only announces updates).
- **yt-dlp's solver** (`vendor/ejs/`): update from yt-dlp-ejs releases when stream URLs stop
  working; `solver.rs` loads `core.min.js` and `lib.min.js`.
