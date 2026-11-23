//! MPRIS (org.mpris.MediaPlayer2): media keys, KDE's media controls / lock screen,
//! `playerctl`, KDE Connect and the like.
//!
//! `mpris_server::Player` isn't thread-safe, so it lives on its own thread with a
//! single-threaded runtime. Commands from the desktop are forwarded as app events;
//! the controller sends `Update`s back.

use std::thread;

use mpris_server::{Metadata, PlaybackStatus, Player, Time, TrackId};
use tokio::sync::mpsc;

/// What the desktop asked for.
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

/// What the app tells the desktop.
pub enum Update {
    Track { n: u64, title: String, artist: String, album: String, art_url: Option<String>, length: f64 },
    NoTrack,
    Status { playing: bool, has_next: bool, has_prev: bool },
    Position(f64),
    Seeked(f64),
    Volume(f64),
}

fn track_id(n: u64) -> TrackId {
    TrackId::try_from(format!("/org/ytm_native/track/t{n}")).unwrap_or(TrackId::NO_TRACK)
}

pub fn start(on_command: impl Fn(Command) + Send + Clone + 'static) -> mpsc::UnboundedSender<Update> {
    let (tx, mut rx) = mpsc::unbounded_channel::<Update>();
    thread::Builder::new()
        .name("mpris".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(e) => return eprintln!("[mpris] no runtime: {e}"),
            };
            let local = tokio::task::LocalSet::new();
            local.block_on(&rt, async move {
                let player = match Player::builder("ytm_native")
                    .identity("ytm-native")
                    .desktop_entry("ytm-native")
                    .can_play(true)
                    .can_pause(true)
                    .can_seek(true)
                    .can_control(true)
                    .can_raise(true)
                    .can_go_next(false)
                    .can_go_previous(false)
                    .build()
                    .await
                {
                    Ok(p) => p,
                    Err(e) => return eprintln!("[mpris] couldn't register on D-Bus: {e}"),
                };
                let c = on_command.clone();
                player.connect_play_pause(move |_| c(Command::PlayPause));
                let c = on_command.clone();
                player.connect_play(move |_| c(Command::Play));
                let c = on_command.clone();
                player.connect_pause(move |_| c(Command::Pause));
                let c = on_command.clone();
                player.connect_stop(move |_| c(Command::Pause));
                let c = on_command.clone();
                player.connect_next(move |_| c(Command::Next));
                let c = on_command.clone();
                player.connect_previous(move |_| c(Command::Previous));
                let c = on_command.clone();
                player.connect_seek(move |_, t| c(Command::SeekBy(t.as_micros() as f64 / 1e6)));
                let c = on_command.clone();
                player.connect_set_position(move |_, id, t| c(Command::SetPosition(id.to_string(), t.as_micros() as f64 / 1e6)));
                let c = on_command.clone();
                player.connect_set_volume(move |_, v| c(Command::Volume(v)));
                let c = on_command.clone();
                player.connect_raise(move |_| c(Command::Raise));
                tokio::task::spawn_local(player.run());

                while let Some(u) = rx.recv().await {
                    let r = match u {
                        Update::Track { n, title, artist, album, art_url, length } => {
                            let mut m = Metadata::builder().trackid(track_id(n)).title(title).artist([artist]);
                            if !album.is_empty() {
                                m = m.album(album);
                            }
                            if let Some(a) = art_url {
                                m = m.art_url(a);
                            }
                            if length > 0.0 {
                                m = m.length(Time::from_millis((length * 1000.0) as i64));
                            }
                            player.set_position(Time::ZERO);
                            player.set_metadata(m.build()).await
                        }
                        Update::NoTrack => player.set_metadata(Metadata::new()).await,
                        Update::Status { playing, has_next, has_prev } => {
                            let status = if playing { PlaybackStatus::Playing } else { PlaybackStatus::Paused };
                            let mut r = Ok(());
                            if player.playback_status() != status {
                                r = player.set_playback_status(status).await;
                            }
                            if player.can_go_next() != has_next {
                                r = r.and(player.set_can_go_next(has_next).await);
                            }
                            if player.can_go_previous() != has_prev {
                                r = r.and(player.set_can_go_previous(has_prev).await);
                            }
                            r
                        }
                        Update::Position(p) => {
                            player.set_position(Time::from_millis((p * 1000.0) as i64));
                            Ok(())
                        }
                        Update::Seeked(p) => {
                            let t = Time::from_millis((p * 1000.0) as i64);
                            player.set_position(t);
                            player.seeked(t).await
                        }
                        Update::Volume(v) => {
                            if (player.volume() - v).abs() > 0.001 { player.set_volume(v).await } else { Ok(()) }
                        }
                    };
                    if let Err(e) = r {
                        eprintln!("[mpris] {e}");
                    }
                }
            });
        })
        .expect("spawn mpris thread");
    tx
}

/// The track id string the desktop will send back in SetPosition for track `n`.
pub fn track_id_string(n: u64) -> String {
    track_id(n).to_string()
}
