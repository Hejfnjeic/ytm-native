//! The mini player: a lyrics window of its own, beside the main one, like Spicy Lyrics'
//! popout in Spicetify. The song's animated background with its synced lyrics, the song
//! on top, and while pointed at, previous/play/next, keep on top, back to the full window
//! and a seek line.
//!
//! The lyrics show in one place at a time: opening this closes them in the main window's
//! Now playing, and opening them there (the lyrics button) closes this.
//!
//! Kept above other windows unless unpinned. On Wayland no app can ask for that itself,
//! so on KDE Plasma KWin is asked to (`keep_above`); elsewhere the window level does it.

use std::time::Duration;

use egui::{Color32, Pos2, Rect, Sense, Ui, Vec2, ViewportBuilder, ViewportCommand, ViewportId, WindowLevel, pos2, vec2};

use super::widgets::{self, icon_at, named, paint_cover, text_at};
use super::{Action, App, titlebar};
use crate::theme::{self, icon};

/// The window when first opened, and as small and as big as it goes.
const SIZE: Vec2 = vec2(400.0, 540.0);
const MIN: Vec2 = vec2(280.0, 220.0);
const MAX: Vec2 = vec2(1200.0, 1400.0);
/// The window's top: the song, and where it's dragged by.
const HEADER: f32 = 60.0;
/// The lyrics' size against Now playing's: a small window wants smaller text.
pub const TEXT_SCALE: f32 = 0.62;
/// Kept above other windows until unpinned.
const PINNED_BY_DEFAULT: bool = true;

/// The window's title: fixed, so a desktop's window rule can find it.
pub fn title() -> &'static str {
    crate::tr!("player-lyrics-window")
}

fn viewport_id() -> ViewportId {
    ViewportId::from_hash_of("ytm-native-lyrics-window")
}

pub struct Mini {
    open: bool,
    /// Opening or closing asked for, done at the start of the next frame.
    wanted: Option<bool>,
    pub pinned: bool,
    /// The size it was last left at.
    size: Vec2,
    /// When it last opened: keep-on-top is asked for again once it's surely mapped.
    opened_at: f64,
    /// Whether KWin was last asked to keep it above (on Wayland).
    kept_above: Option<bool>,
}

impl Mini {
    pub fn load(get: impl Fn(&str) -> Option<String>) -> Self {
        let size = get("lyrics_window_size")
            .and_then(|v| serde_json::from_str::<[f32; 2]>(&v).ok())
            .map(|[w, h]| vec2(w, h).clamp(MIN, MAX))
            .unwrap_or(SIZE);
        Mini { open: false, wanted: None, pinned: get("mini_pinned").map_or(PINNED_BY_DEFAULT, |v| v == "1"), size, opened_at: f64::NEG_INFINITY, kept_above: None }
    }

    pub fn save(&self, storage: &mut dyn eframe::Storage) {
        storage.set_string("mini_pinned", if self.pinned { "1" } else { "0" }.to_owned());
        storage.set_string("lyrics_window_size", serde_json::to_string(&[self.size.x, self.size.y]).unwrap_or_default());
    }

    pub fn active(&self) -> bool {
        self.open
    }

    pub fn request(&mut self, on: bool) {
        self.wanted = Some(on);
    }

    pub fn take_request(&mut self) -> Option<bool> {
        self.wanted.take()
    }

    pub fn set_pinned(&mut self, ctx: &egui::Context, on: bool) {
        self.pinned = on;
        if self.open {
            ctx.send_viewport_cmd_to(viewport_id(), ViewportCommand::WindowLevel(if on { WindowLevel::AlwaysOnTop } else { WindowLevel::Normal }));
            self.kept_above = None;
        }
    }
}

impl App {
    /// Open the lyrics window (its lyrics leave Now playing) or close it.
    pub(super) fn set_mini(&mut self, on: bool, now: f64) {
        if on == self.mini.open {
            return;
        }
        self.mini.open = on;
        if on {
            self.mini.opened_at = now;
            self.mini.kept_above = None;
            if self.np_open && self.np_tab == super::NpTab::Lyrics {
                self.np_open = false;
            }
        }
    }

    /// Every frame: the lyrics window, while it's open. It's drawn in step with the main
    /// window (an immediate viewport), from the same state. Lyrics opened in the main
    /// window's Now playing meanwhile close it: they show in one place at a time.
    pub(super) fn show_mini(&mut self, ctx: &egui::Context, now: f64) {
        if !self.mini.open {
            return;
        }
        if self.np_open && self.np_tab == super::NpTab::Lyrics {
            self.mini.open = false;
            return;
        }
        let custom = titlebar::custom();
        let builder = ViewportBuilder::default()
            .with_title(title())
            .with_app_id("ytm-native")
            .with_inner_size(self.mini.size)
            .with_min_inner_size(MIN)
            .with_max_inner_size(MAX)
            .with_decorations(!custom)
            .with_transparent(custom && crate::corners::available())
            .with_window_level(if self.mini.pinned { WindowLevel::AlwaysOnTop } else { WindowLevel::Normal });
        ctx.show_viewport_immediate(viewport_id(), builder, |ctx, _class| {
            if ctx.input(|i| i.viewport().close_requested()) {
                self.mini.open = false;
                return;
            }
            if let Some(r) = ctx.input(|i| i.viewport().inner_rect) {
                self.mini.size = r.size().clamp(MIN, MAX);
            }
            egui::CentralPanel::default().frame(egui::Frame::NONE).show(ctx, |ui| lyrics_window(self, ui, now, custom));
            if custom {
                titlebar::resize_edges(ctx);
            }
        });
        self.keep_mini_above(now);
    }

    /// Wayland: ask KWin to keep the window above others (or not), once it's mapped.
    fn keep_mini_above(&mut self, now: f64) {
        if !self.mini.open || !crate::wayland_window() || self.mini.kept_above == Some(self.mini.pinned) {
            return;
        }
        // Mapped by then, so KWin can find it.
        if now - self.mini.opened_at < 0.4 {
            return;
        }
        self.mini.kept_above = Some(self.mini.pinned);
        crate::keep_above::set(title(), self.mini.pinned);
    }
}

/// The window's contents: the song's background with its synced lyrics, the song on top
/// (where the window is dragged by), and while pointed at, the transport, pin, the full
/// window and a seek line.
fn lyrics_window(app: &mut App, ui: &mut Ui, now: f64, custom: bool) {
    let ctx = ui.ctx().clone();
    let r = ctx.content_rect();
    // With the app's own frame the window is see-through: everything goes in a rounded card.
    let radius = if custom && crate::corners::available() && !ctx.input(|i| i.viewport().maximized == Some(true)) { crate::corners::RADIUS } else { 0.0 };
    super::nowplaying::backdrop(app, ui, r, r, now, None);
    crate::corners::cut(&ctx, radius);
    let track = app.current().cloned();
    let header = Rect::from_min_size(r.min, vec2(r.width(), HEADER));
    if titlebar::drag_strip(ui, header, "lyrics-mini").double_clicked() {
        app.actions.push(Action::Mini(false));
    }

    // The lyrics, under the song: as Now playing draws them, smaller, centred when narrow.
    let body = Rect::from_min_max(pos2(r.left() + 4.0, header.bottom()), pos2(r.right() - 4.0, r.bottom() - 6.0));
    let mut lyrics_ui = ui.new_child(egui::UiBuilder::new().max_rect(body).layout(egui::Layout::top_down(egui::Align::Min)));
    lyrics_ui.set_clip_rect(body);
    match &track {
        Some(_) => super::nowplaying::lyrics_panel(app, &mut lyrics_ui, body, now, body.width() < 560.0, TEXT_SCALE),
        None => icon_at(ui, body.center(), icon::MUSIC, 32.0, theme::text_faint()),
    }

    // The song: its cover, title and artist.
    if let Some(t) = &track {
        let cover = Rect::from_min_size(pos2(r.left() + 14.0, header.center().y - 18.0), Vec2::splat(36.0));
        paint_cover(ui, cover, &t.thumbs, theme::thumb_radius(), 1.0);
        let x = cover.right() + 11.0;
        let w = (r.right() - 14.0 - x).max(0.0);
        text_at(ui, pos2(x, cover.top() + 1.0), &t.title, theme::semibold(14.0), theme::text(), w);
        text_at(ui, pos2(x, cover.top() + 20.0), &t.artist_line(), theme::regular(12.0), theme::text_dim(), w);
    }

    // The controls, while pointed at (or one has the keyboard).
    let keys = ["prev", "play", "next", "pin", "full", "close"];
    let btn_id = |key: &str| egui::Id::unique(("lyrics-mini-btn", key));
    let focused = ctx.memory(|m| m.focused()).is_some_and(|f| keys.iter().any(|k| btn_id(k) == f));
    let over = ctx.input(|i| i.pointer.hover_pos()).is_some_and(|p| r.contains(p));
    let reveal = theme::anim_bool(&ctx, "lyrics-mini-controls", over || focused, 0.15);
    if reveal <= 0.0 {
        return;
    }
    // The song gives way to the buttons; a shade behind the transport keeps it readable.
    let round = radius as u8;
    ui.painter().rect_filled(header, egui::CornerRadius { nw: round, ne: round, sw: 0, se: 0 }, theme::shade(0.55 * reveal));
    let bar = Rect::from_min_max(pos2(r.left(), r.bottom() - 84.0), r.max);
    super::shell::cover_vgradient(ui.painter(), bar, egui::CornerRadius { nw: 0, ne: 0, sw: round, se: round }, 0.0, Color32::TRANSPARENT, theme::shade(0.7 * reveal));
    let enabled = track.is_some();
    let btn = |ui: &mut Ui, c: Pos2, side: f32, key: &str, name: &str| -> egui::Response {
        let b = Rect::from_center_size(c, Vec2::splat(side));
        let resp = named(ui.interact(b, btn_id(key), Sense::click()), name).on_hover_text(name).on_hover_cursor(egui::CursorIcon::PointingHand);
        let h = theme::anim_bool(ui.ctx(), resp.id.with("h"), resp.hovered(), 0.12);
        if h > 0.0 {
            ui.painter().circle_filled(c, side / 2.0, Color32::WHITE.gamma_multiply(0.16 * h * reveal));
        }
        if resp.has_focus() {
            ui.painter().circle_stroke(c, side / 2.0 - 1.0, egui::Stroke::new(1.5, Color32::WHITE));
        }
        resp
    };
    // Not while still fading in: a quick click there was meant for the window.
    let live = |resp: &egui::Response| resp.clicked() && (reveal > 0.5 || resp.has_focus());
    let white = Color32::WHITE.gamma_multiply(reveal);

    // Previous, play, next, along the bottom.
    let c = pos2(r.center().x, r.bottom() - 38.0);
    let gap = 52.0;
    let prev = btn(ui, c - vec2(gap, 0.0), 38.0, "prev", crate::tr!("ui-previous"));
    icon_at(ui, c - vec2(gap, 0.0), icon::SKIP_BACK, 19.0, white);
    if live(&prev) && enabled {
        app.prev();
    }
    let next = btn(ui, c + vec2(gap, 0.0), 38.0, "next", crate::tr!("ui-next"));
    icon_at(ui, c + vec2(gap, 0.0), icon::SKIP_FORWARD, 19.0, white);
    if live(&next) && enabled {
        app.next();
    }
    let play = btn(ui, c, 46.0, "play", if app.playing() { crate::tr!("ui-pause") } else { crate::tr!("ui-play") });
    ui.painter().circle_filled(c, 21.0, white);
    play_glyph(app, ui, c, 1.05, Color32::BLACK.gamma_multiply(reveal), now);
    if live(&play) && enabled {
        app.toggle();
    }

    // Top right: close (with no system frame to have one), back to the full window, pin.
    let mut x = r.right() - 24.0;
    let mut corner = |ui: &mut Ui, glyph: char, key: &str, name: &str| -> bool {
        let resp = btn(ui, pos2(x, header.center().y), 32.0, key, name);
        icon_at(ui, pos2(x, header.center().y), glyph, 16.0, white);
        x -= 36.0;
        live(&resp)
    };
    if custom && corner(ui, icon::X, "close", crate::tr!("ui-close")) {
        app.actions.push(Action::Mini(false));
    }
    if corner(ui, icon::MAXIMIZE_2, "full", &crate::tr!("player-with-shortcut", action = crate::tr!("player-full-window"), key = "Ctrl+M")) {
        app.actions.push(Action::Mini(false));
        ctx.send_viewport_cmd_to(ViewportId::ROOT, ViewportCommand::Focus);
    }
    let pinned = app.mini.pinned;
    if corner(ui, if pinned { icon::PIN } else { icon::PIN_OFF }, "pin", if pinned { crate::tr!("ui-unpin-window") } else { crate::tr!("settings-keep-on-top") }) {
        app.mini.set_pinned(&ctx, !pinned);
    }

    let mut faded = ui.new_child(egui::UiBuilder::new().max_rect(r));
    faded.multiply_opacity(reveal);
    seek_line(app, &mut faded, r, track.as_ref(), radius);
}

/// The play button's glyph: play, pause, or a spinner while a song starts.
fn play_glyph(app: &App, ui: &Ui, c: Pos2, k: f32, glyph: Color32, now: f64) {
    let morph = theme::anim_bool(ui.ctx(), "mini-play-morph", app.playing() || app.buffering, 0.18);
    if app.buffering {
        let a = now as f32 * 6.0;
        let pts = (0..20).map(|i| c + vec2((a + i as f32 * 0.22).cos(), (a + i as f32 * 0.22).sin()) * 7.0 * k).collect();
        ui.painter().add(egui::Shape::Path(egui::epaint::PathShape::line(pts, egui::epaint::PathStroke::new(2.0, glyph))));
        ui.ctx().request_repaint_after(Duration::from_millis(33));
    } else if morph > 0.5 {
        widgets::pause_bars(ui, c, 6.5 * k * (2.0 * morph - 1.0).max(0.4), glyph);
    } else {
        widgets::play_triangle(ui, c + vec2(1.2 * k, 0.0), 7.0 * k * (1.0 - 2.0 * morph).max(0.4), glyph);
    }
}

/// Mute, or back to the volume from before.
pub(super) fn toggle_mute(app: &mut App, ui: &Ui) {
    let vol = app.audio.volume();
    let stash = super::shell::unmute_id();
    if vol > 0.0 {
        ui.data_mut(|d| d.insert_temp(stash, vol));
        app.audio.set_volume(0.0);
    } else {
        app.audio.set_volume(ui.data(|d| d.get_temp::<f32>(stash)).unwrap_or(0.6));
    }
}

/// The song's length in seconds: its stream's, else what it's listed with, else 0.
fn song_length(app: &App, track: Option<&crate::model::Track>) -> f32 {
    app.audio.status.duration().map(|d| d.as_secs_f32()).or(track.and_then(|t| t.duration).map(|d| d as f32)).unwrap_or(0.0)
}

/// A line along the bottom; thicker on hover, with the time under the pointer.
fn seek_line(app: &mut App, ui: &mut Ui, r: Rect, track: Option<&crate::model::Track>, radius: f32) {
    let status = app.audio.status.clone();
    let total = song_length(app, track);
    let pos = app.seek_drag.unwrap_or(status.position_smooth().as_secs_f32()).min(total.max(0.0));
    // Inside the rounded corners.
    let r = r.shrink2(vec2(radius, 0.0));
    let hit = Rect::from_min_max(pos2(r.left(), r.bottom() - 10.0), r.max);
    let resp = ui.interact(hit, ui.scope_id().with("mini-seek"), Sense::click_and_drag());
    let resp = if total > 0.0 { resp.on_hover_cursor(egui::CursorIcon::PointingHand) } else { resp };
    resp.widget_info(|| egui::WidgetInfo::slider(total > 0.0, pos as f64, crate::tr!("ui-seek")));
    app.seek_wheel.hover(&resp);
    let h = theme::anim_bool(ui.ctx(), "mini-seek-h", total > 0.0 && (resp.hovered() || resp.dragged()), 0.15);
    let thick = 4.0 + 2.0 * h;
    let line = Rect::from_min_max(pos2(r.left(), r.bottom() - thick), r.max);
    ui.painter().rect_filled(line, 0.0, theme::glass(0.2));
    let frac = if total > 0.0 { pos / total } else { 0.0 };
    ui.painter().rect_filled(Rect::from_min_size(line.min, vec2(line.width() * frac, thick)), 0.0, app.accent_color());
    let at = |p: Pos2| ((p.x - r.left()) / r.width()).clamp(0.0, 1.0) * total;
    if h > 0.0
        && let Some(p) = resp.hover_pos().or(resp.interact_pointer_pos())
    {
        // With the time left on, what would be left from there.
        let txt = super::fmt_running(at(p), total, app.time_left);
        let g = widgets::time_galley(ui, &txt, theme::semibold(11.5), Color32::WHITE);
        let half = g.size().x / 2.0 + 6.0;
        let x = p.x.clamp(r.left() + half + 2.0, r.right() - half - 2.0);
        let bg = Rect::from_center_size(pos2(x, line.top() - 13.0), g.size() + vec2(12.0, 5.0));
        ui.painter().rect_filled(bg, 5.0, theme::shade(0.8).gamma_multiply(h));
        ui.painter().galley(bg.min + vec2(6.0, 2.5), g, Color32::WHITE.gamma_multiply(h));
    }
    if total > 0.0
        && let Some(p) = resp.interact_pointer_pos()
    {
        if resp.dragged() {
            app.seek_drag = Some(at(p));
        }
        if resp.drag_stopped() || resp.clicked() {
            app.seek_drag = None;
            app.actions.push(Action::Seek(at(p)));
        }
    }
}
