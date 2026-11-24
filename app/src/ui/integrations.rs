//! The window title, song notifications, the system media controls, the app icon, updates and the tray.

use std::sync::Arc;
use std::time::Duration;

use crate::backend;

use super::{App, dev_var, notes, updatable};

impl App {

    /// The window's title, which the taskbar and Alt+Tab show: the current song, or the
    /// app's name with none. One fixed title while the mini player is the lyrics window:
    /// Wayland lets no app put itself above others, but a KDE window rule for that title
    /// (and the app, ytm-native) can.
    pub(super) fn sync_title(&mut self, ctx: &egui::Context) {
        let want = if self.mini.active() && super::mini::lyrics_active() {
            crate::tr!("player-lyrics-window").to_owned()
        } else {
            window_title(self.current().map(|t| (t.title.as_str(), t.artist_line())))
        };
        if want != self.window_title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(want.clone()));
            self.window_title = want;
        }
    }

    /// A notification when a new song starts while the window is hidden, minimized or
    /// behind other windows (the mini player on show counts as seen). Not for the song
    /// picked in the app, nor the same song again (Repeat one).
    pub(super) fn sync_song_notification(&mut self, ctx: &egui::Context) {
        if !self.song_notifications || !self.playing() || self.buffering || !crate::notify::available() {
            return;
        }
        if self.notified.as_ref().is_some_and(|(g, _)| *g == self.generation) {
            return;
        }
        let Some(t) = self.current().cloned() else { return };
        let again = self.notified.as_ref().is_some_and(|(_, id)| *id == t.id);
        self.notified = Some((self.generation, t.id.clone()));
        let focused = ctx.input(|i| i.viewport().focused) != Some(false);
        let seen = !self.window_hidden && (focused || self.mini.showing());
        if again || seen {
            return;
        }
        if self.notifier.is_none() {
            self.notifier = crate::notify::Notifier::spawn();
        }
        let Some(n) = self.notifier.clone() else { return };
        let song = crate::notify::Song {
            title: t.title.clone(),
            artist: t.artist_line(),
            cover: None,
        };
        let (art, id, generation) = (backend::art_url(&t), t.id, self.generation);
        n.expect(generation);
        let dir = self.backend.data_dir().join("notification-cover");
        let http = self.backend.http();
        self.backend.spawn(async move {
            let cover = match art {
                Some(url) => async {
                    let bytes = http.get(&url).timeout(Duration::from_secs(5)).send().await?.error_for_status()?.bytes().await?;
                    // A new name per song: Windows keeps showing an older picture by its path.
                    let _ = std::fs::remove_dir_all(&dir);
                    std::fs::create_dir_all(&dir)?;
                    let path = dir.join(format!("{}.jpg", file_safe(&id)));
                    std::fs::write(&path, &bytes)?;
                    anyhow::Ok(path)
                }
                .await
                .inspect_err(|e| log::debug!("notification cover: {e:#}"))
                .ok(),
                None => None,
            };
            n.show(generation, crate::notify::Song { cover, ..song });
        });
    }

    /// Switch the window, tray and shortcuts to another icon.
    pub(crate) fn set_app_icon(&mut self, ctx: &egui::Context, id: &'static str) {
        if id == self.app_icon {
            return;
        }
        self.app_icon = id;
        let v = crate::appicon::get(id);
        crate::appicon::save(self.backend.data_dir(), id);
        if cfg!(windows) {
            crate::appicon::set_window_icon(v);
        } else if let Some(icon) = v.icon_data() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Icon(Some(Arc::new(icon))));
        }
        crate::appicon::sync_shortcuts(id, self.backend.data_dir());
        // `tray_window` makes it again with the new icon.
        self.tray = None;
        log::info!("app icon: {id}");
    }

    /// Automatic update checks: shortly after launch, then every six hours.
    pub(super) fn tick_updates(&mut self, ctx: &egui::Context, now: f64) {
        use crate::update::{State, Updater};
        if self.updater.is_none() {
            let c = ctx.clone();
            self.updater = Some(Updater::new(self.backend.data_dir(), self.update_nightly, move || c.request_repaint()));
        }
        let Some(u) = self.updater.clone() else { return };
        // After a check GitHub turned down for its rate limit (a manual one too), again when the limit ends.
        let next = match u.state() {
            State::RateLimited { until } => now + until.saturating_duration_since(std::time::Instant::now()).as_secs_f64(),
            _ => self.update_checked.map_or(8.0, |t| t + 6.0 * 3600.0),
        };
        let due = now >= next && !matches!(u.state(), State::Ready { .. });
        let enabled = self.auto_update && updatable() && dev_var("YTM_BACKGROUND").is_none_or(|_| dev_var("YTM_UPDATE_FORCE").is_some() || dev_var("YTM_UPDATE_FEED").is_some());
        if enabled && due {
            self.update_checked = Some(now);
            u.check();
        } else if enabled && !matches!(u.state(), State::Ready { .. }) {
            // An idle app doesn't repaint on its own: wake up when the next check is due.
            ctx.request_repaint_after(Duration::from_secs_f64((next - now).max(0.5) + 0.1));
        }
        match u.state() {
            State::Ready { version, .. } if !self.update_announced => {
                self.update_announced = true;
                self.notes_dialog = Some(notes::NotesDialog { version, ready: true });
            }
            // A packaged copy (.deb, Arch, Flatpak): once per version, not every launch (the sidebar keeps saying so).
            State::Available { version, .. } if !self.update_announced && self.update_seen.as_ref() != Some(&version) => {
                self.update_announced = true;
                self.update_seen = Some(version.clone());
                self.notes_dialog = Some(notes::NotesDialog { version, ready: true });
            }
            _ => {}
        }
    }

    /// Switch between the stable and nightly update channels, and look for an update
    /// on the new one if updates are automatic.
    pub(super) fn set_update_nightly(&mut self, nightly: bool) {
        if nightly == self.update_nightly {
            return;
        }
        self.update_nightly = nightly;
        log::info!("update channel: {}", if nightly { "nightly" } else { "stable" });
        if let Some(u) = &self.updater {
            u.set_nightly(nightly);
            if self.auto_update && updatable() {
                u.check();
            }
        }
    }

    /// Close the app so the downloaded update installs, then relaunch it.
    pub(super) fn restart_to_update(&mut self, ctx: &egui::Context) {
        self.restart_for_update = true;
        self.quit(ctx);
    }

    /// Close and start again, e.g. so a new proxy setting takes effect.
    pub(super) fn restart(&mut self, ctx: &egui::Context) {
        self.restart_app = true;
        self.quit(ctx);
    }

    /// Close the app for real, even with "Close to tray" on.
    pub(super) fn quit(&mut self, ctx: &egui::Context) {
        self.exiting = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    /// "Close to tray": while it's on there's a tray icon, and × hides the window
    /// behind it. The icon brings the window back or quits. eframe keeps running
    /// `logic` while the window is hidden.
    ///
    /// On Wayland a window can't hide itself, so × minimizes it instead. It can't
    /// bring itself back either: the compositor is only asked for attention, and the
    /// window counts as hidden until it has focus again.
    pub(super) fn tray_window(&mut self, ctx: &egui::Context, frame: &eframe::Frame) {
        // Turning the setting off and on again tries again after a failure.
        self.tray_failed &= self.close_to_tray;
        let want = self.close_to_tray && !self.exiting && !self.tray_failed;
        if want && self.tray.is_none() {
            let c = ctx.clone();
            self.tray_error = None;
            match crate::tray::Tray::new(crate::appicon::get(self.app_icon), move || c.request_repaint()) {
                Ok(t) => self.tray = Some(t),
                // Without an icon there'd be no way back: × just closes.
                Err(e) => {
                    log::warn!("tray icon unavailable: {e:#}");
                    if let Some(u) = e.downcast_ref::<crate::tray::Unsupported>() {
                        // It can't work here: off, with the reason under the setting.
                        self.close_to_tray = false;
                        self.tray_error = Some(u.0.clone());
                    } else {
                        self.tray_failed = true;
                    }
                }
            }
        } else if !want && self.tray.is_some() {
            self.tray = None;
            if self.in_tray && !self.exiting {
                self.show_window(ctx);
            }
        }
        if self.tray.is_none() {
            return;
        }
        let wayland = is_wayland(frame);
        if wayland && self.in_tray && back_from_tray(ctx.input(|i| i.viewport().focused) != Some(false), &mut self.tray_left_focus) {
            self.in_tray = false;
        }
        let hide = |app: &mut Self| {
            ctx.send_viewport_cmd(hide_command(wayland));
            app.in_tray = true;
            app.tray_left_focus = false;
        };
        let show = |app: &mut Self| app.bring_back(ctx, wayland);
        if ctx.input(|i| i.viewport().close_requested()) && !self.exiting {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            hide(self);
        }
        // Minimized counts as hidden: "Show" is what brings it back.
        let shown = |app: &Self| !app.in_tray && ctx.input(|i| i.viewport().minimized) != Some(true);
        for cmd in crate::tray::take() {
            match cmd {
                crate::tray::Cmd::Show => show(self),
                crate::tray::Cmd::Toggle if shown(self) => hide(self),
                crate::tray::Cmd::Toggle => show(self),
                // As the media keys do.
                crate::tray::Cmd::PlayPause => self.toggle(),
                crate::tray::Cmd::Next => self.next(),
                crate::tray::Cmd::Prev => self.prev(),
                crate::tray::Cmd::Quit => self.quit(ctx),
            }
        }
        let shown = shown(self);
        // "Pause" while a song is starting too, as the media controls have it.
        let playing = self.playing() || self.buffering;
        let song = self.current().map(|t| (t.title.clone(), t.artist_line()));
        if let Some(t) = &mut self.tray {
            t.set_shown(shown);
            t.set_now(song.as_ref().map(|(title, artist)| (title.as_str(), artist.as_str())), playing);
        }
    }

    /// The window back for the tray's Show or a later launch. Wayland ignores Visible,
    /// un-minimizing and Focus, so there it can only ask for attention (see `tray_window`).
    pub(super) fn bring_back(&mut self, ctx: &egui::Context, wayland: bool) {
        if wayland {
            ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(egui::UserAttentionType::Informational));
        } else {
            self.show_window(ctx);
        }
    }

    /// Bring the window back (from the tray, or minimized) and to the front.
    pub(super) fn show_window(&mut self, ctx: &egui::Context) {
        self.in_tray = false;
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }

    /// Download the media controls' cover through our own client, into a file.
    pub(super) fn fetch_media_cover(&self, video_id: String, url: String) {
        let (slot, b) = (self.media_cover.clone(), self.backend.clone());
        let dir = self.backend.data_dir().join("media-cover");
        self.backend.spawn(async move {
            let res = async {
                let bytes = b.http().get(&url).send().await?.error_for_status()?.bytes().await?;
                // A new name per song, so Windows doesn't show a cached older cover.
                let _ = std::fs::remove_dir_all(&dir);
                std::fs::create_dir_all(&dir)?;
                let path = dir.join(format!("{}.jpg", file_safe(&video_id)));
                std::fs::write(&path, &bytes)?;
                anyhow::Ok(path)
            }
            .await;
            match res {
                Ok(path) => {
                    *slot.lock().unwrap() = Some((video_id, format!("file://{}", path.display())));
                    b.wake();
                }
                Err(e) => log::debug!("media cover: {e:#}"),
            }
        });
    }
}

/// "Title – Artist" for a song, the app's name without one.
fn window_title(song: Option<(&str, String)>) -> String {
    match song {
        Some((title, artist)) if !artist.is_empty() => format!("{title} – {artist}"),
        Some((title, _)) if !title.is_empty() => title.to_owned(),
        _ => "YT Music".to_owned(),
    }
}

pub(super) fn is_wayland(frame: &eframe::Frame) -> bool {
    use raw_window_handle::{HasDisplayHandle, RawDisplayHandle};
    frame.display_handle().is_ok_and(|h| matches!(h.as_raw(), RawDisplayHandle::Wayland(_)))
}

/// `Visible(false)` does nothing on Wayland; minimizing is what's left.
fn hide_command(wayland: bool) -> egui::ViewportCommand {
    if wayland { egui::ViewportCommand::Minimized(true) } else { egui::ViewportCommand::Visible(false) }
}

/// Wayland doesn't say when a minimized window comes back (from the taskbar, say),
/// but it gets focus again. `left_focus` needs the focus lost first, since the window
/// still has it for a moment after being minimized.
fn back_from_tray(focused: bool, left_focus: &mut bool) -> bool {
    if !focused {
        *left_focus = true;
        return false;
    }
    std::mem::take(left_focus)
}

/// A song's ID as a file name: only letters, digits, `-` and `_`, so one that isn't a
/// YouTube ID can't point outside the folder.
fn file_safe(id: &str) -> String {
    id.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_')).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_title_is_the_song() {
        assert_eq!(window_title(Some(("Get Lucky", "Daft Punk".to_owned()))), "Get Lucky – Daft Punk");
        assert_eq!(window_title(Some(("Get Lucky", String::new()))), "Get Lucky");
        assert_eq!(window_title(Some(("", String::new()))), "YT Music");
        assert_eq!(window_title(None), "YT Music");
    }

    #[test]
    fn wayland_minimizes_because_it_cannot_hide() {
        assert_eq!(hide_command(true), egui::ViewportCommand::Minimized(true));
        assert_eq!(hide_command(false), egui::ViewportCommand::Visible(false));
    }

    #[test]
    fn stays_in_tray_until_focus_is_lost_and_regained() {
        let mut left = false;
        // Still focused just after minimizing: not back yet.
        assert!(!back_from_tray(true, &mut left));
        assert!(!back_from_tray(false, &mut left));
        assert!(!back_from_tray(false, &mut left));
        assert!(back_from_tray(true, &mut left));
        // Once only.
        assert!(!back_from_tray(true, &mut left));
    }
}
