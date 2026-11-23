//! Look like the rest of the KDE desktop: colors, font and icons from the user's
//! KDE settings (~/.config/kdeglobals). Falls back to Breeze Dark defaults (set in the UI).

use std::{collections::HashMap, path::PathBuf};

use slint::{Color, ComponentHandle, Image};

use crate::{Icons, MainWindow, Theme};

fn kdeglobals_path() -> PathBuf {
    dirs::config_dir().unwrap_or_default().join("kdeglobals")
}

/// When the KDE settings last changed (None if there are none, e.g. on Windows). Watched so a
/// new color scheme, font or icon theme applies without a restart.
pub fn settings_changed_at() -> Option<std::time::SystemTime> {
    std::fs::metadata(kdeglobals_path()).and_then(|m| m.modified()).ok()
}

/// kdeglobals as section -> key -> value.
fn read_kdeglobals() -> HashMap<String, HashMap<String, String>> {
    let path = kdeglobals_path();
    let mut out: HashMap<String, HashMap<String, String>> = HashMap::new();
    let Ok(text) = std::fs::read_to_string(path) else { return out };
    let mut section = String::new();
    for line in text.lines() {
        let line = line.trim();
        if let Some(s) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = s.to_string();
        } else if let Some((k, v)) = line.split_once('=') {
            out.entry(section.clone()).or_default().insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    out
}

fn color(cfg: &HashMap<String, HashMap<String, String>>, section: &str, key: &str) -> Option<Color> {
    let v = cfg.get(section)?.get(key)?;
    let p: Vec<u8> = v.split(',').filter_map(|x| x.trim().parse().ok()).collect();
    match p[..] {
        [r, g, b] => Some(Color::from_rgb_u8(r, g, b)),
        [r, g, b, a] => Some(Color::from_argb_u8(a, r, g, b)),
        _ => None,
    }
}

fn mix(a: Color, b: Color, t: f32) -> Color {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color::from_rgb_u8(l(a.red(), b.red()), l(a.green(), b.green()), l(a.blue(), b.blue()))
}

/// First matching icon file in the icon theme (and Breeze as fallback).
fn find_icon(themes: &[String], name: &str) -> Option<PathBuf> {
    let mut bases: Vec<PathBuf> = vec![dirs::data_dir().unwrap_or_default().join("icons")];
    bases.push(PathBuf::from("/usr/share/icons"));
    for theme in themes {
        for base in &bases {
            for cat in ["actions", "places", "status", "devices", "emblems", "apps", "categories"] {
                for size in ["22", "24", "16", "32", "symbolic"] {
                    for ext in ["svg", "png"] {
                        let p = base.join(theme).join(cat).join(size).join(format!("{name}.{ext}"));
                        if p.exists() {
                            return Some(p);
                        }
                    }
                }
            }
        }
    }
    None
}

/// Breeze icons compiled into the binary (assets/icons, LGPL, see the README there).
fn bundled(name: &str) -> Option<&'static [u8]> {
    macro_rules! icons {
        ($($n:literal),* $(,)?) => {
            match name {
                $($n => Some(include_bytes!(concat!("../assets/icons/", $n, ".svg")) as &[u8]),)*
                _ => None,
            }
        };
    }
    icons!(
        "media-playback-start", "media-playback-pause", "media-skip-forward", "media-skip-backward",
        "go-home", "emblem-favorite", "edit-find", "view-media-lyrics", "view-media-playlist", "radio",
        "go-previous", "audio-volume-high", "audio-volume-muted", "im-user", "system-log-out", "media-album-cover",
        "media-playlist-shuffle", "configure", "folder-music-symbolic",
    )
}

pub fn apply(ui: &MainWindow) {
    let cfg = read_kdeglobals();
    let t = ui.global::<Theme>();
    let set = |section: &str, key: &str, f: &dyn Fn(Color)| {
        if let Some(c) = color(&cfg, section, key) {
            f(c);
        }
    };
    set("Colors:Window", "BackgroundNormal", &|c| t.set_window(c));
    set("Colors:Window", "BackgroundAlternate", &|c| t.set_window_alt(c));
    set("Colors:View", "BackgroundNormal", &|c| t.set_view(c));
    set("Colors:View", "BackgroundAlternate", &|c| t.set_view_alt(c));
    set("Colors:Button", "BackgroundNormal", &|c| t.set_button(c));
    set("Colors:Header", "BackgroundNormal", &|c| t.set_header(c));
    set("Colors:Window", "ForegroundNormal", &|c| t.set_text(c));
    set("Colors:Window", "ForegroundInactive", &|c| t.set_dim(c));
    set("Colors:Selection", "BackgroundNormal", &|c| t.set_highlight(c));
    set("Colors:Selection", "ForegroundNormal", &|c| t.set_highlight_text(c));
    set("Colors:Window", "ForegroundPositive", &|c| t.set_positive(c));
    set("Colors:Window", "ForegroundNeutral", &|c| t.set_neutral(c));
    set("Colors:Window", "ForegroundNegative", &|c| t.set_negative(c));
    // Breeze draws frames/separators as the text color at ~20% over the window color.
    t.set_separator(mix(t.get_window(), t.get_text(), 0.2));

    // Font: "Noto Sans,10,-1,5,400,0,0,0,0,0,0,0,0,0,0,1" (family, point size, …).
    if let Some(font) = cfg.get("General").and_then(|g| g.get("font")) {
        let mut parts = font.split(',');
        if let Some(family) = parts.next().filter(|f| !f.is_empty()) {
            t.set_font_family(family.into());
        }
        if let Some(pt) = parts.next().and_then(|p| p.parse::<f32>().ok()).filter(|p| *p > 4.0) {
            t.set_font_size(pt * 96.0 / 72.0);
        }
    }

    let mut themes: Vec<String> = cfg.get("Icons").and_then(|i| i.get("Theme")).cloned().into_iter().collect();
    themes.extend(["breeze-dark".to_string(), "breeze".to_string(), "hicolor".to_string()]);
    // The desktop's icon theme first; the bundled Breeze copies otherwise (Windows has none).
    let icon = |names: &[&str]| -> Image {
        names
            .iter()
            .find_map(|n| find_icon(&themes, n))
            .and_then(|p| Image::load_from_path(&p).ok())
            .or_else(|| names.iter().find_map(|n| bundled(n)).and_then(|svg| Image::load_from_svg_data(svg).ok()))
            .unwrap_or_default()
    };
    let i = ui.global::<Icons>();
    i.set_play(icon(&["media-playback-start"]));
    i.set_pause(icon(&["media-playback-pause"]));
    i.set_next(icon(&["media-skip-forward"]));
    i.set_prev(icon(&["media-skip-backward"]));
    i.set_home(icon(&["go-home"]));
    i.set_liked(icon(&["emblem-favorite", "love", "rating"]));
    i.set_search(icon(&["edit-find", "search"]));
    i.set_lyrics(icon(&["view-media-lyrics", "text-plain"]));
    i.set_queue(icon(&["view-media-playlist", "amarok_playlist"]));
    i.set_radio(icon(&["radio", "media-repeat-all"]));
    i.set_back(icon(&["go-previous"]));
    i.set_volume(icon(&["audio-volume-high"]));
    i.set_muted(icon(&["audio-volume-muted"]));
    i.set_user(icon(&["im-user", "user-identity"]));
    i.set_sign_out(icon(&["system-log-out"]));
    i.set_app(Image::load_from_svg_data(include_bytes!("../assets/icon.svg")).unwrap_or_default());
    i.set_album(icon(&["media-album-cover", "audio-x-generic"]));
    i.set_shuffle(icon(&["media-playlist-shuffle"]));
    i.set_settings(icon(&["configure", "settings-configure"]));
    i.set_library(icon(&["folder-music-symbolic", "folder-music"]));
}
