//! Keep a window above others on KDE Plasma's Wayland session, where no app can ask for
//! that itself: KWin is handed a small script (over D-Bus, as KDE's own tools do) that
//! sets the window's "keep above" by its title. Elsewhere this does nothing; X11 and
//! Windows take the window level the app asks for.

/// Ask KWin to keep the window titled `title` (of this app) above others, or not. Runs on
/// a thread of its own: D-Bus can take a moment.
pub fn set(title: &'static str, above: bool) {
    #[cfg(target_os = "linux")]
    std::thread::Builder::new()
        .name("keep-above".into())
        .spawn(move || {
            if let Err(e) = kwin(title, above) {
                log::info!("keep above: KWin didn't take it ({e:#}); a KDE window rule for \"{title}\" can do it");
            } else {
                log::info!("keep above: {above} for \"{title}\"");
            }
        })
        .ok();
    #[cfg(not(target_os = "linux"))]
    let _ = (title, above);
}

/// The script, for the window of this app with `title`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn script(title: &str, above: bool) -> String {
    let title = serde_json::to_string(title).unwrap_or_default();
    format!(
        "for (const w of workspace.windowList()) {{\n    if (w.resourceClass == \"ytm-native\" && w.caption == {title}) {{\n        w.keepAbove = {above};\n    }}\n}}\n"
    )
}

#[cfg(target_os = "linux")]
fn kwin(title: &str, above: bool) -> anyhow::Result<()> {
    use dbus::blocking::Connection;
    use std::time::Duration;
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    anyhow::ensure!(desktop.split(':').any(|d| d.eq_ignore_ascii_case("KDE")), "not KDE Plasma ({desktop})");
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let path = dir.join(format!("ytm-native-keep-above-{}.js", std::process::id()));
    std::fs::write(&path, script(title, above))?;
    let conn = Connection::new_session()?;
    let scripting = conn.with_proxy("org.kde.KWin", "/Scripting", Duration::from_secs(3));
    const NAME: &str = "ytm-native-keep-above";
    // A copy left loaded from before wouldn't run again.
    let _: Result<(bool,), _> = scripting.method_call("org.kde.kwin.Scripting", "unloadScript", (NAME,));
    let (id,): (i32,) = scripting.method_call("org.kde.kwin.Scripting", "loadScript", (path.to_string_lossy().as_ref(), NAME))?;
    anyhow::ensure!(id >= 0, "KWin refused the script");
    let script = conn.with_proxy("org.kde.KWin", format!("/Scripting/Script{id}"), Duration::from_secs(3));
    let run: Result<(), _> = script.method_call("org.kde.kwin.Script", "run", ());
    let _: Result<(bool,), _> = scripting.method_call("org.kde.kwin.Scripting", "unloadScript", (NAME,));
    let _ = std::fs::remove_file(&path);
    run?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_script_names_the_window_safely() {
        let s = super::script("Lyrics · \"x\"", true);
        assert!(s.contains("w.caption == \"Lyrics · \\\"x\\\"\""));
        assert!(s.contains("w.keepAbove = true;"));
        assert!(s.contains("w.resourceClass == \"ytm-native\""));
    }
}
