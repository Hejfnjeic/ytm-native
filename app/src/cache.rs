//! ytm-native keeps no cache on disk (covers, pages and lyrics come fresh each launch, by
//! request). What builds before this one left there is deleted once, at startup. Kept:
//! the sign-in and settings, the lyrics check's browser profile (`webview-lyrics`, which
//! the check needs) and downloaded updates.

use std::path::Path;

/// What earlier builds cached in the data folder.
const LEFT_OVER: &[&str] = &[
    // Covers and thumbnails.
    "thumbs",
    // Translated lyrics.
    "translations",
    // The last Home page.
    "home.json",
];

/// Delete what earlier builds cached, in the background.
pub fn remove_left_over(data_dir: &Path) {
    let dir = data_dir.to_owned();
    std::thread::spawn(move || {
        for name in LEFT_OVER {
            let path = dir.join(name);
            let gone = if path.is_dir() { std::fs::remove_dir_all(&path) } else { std::fs::remove_file(&path) };
            if gone.is_ok() {
                log::info!("removed the old cache {name}");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    #[test]
    fn old_caches_go_and_the_rest_stays() {
        let dir = std::env::temp_dir().join(format!("ytm-native-cache-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for path in ["thumbs/a", "translations/b.json", "home.json", "webview-lyrics/Cookies", "updates/x"] {
            let p = dir.join(path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, b"x").unwrap();
        }
        super::remove_left_over(&dir);
        for _ in 0..100 {
            if !dir.join("thumbs").exists() && !dir.join("home.json").exists() && !dir.join("translations").exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(!dir.join("thumbs").exists() && !dir.join("translations").exists() && !dir.join("home.json").exists());
        assert!(dir.join("webview-lyrics/Cookies").exists() && dir.join("updates/x").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
