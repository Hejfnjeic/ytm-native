//! What survives a restart: volume, queue, current song, position, and where the queue
//! came from. Stored as JSON in the data dir (no secrets in here).

use std::{fs, path::PathBuf};

use serde::{Deserialize, Serialize};

use crate::yt::Song;

#[derive(Serialize, Deserialize, Clone, PartialEq)]
pub struct SavedSource {
    pub badge: String,
    pub title: String,
    /// Set for radios: the seed to continue from.
    pub radio_seed: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq)]
pub struct Saved {
    pub volume: f32,
    #[serde(default = "one")]
    pub speed: f32,
    pub queue: Vec<Song>,
    pub current: Option<usize>,
    pub position: f64,
    pub source: Option<SavedSource>,
}

impl Default for Saved {
    fn default() -> Self {
        Self { volume: 0.8, speed: 1.0, queue: vec![], current: None, position: 0.0, source: None }
    }
}

fn one() -> f32 {
    1.0
}

fn path() -> PathBuf {
    crate::paths::data_dir().join("state.json")
}

pub fn load() -> Saved {
    fs::read_to_string(path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

pub fn save(s: &Saved) {
    let p = path();
    let tmp = p.with_extension("json.tmp");
    let write = || -> std::io::Result<()> {
        fs::create_dir_all(p.parent().unwrap())?;
        fs::write(&tmp, serde_json::to_vec(s).unwrap_or_default())?;
        fs::rename(&tmp, &p)
    };
    if let Err(e) = write() {
        eprintln!("[state] couldn't save: {e}");
    }
}
