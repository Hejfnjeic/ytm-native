//! Where the app keeps its files.
//!
//! Portable mode: if a `data` folder sits next to the executable (the Windows .7z ships
//! one), everything stays inside that folder: `data/` for the login, state and history,
//! `cache/` for the YouTube player cache. Otherwise the usual per-user locations
//! (Linux: ~/.local/share/ytm-native and ~/.cache/ytm-native).

use std::{path::PathBuf, sync::OnceLock};

fn portable_root() -> Option<PathBuf> {
    static ROOT: OnceLock<Option<PathBuf>> = OnceLock::new();
    ROOT.get_or_init(|| {
        let exe = std::env::current_exe().ok()?;
        let dir = exe.parent()?.to_path_buf();
        dir.join("data").is_dir().then_some(dir)
    })
    .clone()
}

/// Login, state, history.
pub fn data_dir() -> PathBuf {
    match portable_root() {
        Some(root) => root.join("data"),
        None => dirs::data_dir().unwrap_or_else(std::env::temp_dir).join("ytm-native"),
    }
}

/// Re-creatable files (YouTube player cache).
pub fn cache_dir() -> PathBuf {
    match portable_root() {
        Some(root) => root.join("cache"),
        None => dirs::cache_dir().unwrap_or_else(std::env::temp_dir).join("ytm-native"),
    }
}

/// Give freed heap memory back to the OS (glibc keeps it otherwise). No-op elsewhere.
pub fn release_memory() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: malloc_trim only releases unused heap pages.
    unsafe {
        libc::malloc_trim(0);
    }
}
