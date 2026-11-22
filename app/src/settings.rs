//! User settings (the Settings page). JSON in the data dir, next to `state.json`.

use std::{fs, path::PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct Settings {
    /// Don't report plays to YouTube (they stay out of the account's history and don't shape
    /// recommendations). The app's own repeat filter for radios keeps working.
    pub pause_history: bool,
    /// Look for a new version at startup (and every 6 hours while running).
    pub check_updates: bool,
    /// Download and install updates on their own; they apply at the next start.
    pub auto_update: bool,
    /// Show lyric translations (when the lyrics have them) under each line.
    pub show_translation: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self { pause_history: false, check_updates: true, auto_update: true, show_translation: false }
    }
}

fn path() -> PathBuf {
    crate::paths::data_dir().join("settings.json")
}

pub fn load() -> Settings {
    fs::read_to_string(path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

pub fn save(s: &Settings) {
    let p = path();
    let tmp = p.with_extension("json.tmp");
    let write = || -> std::io::Result<()> {
        fs::create_dir_all(p.parent().unwrap())?;
        fs::write(&tmp, serde_json::to_vec_pretty(s).unwrap_or_default())?;
        fs::rename(&tmp, &p)
    };
    if let Err(e) = write() {
        eprintln!("[settings] couldn't save: {e}");
    }
}
