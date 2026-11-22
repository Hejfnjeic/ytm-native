//! Stand-in for the MPRIS module on systems without it (Windows): same interface, no effect.
//! (Windows media controls, SMTC, are a TODO.)

use tokio::sync::mpsc;

#[allow(dead_code)]
pub enum Command {
    PlayPause,
    Play,
    Pause,
    Next,
    Previous,
    SeekBy(f64),
    SetPosition(String, f64),
    Volume(f64),
    Raise,
}

#[allow(dead_code)]
pub enum Update {
    Track { n: u64, title: String, artist: String, album: String, art_url: Option<String>, length: f64 },
    NoTrack,
    Status { playing: bool, has_next: bool, has_prev: bool },
    Position(f64),
    Seeked(f64),
    Volume(f64),
}

pub fn start(_on_command: impl Fn(Command) + Send + Clone + 'static) -> mpsc::UnboundedSender<Update> {
    // Updates go nowhere.
    mpsc::unbounded_channel().0
}

pub fn track_id_string(n: u64) -> String {
    n.to_string()
}
