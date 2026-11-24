//! Signing in by importing a cookies file exported from a browser, and uploading songs
//! to the account.

use std::sync::atomic::Ordering;

use crate::backend::Req;

use super::App;

impl App {
    /// Open the system file picker for songs to upload; it runs on its own thread.
    pub(super) fn pick_uploads(&self, ctx: &egui::Context) {
        let (slot, ctx) = (self.picked_uploads.clone(), ctx.clone());
        std::thread::spawn(move || {
            let files = rfd::FileDialog::new().set_title("Upload music").add_filter("Audio", UPLOAD_EXTS).pick_files();
            if let Some(files) = files {
                *slot.lock().unwrap() = Some(files);
                ctx.request_repaint();
            }
        });
    }

    pub(super) fn upload_files(&mut self, files: Vec<std::path::PathBuf>) {
        let (ok, skipped): (Vec<_>, Vec<_>) = files.into_iter().partition(|f| {
            f.extension().and_then(|e| e.to_str()).is_some_and(|e| UPLOAD_EXTS.contains(&e.to_ascii_lowercase().as_str()))
        });
        if !self.logged_in {
            self.toast(crate::tr!("status-sign-in-upload"));
        } else if ok.is_empty() {
            if !skipped.is_empty() {
                self.toast(crate::tr!("status-upload-formats"));
            }
        } else {
            self.toast(crate::tr!("status-uploading", count = ok.len()));
            self.backend.request(Req::Upload(ok));
        }
    }

    /// Sign in: pick a `cookies.txt` exported from a browser where YouTube Music is signed
    /// in (a private window's export lasts longest). The file picker runs on its own
    /// thread; the youtube.com cookies in it go to the backend, which checks them.
    pub(super) fn start_sign_in(&self, ctx: &egui::Context) {
        if self.signing_in.swap(true, Ordering::Relaxed) {
            return;
        }
        let (backend, busy, ctx, checking) = (self.backend.clone(), self.signing_in.clone(), ctx.clone(), self.cookie_file.clone());
        std::thread::spawn(move || {
            let picked = rfd::FileDialog::new().set_title(crate::tr!("login-pick-cookies")).add_filter("cookies.txt", &["txt"]).pick_file();
            if let Some(path) = picked {
                match std::fs::read_to_string(&path).map_err(anyhow::Error::from).and_then(|text| cookie_header(&text)) {
                    Ok(header) => {
                        // Deleted once YouTube takes it (`login_settled`).
                        *checking.lock().unwrap() = Some(path);
                        backend.request(Req::SetCookie(header));
                    }
                    Err(e) => backend.report_error(crate::tr!("login-failed", error = format!("{e:#}").as_str())),
                }
            }
            busy.store(false, Ordering::Relaxed);
            ctx.request_repaint();
        });
    }
}

impl App {
    /// The sign-in's instructions and its button are in Settings › Account: other Sign in
    /// buttons open it rather than the file picker.
    pub(super) fn open_sign_in(&mut self) {
        self.settings_tab = super::pages::SettingsTab::Account;
        self.actions.push(super::Action::Go(super::Page::Settings));
    }

    /// The login being checked is settled: taken (its file is deleted, as the login lives
    /// encrypted now) or refused (its file stays, to try again).
    pub(super) fn login_settled(&mut self, taken: bool) {
        let Some(path) = self.cookie_file.lock().unwrap().take() else { return };
        if taken {
            match std::fs::remove_file(&path) {
                Ok(()) => log::info!("signed in; the cookies file is deleted"),
                Err(e) => log::warn!("signed in, but the cookies file couldn't be deleted: {e}"),
            }
        }
    }

    /// A cookies file is being checked with YouTube.
    pub(super) fn checking_login(&self) -> bool {
        self.cookie_file.lock().unwrap().is_some()
    }
}

/// The `Cookie` header for youtube.com from a Netscape `cookies.txt` (what browser
/// extensions and yt-dlp export). Cookies for other sites in the file are left out.
pub(super) fn cookie_header(text: &str) -> anyhow::Result<String> {
    let mut cookies: Vec<(&str, &str)> = Vec::new();
    for line in text.lines() {
        let line = line.strip_prefix("#HttpOnly_").unwrap_or(line);
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() >= 7 && f[0].trim_start_matches('.').ends_with("youtube.com") {
            let (name, value) = (f[5], f[6].trim_end());
            match cookies.iter_mut().find(|(n, _)| *n == name) {
                Some(c) => c.1 = value,
                None => cookies.push((name, value)),
            }
        }
    }
    anyhow::ensure!(cookies.iter().any(|(n, _)| *n == "SAPISID"), "{}", crate::tr!("login-no-sapisid"));
    Ok(cookies.iter().map(|(n, v)| format!("{n}={v}")).collect::<Vec<_>>().join("; "))
}

/// File types YouTube Music accepts for upload.
const UPLOAD_EXTS: &[&str] = &["mp3", "m4a", "flac", "ogg", "wma"];

#[cfg(test)]
mod tests {
    use super::cookie_header;

    #[test]
    fn youtube_cookies_from_a_netscape_file() {
        let file = "# Netscape HTTP Cookie File\n.youtube.com\tTRUE\t/\tTRUE\t0\tSAPISID\tabc\n#HttpOnly_.youtube.com\tTRUE\t/\tTRUE\t0\tSID\txyz\n.example.com\tTRUE\t/\tFALSE\t0\tOTHER\t1\n";
        assert_eq!(cookie_header(file).unwrap(), "SAPISID=abc; SID=xyz");
        assert!(cookie_header(".youtube.com\tTRUE\t/\tTRUE\t0\tPREF\tf1\n").is_err());
    }
}
