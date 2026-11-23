//! Windows media controls (SMTC: System Media Transport Controls): media keys, the media
//! flyout next to the volume popup, the lock screen. Same interface as the MPRIS module.
//!
//! The controls come from a system `MediaPlayer` that never plays anything (its own command
//! handling is turned off), so no window handle is needed. They live on their own thread;
//! button presses arrive on Windows' thread pool and are forwarded as commands.

use std::thread;

use tokio::sync::mpsc;
use windows::{
    Foundation::{TimeSpan, TypedEventHandler, Uri},
    Media::{
        MediaPlaybackStatus, MediaPlaybackType, Playback::MediaPlayer, PlaybackPositionChangeRequestedEventArgs,
        SystemMediaTransportControls, SystemMediaTransportControlsButton, SystemMediaTransportControlsButtonPressedEventArgs,
        SystemMediaTransportControlsTimelineProperties,
    },
    Storage::Streams::RandomAccessStreamReference,
    core::HSTRING,
};

/// What Windows asked for.
#[allow(dead_code)]
pub enum Command {
    PlayPause,
    Play,
    Pause,
    Next,
    Previous,
    /// Relative seek, seconds.
    SeekBy(f64),
    /// Absolute position, seconds (only applies to the current track).
    SetPosition(String, f64),
    Volume(f64),
    Raise,
}

/// What the app tells Windows.
#[allow(dead_code)]
pub enum Update {
    Track { n: u64, title: String, artist: String, album: String, art_url: Option<String>, length: f64 },
    NoTrack,
    Status { playing: bool, has_next: bool, has_prev: bool },
    Position(f64),
    Seeked(f64),
    Volume(f64),
}

pub fn track_id_string(n: u64) -> String {
    n.to_string()
}

fn span(secs: f64) -> TimeSpan {
    TimeSpan { Duration: (secs.max(0.0) * 10_000_000.0) as i64 }
}

pub fn start(on_command: impl Fn(Command) + Send + Sync + Clone + 'static) -> mpsc::UnboundedSender<Update> {
    let (tx, rx) = mpsc::unbounded_channel::<Update>();
    let spawned = thread::Builder::new().name("smtc".into()).spawn(move || {
        if let Err(e) = run(on_command, rx) {
            eprintln!("[smtc] media controls unavailable: {e}");
        }
    });
    if let Err(e) = spawned {
        eprintln!("[smtc] no thread: {e}");
    }
    tx
}

fn run(on_command: impl Fn(Command) + Send + Sync + Clone + 'static, mut rx: mpsc::UnboundedReceiver<Update>) -> windows::core::Result<()> {
    let player = MediaPlayer::new()?;
    player.CommandManager()?.SetIsEnabled(false)?;
    let smtc: SystemMediaTransportControls = player.SystemMediaTransportControls()?;
    smtc.SetIsEnabled(true)?;
    smtc.SetIsPlayEnabled(true)?;
    smtc.SetIsPauseEnabled(true)?;
    smtc.SetPlaybackStatus(MediaPlaybackStatus::Closed)?;

    let cb = on_command.clone();
    smtc.ButtonPressed(&TypedEventHandler::<SystemMediaTransportControls, SystemMediaTransportControlsButtonPressedEventArgs>::new(
        move |_, args| {
            if let Some(args) = args.as_ref() {
                let cmd = match args.Button()? {
                    SystemMediaTransportControlsButton::Play => Some(Command::Play),
                    SystemMediaTransportControlsButton::Pause => Some(Command::Pause),
                    SystemMediaTransportControlsButton::Next => Some(Command::Next),
                    SystemMediaTransportControlsButton::Previous => Some(Command::Previous),
                    SystemMediaTransportControlsButton::FastForward => Some(Command::SeekBy(10.0)),
                    SystemMediaTransportControlsButton::Rewind => Some(Command::SeekBy(-10.0)),
                    _ => None,
                };
                if let Some(c) = cmd {
                    cb(c);
                }
            }
            Ok(())
        },
    ))?;

    // Seeking from the flyout's timeline applies to whichever track is current then.
    let track = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let (cb, t) = (on_command.clone(), track.clone());
    smtc.PlaybackPositionChangeRequested(&TypedEventHandler::<SystemMediaTransportControls, PlaybackPositionChangeRequestedEventArgs>::new(
        move |_, args| {
            if let Some(args) = args.as_ref() {
                let secs = args.RequestedPlaybackPosition()?.Duration as f64 / 10_000_000.0;
                cb(Command::SetPosition(track_id_string(t.load(std::sync::atomic::Ordering::Relaxed)), secs));
            }
            Ok(())
        },
    ))?;

    let mut length = 0.0;
    while let Some(u) = rx.blocking_recv() {
        let res: windows::core::Result<()> = (|| {
            match u {
                Update::Track { n, title, artist, album, art_url, length: len } => {
                    track.store(n, std::sync::atomic::Ordering::Relaxed);
                    length = len;
                    let d = smtc.DisplayUpdater()?;
                    d.ClearAll()?;
                    d.SetType(MediaPlaybackType::Music)?;
                    let m = d.MusicProperties()?;
                    m.SetTitle(&HSTRING::from(title))?;
                    m.SetArtist(&HSTRING::from(artist))?;
                    m.SetAlbumTitle(&HSTRING::from(album))?;
                    if let Some(url) = art_url {
                        d.SetThumbnail(&RandomAccessStreamReference::CreateFromUri(&Uri::CreateUri(&HSTRING::from(url))?)?)?;
                    }
                    d.Update()?;
                    set_timeline(&smtc, length, 0.0)?;
                }
                Update::NoTrack => {
                    smtc.DisplayUpdater()?.ClearAll()?;
                    smtc.SetPlaybackStatus(MediaPlaybackStatus::Closed)?;
                }
                Update::Status { playing, has_next, has_prev } => {
                    smtc.SetPlaybackStatus(if playing { MediaPlaybackStatus::Playing } else { MediaPlaybackStatus::Paused })?;
                    smtc.SetIsNextEnabled(has_next)?;
                    smtc.SetIsPreviousEnabled(has_prev)?;
                }
                Update::Position(p) | Update::Seeked(p) => set_timeline(&smtc, length, p)?,
                Update::Volume(_) => {}
            }
            Ok(())
        })();
        if let Err(e) = res {
            eprintln!("[smtc] {e}");
        }
    }
    drop(player);
    Ok(())
}

fn set_timeline(smtc: &SystemMediaTransportControls, length: f64, position: f64) -> windows::core::Result<()> {
    let t = SystemMediaTransportControlsTimelineProperties::new()?;
    t.SetStartTime(span(0.0))?;
    t.SetMinSeekTime(span(0.0))?;
    t.SetEndTime(span(length))?;
    t.SetMaxSeekTime(span(length))?;
    t.SetPosition(span(position))?;
    smtc.UpdateTimelineProperties(&t)
}
