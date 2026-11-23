// No console window on Windows.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod audio;
mod kde;
mod lyrics;
#[cfg(target_os = "linux")]
mod mpris;
#[cfg(not(target_os = "linux"))]
#[path = "mpris_stub.rs"]
mod mpris;
mod paths;
mod queue;
mod settings;
mod update;
mod report;
mod solver;
mod state;
mod store;
mod yt;

use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use anyhow::Result;
use rand::{RngExt, seq::SliceRandom};
use slint::{ComponentHandle, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, SharedString, VecModel};
use tokio::sync::{Semaphore, mpsc};

use crate::{audio::Audio, solver::Solver, yt::{Like, Song}};

slint::include_modules!();

/// Refill the radio when fewer than this many songs are left after the current one.
const REFILL_BELOW: usize = 15;
/// Decoded album art kept in memory (small thumbnails, ~57 KB each at 120 px).
const ART_CACHE_MAX: usize = 300;
/// Songs heard recently (persisted) are skipped when they come up in a radio again.
const HISTORY_LEN: usize = 500;
/// Stop chasing continuations if this many pages in a row add nothing new.
const MAX_EMPTY_PAGES: u32 = 4;
/// Pages of a playlist taken by "Play next" / "Add to queue" (100 songs each).
const MAX_ADD_PAGES: u32 = 20;

enum Ev {
    Search(String),
    OpenRow(usize),
    Jump(usize),
    TogglePlay,
    Next,
    Prev,
    Seek(f32),
    Volume(f32),
    // results of background work
    Connected(Result<Arc<yt::Client>, String>),
    SearchDone(u64, Result<Vec<Song>, String>),
    RadioPage(u64, Result<yt::RadioPage, String>),
    ListPage(u64, Result<(Vec<Song>, Option<String>), String>),
    /// Home shelves and the continuation for more (`true`: a further page, appended).
    HomeLoaded(Result<(Vec<yt::Shelf>, Option<String>), String>, bool),
    /// The browse list was scrolled near its end.
    NearEnd,
    CollectionLoaded(u64, String, Result<yt::Collection, String>),
    ShowView(i32),
    PlayAll,
    /// Shuffle-play the open album/playlist (header button).
    ShuffleAll,
    /// Context-menu action on a row: list 0 = browse panel, 1 = queue.
    RowAction(i32, usize, String),
    /// An album/playlist fetched for a context-menu action (all pages for next/queue).
    RowCollection(String, &'static str, Result<yt::Collection, String>),
    Back,
    Loaded(u64, String, Result<Track, String>),
    Prefetched(String, Result<Track, String>),
    ArtLoaded(String, Vec<String>),
    Tick,
    // sign-in
    NeedSignIn(String),
    ChooseCookies,
    ImportPicked(Option<PathBuf>),
    Imported(Result<(Arc<yt::Client>, String), String>),
    SignOut,
    SignedOutDone(Result<(), String>),
    Mpris(mpris::Command),
    /// Versions found for a song (`true`: from a search by hand).
    LyricsLoaded(String, Vec<lyrics::Lyrics>, bool),
    /// "Wrong lyrics?": search by hand (title, artist) / pick a version (-1: no lyrics).
    LyricsSearch(String, String),
    LyricsPick(i32),
    /// The Settings page changed something (the new values).
    SettingsChanged(settings::Settings),
    // updates
    /// Look for a new version (`true`: asked for on the Settings page).
    UpdateCheck(bool),
    UpdateChecked(Result<Option<update::Release>, String>, bool),
    UpdateInstall,
    UpdateInstalled(Result<String, String>),
    /// Quit and start the (updated) app again.
    Restart,
    Speed(f32),
    ViewArt(String),
    ArtFull(u64, Option<Pixels>),
    /// App is closing: finish the play report, save state, then reply.
    Shutdown(tokio::sync::oneshot::Sender<()>),
    // likes
    LikeClicked(Like),
    LikeStatus(String, Result<Like, String>),
    LikeSet(String, Like, Result<(), String>),
    // artists
    ArtistLoaded(u64, String, Result<yt::Artist, String>),
    /// Artist page header buttons: "shuffle", "mix", "all".
    ArtistButton(String),
    /// Open the playing song's artist (click on the artist name in the player bar).
    GoToPlayingArtist,
    LibraryLoaded(Result<yt::Library, String>),
}

const LIKED: &str = "VLLM";
/// Set when the app should start again after closing (after an update).
static RESTART: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Where the code and the releases live (shown in Settings; updates come from its releases).
fn repo_url() -> String {
    format!("https://github.com/{}", update::REPO)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Home,
    Liked,
    Search,
    Collection,
    Lyrics,
    Artist,
    Settings,
    Library,
}

/// A page to go back to.
#[derive(Clone)]
enum Page {
    Main(View),
    Collection(String, &'static str),
    Artist(String),
}

/// A row in the left panel.
#[derive(Clone)]
enum Row {
    Header(String),
    Item(yt::Item),
}

/// Where the queue's songs come from, and how to get more.
enum Source {
    /// A radio from a seed song (endless; skips recently heard songs).
    /// `watch`: started from a playlist YouTube builds (an artist's Mix/Shuffle); its songs
    /// aren't filtered by the repeat history.
    Radio { seed_id: String, continuation: Option<String>, watch: Option<yt::Watch> },
    /// An album or playlist, played in order; when it runs out, a radio continues
    /// from its last song (like YT Music's autoplay).
    /// `shuffle`: pages are fetched right away and mixed into the rest of the queue.
    List { title: String, continuation: Option<String>, artist: String, thumb: Option<String>, shuffle: bool },
}

struct Radio {
    /// Badge shown in the queue card: RADIO / PLAYLIST / ALBUM.
    badge: String,
    seed_title: String,
    source: Source,
    pages: u32,
    skipped: u32,
    empty_streak: u32,
    fetching: bool,
    ended: bool,
    error: String,
    generation: u64,
}

struct History {
    path: PathBuf,
    ids: VecDeque<String>,
}

impl History {
    fn load(path: PathBuf) -> Self {
        let ids = std::fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        Self { path, ids }
    }
    fn contains(&self, id: &str) -> bool {
        self.ids.iter().any(|x| x == id)
    }
    fn push(&mut self, id: &str) {
        self.ids.retain(|x| x != id);
        self.ids.push_back(id.to_string());
        while self.ids.len() > HISTORY_LEN {
            self.ids.pop_front();
        }
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&self.path, serde_json::to_string(&self.ids).unwrap_or_default());
    }
}

type Pixels = SharedPixelBuffer<Rgba8Pixel>;

/// Bounded cache of decoded art, oldest evicted first.
#[derive(Default)]
struct ArtCache {
    map: HashMap<String, Pixels>,
    order: VecDeque<String>,
}

impl ArtCache {
    fn get(&self, k: &str) -> Option<Pixels> {
        self.map.get(k).cloned()
    }
    /// Returns evicted keys (so they can be fetched again later).
    fn insert(&mut self, k: String, v: Pixels) -> Vec<String> {
        if self.map.insert(k.clone(), v).is_none() {
            self.order.push_back(k);
        }
        let mut evicted = Vec::new();
        while self.order.len() > ART_CACHE_MAX {
            if let Some(old) = self.order.pop_front() {
                self.map.remove(&old);
                evicted.push(old);
            }
        }
        evicted
    }
}

struct App {
    ui: slint::Weak<MainWindow>,
    tx: mpsc::UnboundedSender<Ev>,
    yt: Option<Arc<yt::Client>>,
    audio: Option<Audio>,
    results: Vec<Song>,
    view: View,
    /// Pages to go back to (artist → album → back …). The sidebar starts over.
    back: Vec<Page>,
    artist: Option<(String, yt::Artist)>,
    /// Rows currently shown in the left panel (for click handling).
    rows: Vec<Row>,
    home: Option<Vec<yt::Shelf>>,
    home_error: String,
    /// Continuation for more Home shelves, and whether a page is being fetched.
    home_next: Option<String>,
    home_more_loading: bool,
    liked: Option<yt::Collection>,
    /// Saved playlists, albums, songs and artists (Library page); reloaded when it's opened.
    library: Option<yt::Library>,
    library_error: String,
    library_loading: bool,
    collection: Option<(String, yt::Collection)>,
    collection_badge: &'static str,
    browse_gen: u64,
    browse_error: String,
    queue: Vec<Song>,
    current: Option<usize>,
    /// End (exclusive) of the songs added by hand after the current one (Play next / Add to
    /// queue). They play before the radio/list songs, which continue from here.
    manual_end: usize,
    played: HashSet<String>,
    radio: Option<Radio>,
    history: History,
    art: Arc<std::sync::Mutex<ArtCache>>,
    art_pending: HashSet<String>,
    art_limit: Arc<Semaphore>,
    prefetch: HashMap<String, Track>,
    /// Bytes of the song that's playing (watched for a failed download).
    playing_stream: Option<Arc<audio::Stream>>,
    /// When the current song was handed to the audio engine (logs how fast sound starts).
    load_started: Option<std::time::Instant>,
    /// Last seen change time of the KDE settings (colors, font, icons).
    kde_changed_at: Option<std::time::SystemTime>,
    prefetching: HashSet<String>,
    search_gen: u64,
    play_gen: u64,
    radio_gen: u64,
    loading: bool,
    paused: bool,
    duration: f64,
    pending_advance: bool,
    report: Option<report::Session>,
    finder: Option<Arc<lyrics::Finder>>,
    /// Lyrics per video id (None = looked, nothing found). Small; cleared when large.
    lyrics: HashMap<String, Option<Arc<lyrics::Lyrics>>>,
    lyrics_pending: HashSet<String>,
    settings: settings::Settings,
    updater: Option<Arc<update::Updater>>,
    /// A newer release (found by the last check), and what the updater is doing.
    update: Option<update::Release>,
    update_busy: bool,
    update_ready: bool,
    /// Every version found per song (the one shown is in `lyrics`); memory only.
    lyrics_found: HashMap<String, Vec<Arc<lyrics::Lyrics>>>,
    lyrics_searching: bool,
    mpris: Option<mpsc::UnboundedSender<mpris::Update>>,
    /// Last playback status sent to MPRIS (playing, has_next, has_prev).
    mpris_status: Option<(bool, bool, bool)>,
    volume: f32,
    speed: f32,
    /// Bumped per image-viewer open, so a late high-res download doesn't replace a newer one.
    lightbox_gen: u64,
    /// Restored state waiting for the first sign-in.
    saved: Option<state::Saved>,
    last_saved: Option<state::Saved>,
    /// Seek here once the current song loads (resuming after a restart).
    resume_at: Option<f64>,
    signed_out: bool,
    solver: Solver,
    /// Login came from the encrypted store (save YouTube's cookie updates back to it).
    persist: bool,
    importing: bool,
    ticks: u64,
    /// Consecutive songs that failed to load (for reasons other than sign-out).
    fail_streak: u32,
    /// Like status of the current song (None while unknown).
    like: Option<(String, Like)>,
    /// Dev smoke test (YTM_SMOKE=<query>): search, start a radio, play, screenshot, quit.
    smoke: Option<String>,
    smoke_seek_done: bool,
}

fn fmt_time(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    format!("{}:{:02}", s / 60, s % 60)
}

fn parse_duration(d: &str) -> f64 {
    d.split(':').fold(0.0, |acc, p| acc * 60.0 + p.trim().parse::<f64>().unwrap_or(0.0))
}

/// Ask Google's image CDN for a sharper version of a thumbnail.
fn sized_thumb(url: &str, px: u32) -> String {
    let re = regex::Regex::new(r"=w\d+-h\d+").unwrap();
    if re.is_match(url) { re.replace(url, format!("=w{px}-h{px}").as_str()).into_owned() } else { url.to_string() }
}

#[derive(Clone)]
struct RowData {
    /// 0 header, 1 song, 2 album, 3 playlist, 4 artist
    kind: i32,
    song: Song,
    art: Option<Pixels>,
    current: bool,
    played: bool,
}

fn to_row(r: &RowData) -> SongRow {
    SongRow {
        id: r.song.id.as_str().into(),
        title: r.song.title.as_str().into(),
        artist: r.song.artist.as_str().into(),
        album: r.song.album.as_str().into(),
        duration: r.song.duration.as_str().into(),
        art: r.art.clone().map(slint::Image::from_rgba8).unwrap_or_default(),
        has_art: r.art.is_some(),
        current: r.current,
        played: r.played,
        kind: r.kind,
        has_artist: r.song.artist_id.is_some(),
        has_album: r.song.album_id.is_some(),
    }
}

/// Update a model in place (keeps the list's scroll position).
fn sync_model(model: ModelRc<SongRow>, rows: &[RowData]) {
    let Some(vm) = model.as_any().downcast_ref::<VecModel<SongRow>>() else { return };
    for (i, r) in rows.iter().enumerate() {
        let new = to_row(r);
        if i < vm.row_count() {
            let old = vm.row_data(i).unwrap();
            if old.id != new.id || old.kind != new.kind || old.current != new.current || old.played != new.played || old.has_art != new.has_art {
                vm.set_row_data(i, new);
            }
        } else {
            vm.push(new);
        }
    }
    while vm.row_count() > rows.len() {
        vm.remove(vm.row_count() - 1);
    }
}

impl App {
    fn ui(&self, f: impl FnOnce(&MainWindow) + Send + 'static) {
        let ui = self.ui.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui.upgrade() {
                f(&ui);
            }
        });
    }

    fn art_for(&mut self, url: &Option<String>, px: u32) -> Option<Pixels> {
        let url = sized_thumb(url.as_ref()?, px);
        if let Some(p) = self.art.lock().unwrap().get(&url) {
            return Some(p);
        }
        let Some(yt) = self.yt.clone() else { return None };
        if self.art_pending.insert(url.clone()) {
            let (tx, cache, limit) = (self.tx.clone(), self.art.clone(), self.art_limit.clone());
            tokio::spawn(async move {
                let _permit = limit.acquire().await;
                if let Ok(bytes) = yt.image(&url).await {
                    let decoded = tokio::task::spawn_blocking(move || {
                        // Shrink to display size: radio thumbnails can be 544 px or more.
                        let img = image::load_from_memory(&bytes).ok()?;
                        let img = if img.width() > px * 2 || img.height() > px * 2 { img.thumbnail(px * 2, px * 2) } else { img };
                        let img = img.to_rgba8();
                        Some(Pixels::clone_from_slice(img.as_raw(), img.width(), img.height()))
                    }).await.ok().flatten();
                    if let Some(p) = decoded {
                        let evicted = cache.lock().unwrap().insert(url.clone(), p);
                        let _ = tx.send(Ev::ArtLoaded(url, evicted));
                    }
                }
            });
        }
        None
    }

    /// The page shown now, as something to come back to.
    fn current_page(&self) -> Page {
        match self.view {
            View::Collection => Page::Collection(self.collection.as_ref().map(|(id, _)| id.clone()).unwrap_or_default(), self.collection_badge),
            View::Artist => Page::Artist(self.artist.as_ref().map(|(id, _)| id.clone()).unwrap_or_default()),
            v => Page::Main(v),
        }
    }

    /// Top-level views (sidebar): the back history starts over.
    fn show_view(&mut self, view: View) {
        if view != View::Lyrics {
            self.back.clear();
        }
        self.go(Page::Main(view));
    }

    fn open_collection(&mut self, id: String, badge: &'static str) {
        self.back.push(self.current_page());
        self.go(Page::Collection(id, badge));
    }

    fn open_artist(&mut self, id: String) {
        self.back.push(self.current_page());
        self.go(Page::Artist(id));
    }

    fn go_back(&mut self) {
        let page = self.back.pop().unwrap_or(Page::Main(View::Home));
        self.go(page);
    }

    fn go(&mut self, page: Page) {
        self.browse_error.clear();
        match page {
            Page::Main(view) => {
                self.view = view;
                if view == View::Liked && self.liked.is_none() {
                    self.load_collection(LIKED.to_string());
                }
                if view == View::Library {
                    self.load_library();
                }
            }
            Page::Collection(id, badge) => {
                self.view = View::Collection;
                self.collection_badge = badge;
                if self.collection.as_ref().is_none_or(|(cid, _)| *cid != id) {
                    self.collection = None;
                    self.load_collection(id);
                }
            }
            Page::Artist(id) => {
                self.view = View::Artist;
                if self.artist.as_ref().is_none_or(|(aid, _)| *aid != id) {
                    self.artist = None;
                    self.load_artist(id);
                }
            }
        }
        self.push_browse();
    }

    fn load_artist(&mut self, id: String) {
        let Some(yt) = self.yt.clone() else { return };
        self.browse_gen += 1;
        let (generation, tx) = (self.browse_gen, self.tx.clone());
        tokio::spawn(async move {
            let res = yt.artist(&id).await.map_err(|e| e.to_string());
            let _ = tx.send(Ev::ArtistLoaded(generation, id, res));
        });
    }

    /// Play an artist's Mix or Shuffle: the queue starts with the first page YouTube returns.
    fn start_watch(&mut self, title: String, badge: &str, watch: yt::Watch) {
        self.radio_gen += 1;
        self.radio = Some(Radio {
            badge: badge.into(),
            seed_title: title,
            source: Source::Radio { seed_id: watch.video_id.clone(), continuation: None, watch: Some(watch) },
            pages: 0,
            skipped: 0,
            empty_streak: 0,
            fetching: false,
            ended: false,
            error: String::new(),
            generation: self.radio_gen,
        });
        // Stop what's playing; the first page decides the first song.
        self.play_gen += 1;
        self.finish_report();
        if let Some(a) = &self.audio {
            a.stop();
        }
        self.queue.clear();
        self.current = None;
        self.manual_end = 0;
        self.played.clear();
        self.loading = true;
        self.push_now();
        self.ui(|ui| ui.set_loading_text("Getting the mix…".into()));
        self.fetch_radio();
    }

    fn load_collection(&mut self, id: String) {
        let Some(yt) = self.yt.clone() else { return };
        self.browse_gen += 1;
        let (generation, tx) = (self.browse_gen, self.tx.clone());
        tokio::spawn(async move {
            let res = yt.collection(&id).await.map_err(|e| e.to_string());
            let _ = tx.send(Ev::CollectionLoaded(generation, id, res));
        });
    }

    /// (Re)load the Library: it changes whenever something is saved, here or elsewhere.
    fn load_library(&mut self) {
        let Some(yt) = self.yt.clone() else { return };
        if self.library_loading {
            return;
        }
        self.library_loading = true;
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let res = yt.library().await.map_err(|e| e.to_string());
            let _ = tx.send(Ev::LibraryLoaded(res));
        });
    }

    /// The Library's saved songs as a list to play.
    fn library_songs(&self) -> Option<yt::Collection> {
        let l = self.library.as_ref()?;
        Some(yt::Collection {
            title: "Songs in your library".into(),
            subtitle: String::new(),
            thumb: l.songs.first().and_then(|s| s.thumb.clone()),
            artist: String::new(),
            tracks: l.songs.clone(),
            continuation: None,
        })
    }

    fn load_home(&mut self) {
        let Some(yt) = self.yt.clone() else { return };
        self.home_next = None;
        let tx = self.tx.clone();
        tokio::spawn(async move {
            // First page plus one more (Quick picks, mixes and such live further down); the rest
            // loads as the list is scrolled.
            let res: Result<(Vec<yt::Shelf>, Option<String>)> = async {
                let (mut shelves, mut next) = yt.home(None).await?;
                if let Some(c) = next.take() {
                    let (more, n) = yt.home(Some(&c)).await?;
                    shelves.extend(more);
                    next = n;
                }
                Ok((shelves, next))
            }.await;
            let _ = tx.send(Ev::HomeLoaded(res.map_err(|e| e.to_string()), false));
        });
    }

    /// Next page of Home shelves (when the list is scrolled near its end).
    fn load_more_home(&mut self) {
        let (Some(yt), Some(next)) = (self.yt.clone(), self.home_next.clone()) else { return };
        if self.home_more_loading {
            return;
        }
        self.home_more_loading = true;
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let res = yt.home(Some(&next)).await.map_err(|e| e.to_string());
            let _ = tx.send(Ev::HomeLoaded(res, true));
        });
    }

    fn open_row(&mut self, i: usize) {
        let Some(Row::Item(item)) = self.rows.get(i).cloned() else { return };
        match item.kind {
            yt::Kind::Song => match self.view {
                View::Liked | View::Collection => {
                    let c = if self.view == View::Liked { self.liked.as_ref() } else { self.collection.as_ref().map(|(_, c)| c) };
                    let Some(c) = c else { return };
                    let c = c.clone();
                    let badge = if self.view == View::Liked { "PLAYLIST" } else { self.collection_badge };
                    let index = c.tracks.iter().position(|t| t.id == item.song.id).unwrap_or(0);
                    self.play_list(&c, badge, index, false);
                }
                View::Library => {
                    let Some(c) = self.library_songs() else { return };
                    let index = c.tracks.iter().position(|t| t.id == item.song.id).unwrap_or(0);
                    self.play_list(&c, "PLAYLIST", index, false);
                }
                _ => self.start_radio(item.song),
            },
            yt::Kind::Album => self.open_collection(item.song.id, "ALBUM"),
            // Liked Music has its own page.
            yt::Kind::Playlist if item.song.id == LIKED => self.show_view(View::Liked),
            yt::Kind::Playlist => self.open_collection(item.song.id, "PLAYLIST"),
            yt::Kind::Artist => self.open_artist(item.song.id),
        }
    }

    fn push_browse(&mut self) {
        let song_item = |s: &Song| Row::Item(yt::Item { kind: yt::Kind::Song, song: s.clone() });
        let (rows, hint, loading): (Vec<Row>, String, bool) = match self.view {
            View::Home => match &self.home {
                Some(shelves) => (
                    shelves.iter().flat_map(|sh| {
                        std::iter::once(Row::Header(sh.title.clone()))
                            .chain(sh.items.iter().cloned().map(Row::Item))
                    }).collect(),
                    String::new(),
                    false,
                ),
                None if !self.home_error.is_empty() => (vec![], self.home_error.clone(), false),
                None => (vec![], "Loading your Home…".into(), true),
            },
            View::Search => (self.results.iter().map(song_item).collect(),
                             "Search for a song, then click it to start a radio from it.".into(), false),
            View::Lyrics | View::Settings => (vec![], String::new(), false),
            View::Library => match &self.library {
                Some(l) => {
                    let song_items: Vec<yt::Item> = l.songs.iter().map(|s| yt::Item { kind: yt::Kind::Song, song: s.clone() }).collect();
                    let sections = [("Playlists", &l.playlists), ("Albums", &l.albums), ("Songs", &song_items), ("Artists", &l.artists)];
                    (
                        sections.into_iter().filter(|(_, items)| !items.is_empty()).flat_map(|(title, items)| {
                            std::iter::once(Row::Header(title.into())).chain(items.iter().cloned().map(Row::Item))
                        }).collect(),
                        "Nothing saved yet. Albums, playlists, songs and artists you save on YouTube Music show up here.".into(),
                        false,
                    )
                }
                None if !self.library_error.is_empty() => (vec![], self.library_error.clone(), false),
                None => (vec![], "Loading your library…".into(), true),
            },
            View::Artist => match &self.artist {
                Some((_, a)) => (
                    a.shelves.iter().flat_map(|sh| {
                        std::iter::once(Row::Header(sh.title.clone())).chain(sh.items.iter().cloned().map(Row::Item))
                    }).collect(),
                    "Nothing here.".into(),
                    false,
                ),
                None if !self.browse_error.is_empty() => (vec![], self.browse_error.clone(), false),
                None => (vec![], "Loading…".into(), true),
            },
            View::Liked | View::Collection => {
                let c = if self.view == View::Liked { self.liked.as_ref() } else { self.collection.as_ref().map(|(_, c)| c) };
                match c {
                    Some(c) => (c.tracks.iter().map(song_item).collect(), "This list is empty.".into(), false),
                    None if !self.browse_error.is_empty() => (vec![], self.browse_error.clone(), false),
                    None => (vec![], "Loading…".into(), true),
                }
            }
        };
        let header = match self.view {
            View::Liked => self.liked.as_ref(),
            View::Collection => self.collection.as_ref().map(|(_, c)| c),
            _ => None,
        };
        let artist = self.artist.as_ref().filter(|_| self.view == View::Artist).map(|(_, a)| a);
        let show_header = header.is_some() || artist.is_some();
        let (h_title, h_sub, h_thumb) = match (header, artist) {
            (Some(c), _) => {
                let more = if c.continuation.is_some() { " · more load as it plays" } else { "" };
                (c.title.clone(), format!("{}{more}", c.subtitle), c.thumb.clone())
            }
            (None, Some(a)) => (a.name.clone(), a.subtitle.clone(), a.thumb.clone()),
            _ => Default::default(),
        };
        let artist_buttons = artist.map_or((false, false, false), |a| (a.shuffle.is_some(), a.mix.is_some(), a.all_songs.is_some()));
        let can_back = !self.back.is_empty();
        let h_art = self.art_for(&h_thumb, 300);
        let data: Vec<RowData> = rows.iter().map(|r| match r {
            Row::Header(t) => RowData { kind: 0, song: Song { id: format!("hdr:{t}"), title: t.clone(), ..Default::default() }, art: None, current: false, played: false },
            Row::Item(it) => {
                let art = self.art_for(&it.song.thumb, 120);
                let kind = match it.kind { yt::Kind::Song => 1, yt::Kind::Album => 2, yt::Kind::Playlist => 3, yt::Kind::Artist => 4 };
                let mut song = it.song.clone();
                if it.kind == yt::Kind::Album {
                    // The label on the right: "Single" / "EP" from a subtitle like "Single • 2012".
                    song.duration = match song.artist.split(" • ").next() { Some(k @ ("Single" | "EP")) => k.into(), _ => "Album".into() };
                }
                RowData { kind, song, art, current: false, played: false }
            }
        }).collect();
        self.rows = rows;
        let view = match self.view { View::Home => 0, View::Liked => 1, View::Search => 2, View::Collection => 3, View::Lyrics => 4, View::Artist => 5, View::Settings => 6, View::Library => 7 };
        self.ui(move |ui| {
            sync_model(ui.get_browse(), &data);
            ui.set_view(view);
            ui.set_browse_hint(hint.into());
            ui.set_browse_loading(loading);
            ui.set_coll_visible(show_header);
            ui.set_coll_title(h_title.into());
            ui.set_coll_subtitle(h_sub.into());
            ui.set_coll_has_art(h_art.is_some());
            ui.set_coll_art(h_art.map(slint::Image::from_rgba8).unwrap_or_default());
            ui.set_can_back(can_back);
            ui.set_artist_has_shuffle(artist_buttons.0);
            ui.set_artist_has_mix(artist_buttons.1);
            ui.set_artist_has_all(artist_buttons.2);
        });
    }

    fn push_queue(&mut self) {
        let songs = self.queue.clone();
        let current = self.current;
        let rows: Vec<RowData> = songs.into_iter().enumerate().map(|(i, s)| {
            let art = self.art_for(&s.thumb, 120);
            let played = current.is_some_and(|c| i < c) || self.played.contains(&s.id);
            RowData { kind: 1, current: Some(i) == current, played, song: s, art }
        }).collect();
        let upcoming = current.map_or(self.queue.len(), |c| self.queue.len().saturating_sub(c + 1)) as i32;
        let (active, seed, pages, skipped, fetching, ended, error, badge, is_list) = match &self.radio {
            Some(r) => (true, r.seed_title.clone(), r.pages as i32, r.skipped as i32, r.fetching, r.ended, r.error.clone(),
                        r.badge.clone(), matches!(r.source, Source::List { .. })),
            None => (false, String::new(), 0, 0, false, false, String::new(), "RADIO".to_string(), false),
        };
        let current_row = current.map_or(-1, |c| c as i32);
        let shuffled = matches!(self.radio.as_ref().map(|r| &r.source), Some(Source::List { shuffle: true, .. }));
        self.ui(move |ui| {
            sync_model(ui.get_queue(), &rows);
            ui.set_queue_current(current_row);
            ui.set_radio_shuffled(shuffled);
            ui.set_radio_active(active);
            ui.set_radio_seed(seed.into());
            ui.set_radio_upcoming(upcoming);
            ui.set_radio_pages(pages);
            ui.set_radio_skipped(skipped);
            ui.set_radio_fetching(fetching);
            ui.set_radio_ended(ended);
            ui.set_radio_error(error.into());
            ui.set_radio_badge(badge.into());
            ui.set_radio_is_list(is_list);
        });
    }

    fn push_now(&mut self) {
        let song = self.current.and_then(|c| self.queue.get(c)).cloned();
        let art = song.as_ref().and_then(|s| self.art_for(&s.thumb, 300));
        let (loading, paused) = (self.loading, self.paused);
        self.ui(move |ui| {
            ui.set_has_current(song.is_some());
            ui.set_now_title(song.as_ref().map_or("Nothing playing".into(), |s| s.title.as_str().into()));
            ui.set_now_artist(song.as_ref().map_or(SharedString::new(), |s| s.artist.as_str().into()));
            ui.set_now_artist_link(song.as_ref().is_some_and(|s| s.artist_id.is_some()));
            ui.set_now_has_art(art.is_some());
            ui.set_now_art(art.map(slint::Image::from_rgba8).unwrap_or_default());
            ui.set_loading(loading);
            ui.set_playing(!loading && !paused && song.is_some());
        });
    }

    /// YouTube rejected the login: stop everything that would keep failing, and say so.
    fn mark_signed_out(&mut self) {
        if self.signed_out {
            return;
        }
        self.signed_out = true;
        self.pending_advance = false;
        if let Some(r) = self.radio.as_mut() {
            r.error = "Paused: signed out.".into();
        }
        self.push_queue();
        self.set_error("YouTube signed this app out (the imported login stopped working). Re-import cookies and restart.".into());
        self.ui(|ui| {
            ui.set_account_ok(false);
            ui.set_account_text("Signed out by YouTube".into());
            ui.set_show_sign_in(true);
            ui.set_sign_in_status("YouTube no longer accepts the stored login. Import a fresh one (steps above).".into());
        });
        eprintln!("[auth] signed out: YouTube answered 401");
    }

    fn request_lyrics(&mut self, song: &Song) {
        if self.lyrics.contains_key(&song.id) || !self.lyrics_pending.insert(song.id.clone()) {
            return;
        }
        let Some(finder) = self.finder.clone() else { return };
        let (tx, s, dur) = (self.tx.clone(), song.clone(), self.duration);
        tokio::spawn(async move {
            let found = finder.find_all(&s.title, &s.artist, &s.album, dur, Some(&s.id)).await;
            let _ = tx.send(Ev::LyricsLoaded(s.id, found, false));
        });
    }

    /// Show the current song's lyrics (or their loading/none state).
    fn push_lyrics(&self) {
        let id = self.current_id();
        let found = id.as_ref().and_then(|i| self.lyrics.get(i)).cloned();
        let looking = id.as_ref().is_some_and(|i| self.lyrics_pending.contains(i));
        type Plain = Vec<(Vec<Vec<lyrics::TimedChar>>, f32, f32, bool, bool, String)>;
        let has_translation = found.as_ref().is_some_and(|l| l.as_ref().is_some_and(|l| l.has_translation()));
        let (plain, sync, status): (Plain, i32, String) = match found {
            Some(Some(l)) => {
                let lines = l.lines.iter().map(|ln| (lyrics::line_units(ln), ln.start as f32, ln.end as f32, ln.opposite, ln.background,
                    ln.translation.clone().unwrap_or_default())).collect();
                let sync = match l.sync {
                    lyrics::Sync::Syllable => 1,
                    lyrics::Sync::Line => 2,
                    lyrics::Sync::Static => 3,
                };
                (lines, sync, format!("Lyrics from {} · {}", l.source, l.kind()))
            }
            Some(None) if id.as_ref().is_some_and(|i| self.lyrics_found.get(i).is_some_and(|v| !v.is_empty())) =>
                (vec![], 3, "Lyrics hidden for this song.".into()),
            Some(None) => (vec![], 3, "No lyrics found (LRCLIB, lrc.red, BetterLyrics).".into()),
            None if looking => (vec![], 0, "Looking for lyrics…".into()),
            None => (vec![], 0, if id.is_some() { "Lyrics load when the song starts.".into() } else { String::new() }),
        };
        // Slint models aren't Send: they're built on the UI thread from plain data.
        self.ui(move |ui| {
            let model: Vec<LyricLine> = plain.into_iter().map(|(units, start, end, opposite, background, translation)| LyricLine {
                text: units.iter().flatten().map(|c| c.0.as_str()).collect::<String>().into(),
                words: ModelRc::new(VecModel::from(units.into_iter().map(|u| {
                    // The piece is sung from its first character's start to its last sung character's end.
                    let sung: Vec<&lyrics::TimedChar> = u.iter().filter(|c| !c.0.trim().is_empty()).collect();
                    LyricWord {
                        text: u.iter().map(|c| c.0.as_str()).collect::<String>().into(),
                        start: sung.first().map_or(start, |c| c.1),
                        end: sung.last().map_or(end, |c| c.2),
                    }
                }).collect::<Vec<_>>())),
                start, end, opposite, background,
                translation: translation.into(),
            }).collect();
            ui.set_lyrics(ModelRc::new(VecModel::from(model)));
            ui.set_lyrics_sync(sync);
            ui.set_lyrics_status(status.into());
            ui.set_lyrics_has_translation(has_translation);
        });
        self.push_lyric_options();
    }

    /// Update state for the Settings page (and the toolbar notice).
    fn push_update_status(&self, status: String) {
        let available = self.update.is_some() && !self.update_ready && update::mode() == update::Mode::SelfUpdate;
        let notice = match (&self.update, self.update_ready) {
            (Some(r), true) => format!("Version {} installed", r.version),
            (Some(r), false) if update::mode() != update::Mode::SelfUpdate => format!("Version {} available", r.version),
            _ => String::new(),
        };
        let (busy, ready) = (self.update_busy, self.update_ready);
        self.ui(move |ui| {
            ui.set_update_status(status.into());
            ui.set_update_busy(busy);
            ui.set_update_available(available);
            ui.set_update_ready(ready);
            ui.set_update_notice(notice.into());
        });
    }

    /// The "Wrong lyrics?" list: every version found for the current song.
    fn push_lyric_options(&self) {
        let id = self.current_id();
        let shown = id.as_ref().and_then(|i| self.lyrics.get(i)).cloned().flatten();
        let options: Vec<(String, String, bool)> = id.as_ref().and_then(|i| self.lyrics_found.get(i)).into_iter().flatten()
            .map(|l| (format!("{} · {}", l.source, l.kind()), l.preview(), shown.as_ref().is_some_and(|s| Arc::ptr_eq(s, l))))
            .collect();
        let hidden = id.as_ref().is_some_and(|i| matches!(self.lyrics.get(i), Some(None)));
        let song = self.current.and_then(|c| self.queue.get(c)).map(|s| (s.title.clone(), lyrics::main_artist(&s.artist)));
        let searching = self.lyrics_searching;
        self.ui(move |ui| {
            let model: Vec<LyricOption> = options.into_iter()
                .map(|(label, preview, current)| LyricOption { label: label.into(), preview: preview.into(), current }).collect();
            ui.set_lyric_options(ModelRc::new(VecModel::from(model)));
            ui.set_lyrics_hidden(hidden);
            ui.set_lyrics_searching(searching);
            if let Some((title, artist)) = song {
                ui.set_lyrics_song_title(title.into());
                ui.set_lyrics_song_artist(artist.into());
            }
        });
    }

    fn to_mpris(&self, u: mpris::Update) {
        if let Some(m) = &self.mpris {
            let _ = m.send(u);
        }
    }

    fn send_track_meta(&self, song: &Song) {
        self.to_mpris(mpris::Update::Track {
            n: self.play_gen,
            title: song.title.clone(),
            artist: song.artist.clone(),
            album: song.album.clone(),
            art_url: song.thumb.as_deref().map(|t| sized_thumb(t, 544)),
            length: self.duration,
        });
    }

    fn on_mpris(&mut self, cmd: mpris::Command) {
        let pos = self.audio.as_ref().map_or(0.0, |a| a.status().position);
        match cmd {
            mpris::Command::PlayPause => self.handle(Ev::TogglePlay),
            mpris::Command::Play => {
                let loaded = self.audio.as_ref().is_some_and(|a| a.status().loaded);
                if self.paused || !loaded {
                    self.handle(Ev::TogglePlay);
                }
            }
            mpris::Command::Pause => {
                if !self.paused && self.audio.as_ref().is_some_and(|a| a.status().loaded) {
                    self.handle(Ev::TogglePlay);
                }
            }
            mpris::Command::Next => self.next(),
            mpris::Command::Previous => self.prev(),
            mpris::Command::SeekBy(d) => {
                let to = (pos + d).clamp(0.0, self.duration.max(0.0));
                self.handle(Ev::Seek(to as f32));
            }
            mpris::Command::SetPosition(id, p) => {
                if id == mpris::track_id_string(self.play_gen) {
                    self.handle(Ev::Seek(p as f32));
                }
            }
            mpris::Command::Volume(v) => {
                let v = v.clamp(0.0, 1.0) as f32;
                self.handle(Ev::Volume(v));
                self.ui(move |ui| ui.set_volume(v));
            }
            mpris::Command::Raise => {
                self.ui(|ui| {
                    let _ = ui.show();
                });
            }
        }
    }

    /// Restore the queue from the last session (paused; press play to continue).
    fn restore(&mut self) {
        let Some(saved) = self.saved.take() else { return };
        if !self.queue.is_empty() || saved.queue.is_empty() {
            return;
        }
        let Some(cur) = saved.current.filter(|c| *c < saved.queue.len()) else { return };
        self.queue = saved.queue;
        self.current = Some(cur);
        self.manual_end = cur + 1;
        self.resume_at = Some(saved.position);
        self.paused = true;
        if let Some(src) = saved.source {
            self.radio_gen += 1;
            self.radio = Some(Radio {
                badge: src.badge,
                seed_title: src.title.clone(),
                source: match src.radio_seed {
                    Some(seed_id) => Source::Radio { seed_id, continuation: None, watch: None },
                    None => Source::List { title: src.title, continuation: None, artist: String::new(), thumb: None, shuffle: false },
                },
                pages: 0,
                skipped: 0,
                empty_streak: 0,
                fetching: false,
                ended: false,
                error: String::new(),
                generation: self.radio_gen,
            });
        }
        let song = self.queue[cur].clone();
        self.duration = parse_duration(&song.duration);
        let (pos, dur) = (saved.position, self.duration);
        self.ui(move |ui| {
            ui.set_position(pos as f32);
            ui.set_pos_text(fmt_time(pos).into());
            ui.set_duration(dur as f32);
            ui.set_dur_text(fmt_time(dur).into());
            ui.set_loading_text("".into());
        });
        self.send_track_meta(&song);
        self.to_mpris(mpris::Update::Position(pos));
        self.push_queue();
        self.push_now();
        eprintln!("[state] restored queue of {} at #{cur}, {pos:.0}s", self.queue.len());
        if self.smoke.as_deref() == Some("resume") {
            let _ = self.tx.send(Ev::TogglePlay);
            smoke_screenshot(self.ui.clone(), 10);
        }
    }

    fn snapshot(&self) -> state::Saved {
        let position = match &self.audio {
            Some(a) if a.status().loaded => a.status().position,
            _ => self.resume_at.unwrap_or(0.0),
        };
        state::Saved {
            volume: self.volume,
            speed: self.speed,
            queue: self.queue.clone(),
            current: self.current,
            position: (position * 10.0).round() / 10.0,
            source: self.radio.as_ref().map(|r| state::SavedSource {
                badge: r.badge.clone(),
                title: r.seed_title.clone(),
                radio_seed: match &r.source {
                    Source::Radio { seed_id, .. } => Some(seed_id.clone()),
                    Source::List { .. } => None,
                },
            }),
        }
    }

    /// Save if anything changed (or always when `force`).
    fn save_state(&mut self, force: bool) {
        if std::env::var_os("YTM_SMOKE").is_some() && std::env::var_os("YTM_SMOKE_STATE").is_none() {
            return; // smoke tests leave the real session alone
        }
        if self.saved.is_some() {
            return; // not restored yet: don't overwrite the last session with an empty one
        }
        let snap = self.snapshot();
        if !force && self.last_saved.as_ref() == Some(&snap) {
            return;
        }
        if force {
            state::save(&snap);
        } else {
            let s = snap.clone();
            tokio::task::spawn_blocking(move || state::save(&s));
        }
        self.last_saved = Some(snap);
    }

    fn current_id(&self) -> Option<String> {
        self.current.and_then(|c| self.queue.get(c)).map(|s| s.id.clone())
    }

    fn push_like(&self) {
        let (known, v) = match &self.like {
            Some((_, Like::Like)) => (true, 1),
            Some((_, Like::Dislike)) => (true, -1),
            Some((_, Like::Indifferent)) => (true, 0),
            None => (false, 0),
        };
        self.ui(move |ui| {
            ui.set_like_known(known);
            ui.set_like_status(v);
        });
    }

    fn finish_report(&mut self) {
        if let Some(r) = self.report.take() {
            let pos = self.audio.as_ref().map_or(0.0, |a| a.status().position);
            r.finish(pos);
        }
    }

    fn set_error(&self, msg: String) {
        self.ui(move |ui| ui.set_error_text(msg.into()));
    }

    // ---------- radio ----------

    fn start_radio(&mut self, seed: Song) {
        self.radio_gen += 1;
        self.radio = Some(Radio {
            badge: "RADIO".into(),
            seed_title: seed.title.clone(),
            source: Source::Radio { seed_id: seed.id.clone(), continuation: None, watch: None },
            pages: 0,
            skipped: 0,
            empty_streak: 0,
            fetching: false,
            ended: false,
            error: String::new(),
            generation: self.radio_gen,
        });
        self.queue = vec![seed];
        self.current = Some(0);
        self.manual_end = 1;
        self.played.clear();
        self.fetch_radio();
        self.play_current();
    }

    // ---------- queue editing ----------

    /// Right-click menu action on a row of the browse panel (list 0) or the queue (list 1).
    fn row_action(&mut self, list: i32, i: usize, action: &str) {
        let song = if list == 1 {
            self.queue.get(i).cloned()
        } else {
            match self.rows.get(i) { Some(Row::Item(it)) if it.kind == yt::Kind::Song => Some(it.song.clone()), _ => None }
        };
        match (action, song) {
            ("artist", Some(Song { artist_id: Some(id), .. })) => return self.open_artist(id),
            ("album", Some(Song { album_id: Some(id), .. })) => return self.open_collection(id, "ALBUM"),
            ("artist" | "album", _) => return,
            _ => {}
        }
        if list == 1 {
            let Some(song) = self.queue.get(i).cloned() else { return };
            match action {
                "play" => {
                    if self.current != Some(i) {
                        self.current = Some(i);
                        self.play_current();
                    }
                }
                "next" => self.insert_songs(vec![song], true),
                "queue" => self.insert_songs(vec![song], false),
                "remove" => self.remove_at(i),
                "radio" => self.start_radio(song),
                _ => {}
            }
            return;
        }
        let Some(Row::Item(item)) = self.rows.get(i).cloned() else { return };
        match item.kind {
            yt::Kind::Song => match action {
                "next" => self.insert_songs(vec![item.song], true),
                "queue" => self.insert_songs(vec![item.song], false),
                "radio" => self.start_radio(item.song),
                _ => {}
            },
            yt::Kind::Album | yt::Kind::Playlist => {
                let Some(yt) = self.yt.clone() else { return };
                let badge = if item.kind == yt::Kind::Album { "ALBUM" } else { "PLAYLIST" };
                let (tx, id, action) = (self.tx.clone(), item.song.id, action.to_string());
                tokio::spawn(async move {
                    let res = async {
                        let mut c = yt.collection(&id).await?;
                        // Adding to the queue takes the whole list (playing it loads pages as it goes).
                        let mut pages = 1;
                        while matches!(action.as_str(), "next" | "queue") && pages < MAX_ADD_PAGES {
                            let Some(cont) = c.continuation.take() else { break };
                            let (more, next) = yt.collection_more(&cont, &c.artist, &c.thumb).await?;
                            c.tracks.extend(more);
                            c.continuation = next;
                            pages += 1;
                        }
                        anyhow::Ok(c)
                    }.await.map_err(|e| e.to_string());
                    let _ = tx.send(Ev::RowCollection(action, badge, res));
                });
            }
            yt::Kind::Artist => {}
        }
    }

    /// Put songs right after the current one (`next`), or after the other songs added by hand.
    /// A song that's already coming up is moved, not duplicated.
    fn insert_songs(&mut self, songs: Vec<Song>, next: bool) {
        if songs.is_empty() {
            return;
        }
        let Some(c) = self.current else {
            // Nothing playing: play them, and let a radio follow, as with a list.
            let c = yt::Collection { title: songs[0].title.clone(), subtitle: String::new(), thumb: None,
                artist: String::new(), tracks: songs, continuation: None };
            self.play_list(&c, "QUEUE", 0, false);
            return;
        };
        for s in &songs {
            self.played.remove(&s.id);
        }
        queue::insert(&mut self.queue, c, &mut self.manual_end, songs, next);
        self.push_queue();
        self.prefetch_next();
        self.save_state(true);
    }

    /// Remove a song from the queue (not the one playing).
    fn remove_at(&mut self, i: usize) {
        let Some(c) = self.current else { return };
        self.current = Some(queue::remove(&mut self.queue, c, &mut self.manual_end, i));
        self.push_queue();
        self.prefetch_next();
        self.maybe_refill();
        self.save_state(true);
    }

    fn upcoming(&self) -> usize {
        self.current.map_or(self.queue.len(), |c| self.queue.len().saturating_sub(c + 1))
    }

    fn maybe_refill(&mut self) {
        if self.signed_out {
            return;
        }
        // A list without more pages switches to a radio only near its end.
        let finished_list = matches!(self.radio.as_ref().map(|r| &r.source), Some(Source::List { continuation: None, .. }));
        // A shuffled list loads all its pages right away, so every song can come up early.
        let shuffling = matches!(self.radio.as_ref().map(|r| &r.source), Some(Source::List { continuation: Some(_), shuffle: true, .. }));
        let need = shuffling || self.upcoming() < if finished_list { 3 } else { REFILL_BELOW };
        if let Some(r) = &self.radio
            && need && !r.fetching && !r.ended && r.error.is_empty()
        {
            self.fetch_radio();
        }
    }

    /// Play an album/playlist from track `index` (or shuffled); a radio takes over when it ends.
    fn play_list(&mut self, c: &yt::Collection, badge: &str, index: usize, shuffle: bool) {
        if c.tracks.is_empty() {
            return;
        }
        self.radio_gen += 1;
        self.radio = Some(Radio {
            badge: badge.into(),
            seed_title: c.title.clone(),
            source: Source::List { title: c.title.clone(), continuation: c.continuation.clone(), artist: c.artist.clone(), thumb: c.thumb.clone(), shuffle },
            pages: 1,
            skipped: 0,
            empty_streak: 0,
            fetching: false,
            ended: false,
            error: String::new(),
            generation: self.radio_gen,
        });
        self.queue = c.tracks.clone();
        if shuffle {
            self.queue.shuffle(&mut rand::rng());
        }
        self.current = Some(if shuffle { 0 } else { index.min(self.queue.len() - 1) });
        self.manual_end = self.current.unwrap_or(0) + 1;
        self.played.clear();
        self.play_current();
    }

    fn fetch_radio(&mut self) {
        let (Some(yt), Some(r)) = (self.yt.clone(), self.radio.as_mut()) else { return };
        // A finished list hands over to a radio from its last song.
        if let Source::List { title, continuation: None, .. } = &r.source {
            let Some(last) = self.queue.last().cloned() else { return };
            r.seed_title = format!("{} (after {})", last.title, title);
            r.badge = "RADIO".into();
            r.source = Source::Radio { seed_id: last.id, continuation: None, watch: None };
            r.pages = 0;
        }
        r.fetching = true;
        let (generation, tx) = (r.generation, self.tx.clone());
        match &r.source {
            Source::Radio { seed_id, continuation, watch } => {
                let (seed, cont, watch) = (seed_id.clone(), continuation.clone(), watch.clone());
                tokio::spawn(async move {
                    let res = yt.radio(&seed, watch.as_ref(), cont.as_deref()).await.map_err(|e| e.to_string());
                    let _ = tx.send(Ev::RadioPage(generation, res));
                });
            }
            Source::List { continuation: Some(c), artist, thumb, .. } => {
                let (c, artist, thumb) = (c.clone(), artist.clone(), thumb.clone());
                tokio::spawn(async move {
                    let res = yt.collection_more(&c, &artist, &thumb).await.map_err(|e| e.to_string());
                    let _ = tx.send(Ev::ListPage(generation, res));
                });
            }
            Source::List { continuation: None, .. } => unreachable!(),
        }
        self.push_queue();
    }

    fn on_list_page(&mut self, generation: u64, res: Result<(Vec<Song>, Option<String>), String>) {
        if res.as_ref().is_err_and(|e| e.contains(yt::SIGNED_OUT)) {
            self.mark_signed_out();
            return;
        }
        let Some(r) = self.radio.as_mut().filter(|r| r.generation == generation) else { return };
        r.fetching = false;
        match res {
            Err(e) => r.error = format!("Couldn't load more of the list: {e}"),
            Ok((songs, next)) => {
                r.pages += 1;
                let mut shuffled = false;
                if let Source::List { continuation, shuffle, .. } = &mut r.source {
                    *continuation = next;
                    shuffled = *shuffle;
                }
                if shuffled {
                    // Mix into the part that hasn't played yet (after songs added by hand).
                    let mut rng = rand::rng();
                    let from = self.manual_end.max(self.current.map_or(0, |c| c + 1)).min(self.queue.len());
                    for s in songs {
                        let at = rng.random_range(from..=self.queue.len());
                        self.queue.insert(at, s);
                    }
                } else {
                    self.queue.extend(songs);
                }
            }
        }
        self.push_queue();
        self.maybe_refill();
        if self.pending_advance && self.upcoming() > 0 {
            self.pending_advance = false;
            self.next();
        }
        self.prefetch_next();
    }

    fn on_radio_page(&mut self, generation: u64, res: Result<yt::RadioPage, String>) {
        if res.as_ref().is_err_and(|e| e.contains(yt::SIGNED_OUT)) {
            if let Some(r) = self.radio.as_mut() {
                r.fetching = false;
            }
            self.mark_signed_out();
            return;
        }
        let Some(r) = self.radio.as_mut().filter(|r| r.generation == generation) else { return };
        eprintln!("[radio] page: {}", match &res { Ok(p) => format!("{} songs, continuation: {}", p.songs.len(), p.continuation.is_some()), Err(e) => e.clone() });
        r.fetching = false;
        match res {
            Err(e) => r.error = format!("Couldn't load more: {e}"),
            Ok(page) => {
                r.pages += 1;
                let ended_now = page.continuation.is_none();
                if let Source::Radio { continuation, .. } = &mut r.source {
                    *continuation = page.continuation;
                }
                let in_queue: HashSet<String> = self.queue.iter().map(|s| s.id.clone()).collect();
                let filter = matches!(&r.source, Source::Radio { watch: None, .. });
                let mut added = 0;
                let mut seen = HashSet::new();
                for s in page.songs {
                    if in_queue.contains(&s.id) || !seen.insert(s.id.clone()) {
                        continue; // the seed itself, or a duplicate within the radio
                    }
                    if filter && self.history.contains(&s.id) {
                        r.skipped += 1;
                        continue;
                    }
                    self.queue.push(s);
                    added += 1;
                }
                r.empty_streak = if added == 0 { r.empty_streak + 1 } else { 0 };
                if ended_now || r.empty_streak >= MAX_EMPTY_PAGES {
                    r.ended = true;
                }
            }
        }
        self.push_queue();
        if self.current.is_none() && !self.queue.is_empty() {
            // An artist's Mix/Shuffle: its first song starts now.
            self.current = Some(0);
            self.play_current();
        } else if self.current.is_none() && self.radio.as_ref().is_some_and(|r| !r.fetching) {
            self.loading = false;
            self.push_now();
            self.ui(|ui| ui.set_loading_text("".into()));
        }
        self.maybe_refill();
        if self.pending_advance && self.upcoming() > 0 {
            self.pending_advance = false;
            self.next();
        }
        self.prefetch_next();
    }

    // ---------- playback ----------

    fn play_current(&mut self) {
        let Some(song) = self.current.and_then(|c| self.queue.get(c)).cloned() else { return };
        if self.signed_out && !self.prefetch.contains_key(&song.id) {
            self.set_error("Signed out by YouTube: re-import cookies and restart.".into());
            return;
        }
        self.manual_end = self.manual_end.max(self.current.map_or(0, |c| c + 1));
        eprintln!("[queue] now #{}: {} ({})", self.current.unwrap_or(0), song.title, if self.prefetch.contains_key(&song.id) { "prefetched" } else { "fetching" });
        self.play_gen += 1;
        self.resume_at = None;
        self.loading = true;
        self.paused = false;
        self.duration = parse_duration(&song.duration);
        self.pending_advance = false;
        self.finish_report();
        if let Some(a) = &self.audio {
            a.stop();
        }
        self.like = None;
        self.push_like();
        let dur = self.duration;
        self.ui(move |ui| {
            ui.set_error_text("".into());
            ui.set_position(0.0);
            ui.set_duration(dur as f32);
            ui.set_pos_text("0:00".into());
            ui.set_dur_text(fmt_time(dur).into());
            ui.set_quality_text("".into());
        });
        self.push_now();
        self.push_queue();
        self.maybe_refill();
        self.send_track_meta(&song);
        self.push_lyrics();

        let generation = self.play_gen;
        self.playing_stream = None;
        if self.prefetch.get(&song.id).is_some_and(|t| t.data.failed().is_some()) {
            self.prefetch.remove(&song.id);
        }
        if let Some(data) = self.prefetch.remove(&song.id) {
            let _ = self.tx.send(Ev::Loaded(generation, song.id, Ok(data)));
            return;
        }
        let Some(yt) = self.yt.clone() else { return };
        self.ui(|ui| ui.set_loading_text("Getting stream… (the first song after a YouTube player update takes ~10 s)".into()));
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let res = fetch_audio(&yt, &song.id).await.map_err(|e| e.to_string());
            let _ = tx.send(Ev::Loaded(generation, song.id, res));
        });
    }

    fn on_loaded(&mut self, generation: u64, id: String, res: Result<Track, String>) {
        if generation != self.play_gen {
            // Superseded: keep it for later if it's still in the queue.
            if let Ok(d) = res {
                self.prefetch.insert(id, d);
            }
            return;
        }
        self.loading = false;
        eprintln!("[play] {id}: {}", match &res { Ok(t) => format!("itag {}, {} KiB ({} KiB here)", t.itag, t.data.len() / 1024, t.data.downloaded() / 1024), Err(e) => e.clone() });
        match res {
            Ok(Track { data, itag, duration }) => {
                self.fail_streak = 0;
                if duration > 0.0 {
                    self.duration = duration;
                    self.ui(move |ui| {
                        ui.set_duration(duration as f32);
                        ui.set_dur_text(fmt_time(duration).into());
                    });
                }
                self.playing_stream = Some(data.clone());
                self.load_started = Some(std::time::Instant::now());
                if let Some(a) = &self.audio {
                    a.load(&data);
                    if let Some(p) = self.resume_at.take() {
                        a.seek(p);
                        self.to_mpris(mpris::Update::Seeked(p));
                    }
                }
                if duration > 0.0
                    && let Some(song) = self.current.and_then(|c| self.queue.get(c)).cloned()
                {
                    self.send_track_meta(&song);
                }
                if let Some(song) = self.current.and_then(|c| self.queue.get(c)).cloned() {
                    self.request_lyrics(&song);
                }
                if std::env::var("YTM_SMOKE").is_ok_and(|v| v != "resume" && v != "home" && v != "library" && !v.starts_with("lyrics:") && !v.starts_with("lightbox:")) && std::env::var_os("YTM_SMOKE_INPUT").is_none() && !self.smoke_seek_done {
                    // Smoke test: jump near the end to exercise seeking and auto-advance.
                    self.smoke_seek_done = true;
                    let (tx, to) = (self.tx.clone(), (self.duration - 6.0).max(0.0) as f32);
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_secs(4)).await;
                        eprintln!("[smoke] seeking to {to:.0}s");
                        let _ = tx.send(Ev::Seek(to));
                    });
                }
                if let Some(yt) = self.yt.clone() {
                    // Smoke tests don't touch the account's history.
                    // Smoke tests don't touch the account's history; neither does "Pause watch history".
                    if std::env::var_os("YTM_SMOKE").is_none() && !self.settings.pause_history {
                        self.report = Some(report::Session::start(yt.clone(), id.clone(), itag));
                    }
                    let (tx, vid) = (self.tx.clone(), id.clone());
                    tokio::spawn(async move {
                        let res = yt.like_status(&vid).await.map_err(|e| e.to_string());
                        let _ = tx.send(Ev::LikeStatus(vid, res));
                    });
                }
                self.history.push(&id);
                self.played.insert(id);
                let q = match itag {
                    774 => "Opus · 256 kbps",
                    251 => "Opus · 128 kbps",
                    _ => "Opus · low quality",
                };
                self.ui(move |ui| ui.set_quality_text(q.into()));
                self.prefetch_next();
            }
            Err(e) if e.contains(yt::SIGNED_OUT) => self.mark_signed_out(),
            Err(e) => {
                let title = self.current.and_then(|c| self.queue.get(c)).map(|s| s.title.clone()).unwrap_or_default();
                self.fail_streak += 1;
                if self.fail_streak >= 3 {
                    self.set_error(format!("Couldn't play “{title}”: {e}. Stopped after 3 failures in a row."));
                } else if self.upcoming() > 0 {
                    self.set_error(format!("Couldn't play “{title}”: {e}. Skipping."));
                    // Don't loop instantly on a systematic failure.
                    let tx = self.tx.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        let _ = tx.send(Ev::Next);
                    });
                }
            }
        }
        self.push_now();
    }

    fn prefetch_next(&mut self) {
        let Some(next) = self.current.and_then(|c| self.queue.get(c + 1)).cloned() else { return };
        if self.prefetch.contains_key(&next.id) || !self.prefetching.insert(next.id.clone()) {
            return;
        }
        // Keep memory bounded: only what's still ahead in the queue.
        let ahead: HashSet<String> = self.current.map(|c| self.queue[c..].iter().map(|s| s.id.clone()).collect()).unwrap_or_default();
        self.prefetch.retain(|k, _| ahead.contains(k));
        let Some(yt) = self.yt.clone() else { return };
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let res = fetch_audio(&yt, &next.id).await.map_err(|e| e.to_string());
            let _ = tx.send(Ev::Prefetched(next.id, res));
        });
    }

    fn next(&mut self) {
        match self.current {
            Some(c) if c + 1 < self.queue.len() => {
                self.current = Some(c + 1);
                self.play_current();
            }
            Some(_) if self.radio.as_ref().is_some_and(|r| r.fetching || !r.ended) => {
                self.pending_advance = true;
                self.maybe_refill();
                self.ui(|ui| {
                    ui.set_loading(true);
                    ui.set_loading_text("Waiting for more radio songs…".into());
                });
            }
            _ => {}
        }
    }

    fn prev(&mut self) {
        let pos = self.audio.as_ref().map_or(0.0, |a| a.status().position);
        match self.current {
            Some(c) if pos < 3.0 && c > 0 => {
                self.current = Some(c - 1);
                self.play_current();
            }
            Some(_) => {
                if let Some(a) = &self.audio {
                    a.seek(0.0);
                }
            }
            None => {}
        }
    }

    fn tick(&mut self) {
        self.ticks += 1;
        self.ui(|ui| {
            let awake = !ui.window().is_minimized();
            let rt = ui.global::<Runtime>();
            if rt.get_awake() != awake {
                rt.set_awake(awake);
            }
        });
        if self.ticks % 8 == 0 {
            // Every 2 s: a new KDE color scheme / font / icon theme applies right away.
            let changed = kde::settings_changed_at();
            if changed != self.kde_changed_at {
                self.kde_changed_at = changed;
                eprintln!("[kde] settings changed; applying");
                self.ui(|ui| kde::apply(ui));
            }
        }
        if self.ticks % 86_400 == 0 {
            // Every 6 hours (the first check runs shortly after startup).
            let _ = self.tx.send(Ev::UpdateCheck(false));
        }
        if self.ticks % 240 == 0 {
            // Every minute: return freed memory (old tracks, decoded art) to the OS.
            paths::release_memory();
        }
        if self.ticks % 40 == 0
            && self.persist
            && let Some(yt) = self.yt.as_ref().filter(|y| y.take_changed())
        {
            let cookies = yt.cookies();
            tokio::task::spawn_blocking(move || {
                if let Err(e) = store::save(&cookies) {
                    eprintln!("[auth] couldn't save refreshed cookies: {e:#}");
                }
            });
        }
        if self.ticks % 20 == 0 {
            self.save_state(false);
        }
        let Some(a) = &self.audio else { return };
        let st = a.status();
        let playing = st.loaded && st.playing && !self.loading;
        let status = (playing, self.current.is_some_and(|c| c + 1 < self.queue.len()) || self.radio.is_some(), self.current.is_some());
        if self.mpris_status != Some(status) {
            self.mpris_status = Some(status);
            self.to_mpris(mpris::Update::Status { playing: status.0, has_next: status.1, has_prev: status.2 });
        }
        if self.ticks % 4 == 0 && st.loaded {
            self.to_mpris(mpris::Update::Position(st.position));
        }
        if self.view == View::Lyrics && st.loaded {
            let (pos, running) = (st.position as f32, playing);
            // The line being sung (or the last one that started).
            let line = self.current_id()
                .and_then(|id| self.lyrics.get(&id).cloned().flatten())
                .and_then(|l| l.lines.iter().rposition(|ln| ln.start <= st.position))
                .map_or(-1, |i| i as i32);
            self.ui(move |ui| {
                ui.set_lyrics_sync_pos(pos);
                ui.set_lyrics_running(running);
                ui.set_lyrics_line(line);
            });
        }
        if st.position > 0.0
            && let Some(t) = self.load_started.take()
        {
            let have = self.playing_stream.as_ref().map_or(0, |s| s.downloaded() / 1024);
            eprintln!("[play] sound after {} ms ({have} KiB downloaded by then)", t.elapsed().as_millis());
        }
        if let Some(e) = self.playing_stream.as_ref().and_then(|s| s.failed()) {
            self.playing_stream = None;
            a.stop();
            let id = self.current_id().unwrap_or_default();
            self.on_loaded(self.play_gen, id, Err(format!("download failed ({e})")));
            return;
        }
        if st.finished && !self.loading {
            if self.upcoming() > 0 || self.radio.is_some() {
                self.next();
            } else {
                a.stop();
                self.finish_report();
                self.paused = true;
                self.push_now();
            }
            return;
        }
        if st.loaded
            && let Some(r) = self.report.as_mut()
        {
            r.progress(st.position, st.playing && !self.loading);
        }
        if st.loaded {
            let pos = st.position.min(self.duration.max(st.position));
            let playing = st.playing && !self.loading;
            self.ui(move |ui| {
                if !ui.get_seeking() {
                    ui.set_position(pos as f32);
                    ui.set_pos_text(fmt_time(pos).into());
                }
                ui.set_playing(playing);
            });
        }
    }

    fn handle(&mut self, ev: Ev) {
        match ev {
            Ev::Connected(Ok(yt)) => {
                self.yt = Some(yt);
                self.signed_out = false;
                self.fail_streak = 0;
                if let Some(r) = self.radio.as_mut() {
                    r.error.clear();
                }
                self.ui(|ui| {
                    ui.set_show_sign_in(false);
                    ui.set_error_text("".into());
                });
                self.home = None;
                self.home_error.clear();
                self.liked = None;
                self.library = None;
                self.load_home();
                self.push_browse();
                self.restore();
                match self.smoke.as_deref() {
                    Some("home" | "resume") => {}
                    Some("liked") => {
                        let _ = self.tx.send(Ev::ShowView(1));
                    }
                    Some("library") => {
                        let _ = self.tx.send(Ev::ShowView(7));
                    }
                    Some(q) => {
                        let q = q.strip_prefix("lyrics:").or_else(|| q.strip_prefix("lightbox:")).unwrap_or(q);
                        eprintln!("[smoke] signed in; searching {q:?}");
                        let _ = self.tx.send(Ev::Search(q.to_string()));
                    }
                    None => {}
                }
                self.ui(|ui| {
                    ui.set_account_ok(true);
                    ui.set_account_text("Signed in".into());
                });
            }
            Ev::Connected(Err(e)) => {
                let _ = self.tx.send(Ev::NeedSignIn(format!("Couldn't sign in: {e}")));
            }
            Ev::NeedSignIn(msg) => {
                if self.smoke.is_some() {
                    smoke_screenshot(self.ui.clone(), 3);
                }
                self.ui(move |ui| {
                    ui.set_account_ok(false);
                    ui.set_account_text("Not signed in".into());
                    ui.set_show_sign_in(true);
                    ui.set_sign_in_busy(false);
                    ui.set_sign_in_status(msg.into());
                });
            }
            Ev::ChooseCookies => {
                if self.importing {
                    return;
                }
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    let picked = rfd::AsyncFileDialog::new()
                        .set_title("Choose the cookies.txt exported from music.youtube.com")
                        .add_filter("Netscape cookies file", &["txt"])
                        .pick_file()
                        .await
                        .map(|f| f.path().to_path_buf());
                    let _ = tx.send(Ev::ImportPicked(picked));
                });
            }
            Ev::ImportPicked(None) => {}
            Ev::ImportPicked(Some(path)) => {
                self.importing = true;
                self.ui(|ui| {
                    ui.set_sign_in_busy(true);
                    ui.set_sign_in_status("Checking the login with YouTube…".into());
                });
                let (tx, solver) = (self.tx.clone(), self.solver.clone());
                tokio::spawn(async move {
                    let res = import_login(path, solver).await.map_err(|e| format!("{e:#}"));
                    let _ = tx.send(Ev::Imported(res));
                });
            }
            Ev::Imported(res) => {
                self.importing = false;
                match res {
                    Ok((yt, notice)) => {
                        self.persist = true;
                        let _ = self.tx.send(Ev::Connected(Ok(yt)));
                        self.ui(move |ui| {
                            ui.set_sign_in_busy(false);
                            ui.set_sign_in_notice(notice.into());
                        });
                    }
                    Err(e) => {
                        self.ui(move |ui| {
                            ui.set_sign_in_busy(false);
                            ui.set_sign_in_status(format!("{e}. The file was left where it was.").into());
                        });
                    }
                }
            }
            Ev::SignOut => {
                self.finish_report();
                self.to_mpris(mpris::Update::NoTrack);
                if let Some(a) = &self.audio {
                    a.stop();
                }
                self.yt = None;
                self.radio = None;
                self.queue.clear();
                self.current = None;
                self.prefetch.clear();
                self.like = None;
                self.loading = false;
                self.push_like();
                self.push_queue();
                self.push_now();
                let (tx, persist) = (self.tx.clone(), self.persist);
                self.persist = false;
                tokio::spawn(async move {
                    let res = if persist {
                        tokio::task::spawn_blocking(store::clear).await.map_err(|e| e.to_string()).and_then(|r| r.map_err(|e| format!("{e:#}")))
                    } else {
                        Ok(())
                    };
                    let _ = tx.send(Ev::SignedOutDone(res));
                });
            }
            Ev::SignedOutDone(res) => {
                let msg = match res {
                    Ok(()) => "Signed out. The stored login and its key were deleted.".to_string(),
                    Err(e) => format!("Signed out, but deleting the stored login failed: {e}"),
                };
                let _ = self.tx.send(Ev::NeedSignIn(msg));
            }
            Ev::Search(q) => {
                let q = q.trim().to_string();
                let Some(yt) = self.yt.clone() else { return };
                if q.is_empty() {
                    return;
                }
                self.search_gen += 1;
                let (generation, tx) = (self.search_gen, self.tx.clone());
                self.ui(|ui| ui.set_searching(true));
                tokio::spawn(async move {
                    let res = yt.search(&q).await.map_err(|e| e.to_string());
                    let _ = tx.send(Ev::SearchDone(generation, res));
                });
            }
            Ev::SearchDone(generation, res) => {
                if generation != self.search_gen {
                    return;
                }
                self.ui(|ui| ui.set_searching(false));
                match res {
                    Ok(songs) => {
                        let empty = songs.is_empty();
                        if let Some(q) = self.smoke.take() {
                            eprintln!("[smoke] {} results; first: {:?}", songs.len(), songs.first().map(|s| (&s.title, &s.artist)));
                            let _ = self.tx.send(Ev::OpenRow(0));
                            if q.starts_with("lyrics:") {
                                let _ = self.tx.send(Ev::ShowView(4));
                            }
                            if let Some(secs) = std::env::var("YTM_SMOKE_MINIMIZE").ok().and_then(|v| v.parse::<u64>().ok()) {
                                let ui = self.ui.clone();
                                tokio::spawn(async move {
                                    tokio::time::sleep(Duration::from_secs(secs)).await;
                                    eprintln!("[smoke] minimizing");
                                    let _ = slint::invoke_from_event_loop(move || {
                                        if let Some(ui) = ui.upgrade() {
                                            ui.window().set_minimized(true);
                                        }
                                    });
                                });
                            }
                            if q.starts_with("lightbox:") {
                                // Same as right-clicking the now-playing artwork.
                                let ui = self.ui.clone();
                                tokio::spawn(async move {
                                    tokio::time::sleep(Duration::from_secs(8)).await;
                                    let _ = slint::invoke_from_event_loop(move || {
                                        if let Some(ui) = ui.upgrade() {
                                            ui.invoke_view_image(ui.get_now_art(), "now".into());
                                        }
                                    });
                                });
                            }
                            smoke_screenshot(self.ui.clone(), 30);
                        }
                        self.results = songs;
                        self.show_view(View::Search);
                        if empty {
                            self.ui(|ui| ui.set_browse_hint("No songs found.".into()));
                        }
                    }
                    Err(e) => {
                        self.results.clear();
                        self.show_view(View::Search);
                        self.ui(move |ui| ui.set_browse_hint(format!("Search failed: {e}").into()));
                    }
                }
            }
            Ev::OpenRow(i) => self.open_row(i),
            Ev::NearEnd => {
                if self.view == View::Home {
                    self.load_more_home();
                }
            }
            Ev::Jump(i) => {
                if i < self.queue.len() {
                    self.current = Some(i);
                    self.play_current();
                }
            }
            Ev::TogglePlay => {
                let nothing_loaded = self.audio.as_ref().is_some_and(|a| !a.status().loaded);
                if nothing_loaded && self.current.is_some() && !self.loading {
                    // Restored after a restart (or stopped): start the current song, at the saved spot.
                    let at = self.resume_at;
                    self.play_current();
                    self.resume_at = at;
                } else if let Some(a) = &self.audio
                    && !self.loading
                {
                    self.paused = !self.paused;
                    a.set_paused(self.paused);
                    self.push_now();
                }
            }
            Ev::Next => self.next(),
            Ev::Prev => self.prev(),
            Ev::Seek(s) => {
                if let Some(a) = &self.audio {
                    a.seek(s as f64);
                }
                let playing = !self.paused && !self.loading;
                if let Some(r) = self.report.as_mut() {
                    r.seek(s as f64, playing);
                }
                self.to_mpris(mpris::Update::Seeked(s as f64));
            }
            Ev::LikeClicked(button) => {
                let Some((id, current)) = self.like.clone() else { return };
                let Some(yt) = self.yt.clone() else { return };
                // Pressing the active button again clears it.
                let new = if current == button { Like::Indifferent } else { button };
                self.like = Some((id.clone(), new));
                self.push_like();
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    let res = yt.set_like(&id, new).await.map_err(|e| e.to_string());
                    let _ = tx.send(Ev::LikeSet(id, current, res));
                });
            }
            Ev::LikeStatus(id, res) => {
                if self.current_id().as_deref() == Some(id.as_str()) {
                    match res {
                        Ok(l) => {
                            eprintln!("[like] {id}: status {l:?}");
                            self.like = Some((id, l));
                            self.push_like();
                        }
                        Err(e) => eprintln!("[like] status for {id}: {e}"),
                    }
                }
            }
            Ev::LikeSet(id, previous, res) => {
                if let Err(e) = res {
                    self.set_error(format!("Couldn't update like: {e}"));
                    if self.like.as_ref().is_some_and(|(cur, _)| *cur == id) {
                        self.like = Some((id, previous));
                        self.push_like();
                    }
                } else {
                    eprintln!("[like] {id}: saved");
                }
            }
            Ev::Volume(v) => {
                self.volume = v;
                if let Some(a) = &self.audio {
                    a.set_volume(v);
                }
                self.to_mpris(mpris::Update::Volume(v as f64));
            }
            Ev::Mpris(cmd) => self.on_mpris(cmd),
            Ev::Speed(v) => {
                let v = (v.clamp(0.1, 3.0) * 100.0).round() / 100.0;
                self.speed = v;
                if let Some(a) = &self.audio {
                    a.set_speed(v);
                }
                self.ui(move |ui| ui.set_speed(v));
            }
            Ev::ViewArt(key) => {
                // The viewer already shows the small image; fetch a large version to replace it.
                let thumb = match key.as_str() {
                    "now" => self.current.and_then(|c| self.queue.get(c)).and_then(|s| s.thumb.clone()),
                    "coll" => match self.view {
                        View::Liked => self.liked.as_ref().and_then(|c| c.thumb.clone()),
                        _ => self.collection.as_ref().and_then(|(_, c)| c.thumb.clone()),
                    },
                    k => {
                        let (src, id) = k.split_at(2);
                        let pick = |s: &Song| s.id == id;
                        if src == "q:" {
                            self.queue.iter().find(|s| pick(s)).and_then(|s| s.thumb.clone())
                        } else {
                            self.rows.iter().find_map(|r| match r { Row::Item(it) if pick(&it.song) => it.song.thumb.clone(), _ => None })
                        }
                    }
                };
                self.lightbox_gen += 1;
                let (Some(url), Some(yt)) = (thumb, self.yt.clone()) else { return };
                let (tx, generation) = (self.tx.clone(), self.lightbox_gen);
                tokio::spawn(async move {
                    let big = sized_thumb(&url, 1200);
                    let px = match yt.image(&big).await {
                        Ok(bytes) => tokio::task::spawn_blocking(move || {
                            let img = image::load_from_memory(&bytes).ok()?.to_rgba8();
                            Some(Pixels::clone_from_slice(img.as_raw(), img.width(), img.height()))
                        }).await.ok().flatten(),
                        Err(_) => None,
                    };
                    let _ = tx.send(Ev::ArtFull(generation, px));
                });
            }
            Ev::ArtFull(generation, px) => {
                if generation == self.lightbox_gen
                    && let Some(px) = px
                {
                    self.ui(move |ui| {
                        if ui.get_lightbox_open() {
                            ui.set_lightbox_image(slint::Image::from_rgba8(px));
                        }
                    });
                }
            }
            Ev::LyricsLoaded(id, found, manual) => {
                eprintln!("[lyrics] {id}{}: {}", if manual { " (by hand)" } else { "" },
                    if found.is_empty() { "none".into() } else { found.iter().map(|l| format!("{:?} from {}", l.sync, l.source)).collect::<Vec<_>>().join(", ") });
                let found: Vec<Arc<lyrics::Lyrics>> = found.into_iter().map(Arc::new).collect();
                if manual {
                    self.lyrics_searching = false;
                    // New results first; keep the earlier ones (minus duplicates) after them.
                    let old = self.lyrics_found.remove(&id).unwrap_or_default();
                    let mut all = found;
                    for l in old {
                        if !all.iter().any(|n| n.signature() == l.signature()) {
                            all.push(l);
                        }
                    }
                    self.lyrics_found.insert(id.clone(), all);
                } else {
                    self.lyrics_pending.remove(&id);
                    if self.lyrics.len() > 50 {
                        self.lyrics.clear();
                        self.lyrics_found.clear();
                    }
                    self.lyrics.insert(id.clone(), found.first().cloned());
                    self.lyrics_found.insert(id.clone(), found);
                }
                if self.current_id().as_deref() == Some(id.as_str()) {
                    self.push_lyrics();
                }
            }
            Ev::LyricsSearch(title, artist) => {
                let (Some(finder), Some(id)) = (self.finder.clone(), self.current_id()) else { return };
                if title.trim().is_empty() || self.lyrics_searching {
                    return;
                }
                self.lyrics_searching = true;
                self.push_lyric_options();
                let (tx, dur) = (self.tx.clone(), self.duration);
                tokio::spawn(async move {
                    let found = finder.find_all(title.trim(), artist.trim(), "", dur, None).await;
                    let _ = tx.send(Ev::LyricsLoaded(id, found, true));
                });
            }
            Ev::UpdateCheck(manual) => {
                let Some(u) = self.updater.clone() else { return };
                if self.update_busy || self.update_ready || (!manual && !self.settings.check_updates) {
                    return;
                }
                self.update_busy = true;
                self.push_update_status(if manual { "Checking…".into() } else { String::new() });
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    let res = u.check().await.map_err(|e| format!("{e:#}"));
                    let _ = tx.send(Ev::UpdateChecked(res, manual));
                });
            }
            Ev::UpdateChecked(res, manual) => {
                self.update_busy = false;
                match res {
                    Ok(None) => {
                        self.update = None;
                        self.push_update_status(format!("You have the latest version ({}).", update::current_version()));
                    }
                    Ok(Some(r)) => {
                        eprintln!("[update] {} available ({:?})", r.version, update::mode());
                        let (version, page) = (r.version.clone(), r.page.clone());
                        self.update = Some(r);
                        match update::mode() {
                            update::Mode::SelfUpdate if self.settings.auto_update => {
                                let _ = self.tx.send(Ev::UpdateInstall);
                            }
                            update::Mode::SelfUpdate => self.push_update_status(format!("Version {version} is available. {page}")),
                            update::Mode::Packaged(by) => self.push_update_status(format!(
                                "Version {version} is available; update it with your package manager ({by}). {page}")),
                            update::Mode::FromSource => self.push_update_status(format!(
                                "Version {version} is available. This copy was built from source: pull and rebuild, or install a release. {page}")),
                        }
                    }
                    Err(e) => {
                        eprintln!("[update] check failed: {e}");
                        self.push_update_status(if manual { format!("Couldn't check: {e}") } else { String::new() });
                    }
                }
            }
            Ev::UpdateInstall => {
                let (Some(u), Some(r)) = (self.updater.clone(), self.update.clone()) else { return };
                if self.update_busy || self.update_ready || update::mode() != update::Mode::SelfUpdate {
                    return;
                }
                self.update_busy = true;
                let size = r.download_size(None).map_or(String::new(), |b| format!(" ({:.1} MB)", b as f64 / 1e6));
                self.push_update_status(format!("Downloading version {}{size}…", r.version));
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    let res = u.install(&r).await.map_err(|e| format!("{e:#}"));
                    let _ = tx.send(Ev::UpdateInstalled(res));
                });
            }
            Ev::UpdateInstalled(res) => {
                self.update_busy = false;
                let version = self.update.as_ref().map(|r| r.version.clone()).unwrap_or_default();
                match res {
                    Ok(how) => {
                        eprintln!("[update] installed {version} ({how})");
                        self.update_ready = true;
                        self.push_update_status(format!("Version {version} is installed ({how}). It starts the next time you open the app."));
                    }
                    Err(e) => {
                        eprintln!("[update] install failed: {e}");
                        self.push_update_status(format!("Couldn't install version {version}: {e}"));
                    }
                }
            }
            Ev::Restart => {
                eprintln!("[update] restarting");
                RESTART.store(true, std::sync::atomic::Ordering::Relaxed);
                self.ui(|ui| { let _ = ui.window().hide(); let _ = slint::quit_event_loop(); });
            }
            Ev::SettingsChanged(new) => {
                eprintln!("[settings] {new:?}");
                if new.pause_history && !self.settings.pause_history {
                    // Stop reporting the current song too (from here on).
                    self.report = None;
                }
                if std::env::var_os("YTM_SMOKE").is_none() || std::env::var_os("YTM_SMOKE_STATE").is_some() {
                    settings::save(&new);
                }
                self.settings = new;
            }
            Ev::LyricsPick(i) => {
                let Some(id) = self.current_id() else { return };
                let pick = usize::try_from(i).ok().and_then(|i| self.lyrics_found.get(&id)?.get(i).cloned());
                if i >= 0 && pick.is_none() {
                    return;
                }
                self.lyrics.insert(id, pick);
                self.push_lyrics();
            }
            Ev::Shutdown(reply) => {
                self.finish_report();
                self.save_state(true);
                // Give the final play-report ping a moment to go out.
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(800)).await;
                    let _ = reply.send(());
                });
            }
            Ev::RadioPage(generation, res) => self.on_radio_page(generation, res),
            Ev::ListPage(generation, res) => self.on_list_page(generation, res),
            Ev::HomeLoaded(res, more) => {
                if more {
                    self.home_more_loading = false;
                }
                match res {
                    Ok((shelves, next)) => {
                        eprintln!("[home] {} shelves{}, more: {}", shelves.len(), if more { " more" } else { "" }, next.is_some());
                        self.home_next = next;
                        match (&mut self.home, more) {
                            (Some(h), true) => h.extend(shelves),
                            _ => self.home = Some(shelves),
                        }
                    }
                    Err(e) if e.contains(yt::SIGNED_OUT) => self.mark_signed_out(),
                    Err(e) if more => eprintln!("[home] more: {e}"),
                    Err(e) => {
                        self.home_error = format!("Couldn't load Home: {e}");
                    }
                }
                if self.view == View::Home {
                    self.push_browse();
                }
                if self.smoke.as_deref() == Some("home") {
                    smoke_screenshot(self.ui.clone(), 4);
                }
            }
            Ev::CollectionLoaded(generation, id, res) => {
                if generation != self.browse_gen {
                    return;
                }
                match res {
                    Ok(c) => {
                        eprintln!("[collection] {id}: {} tracks, more: {}", c.tracks.len(), c.continuation.is_some());
                        if id == LIKED {
                            if self.smoke.take().as_deref() == Some("liked") {
                                let _ = self.tx.send(Ev::OpenRow(2));
                                smoke_screenshot(self.ui.clone(), 25);
                            }
                            self.liked = Some(c);
                        } else {
                            self.collection = Some((id, c));
                        }
                    }
                    Err(e) if e.contains(yt::SIGNED_OUT) => self.mark_signed_out(),
                    Err(e) => self.browse_error = format!("Couldn't load it: {e}"),
                }
                self.push_browse();
            }
            Ev::ArtistLoaded(generation, id, res) => {
                if generation != self.browse_gen {
                    return;
                }
                match res {
                    Ok(a) => {
                        eprintln!("[artist] {id}: {} shelves, mix: {}, shuffle: {}", a.shelves.len(), a.mix.is_some(), a.shuffle.is_some());
                        self.artist = Some((id, a));
                    }
                    Err(e) if e.contains(yt::SIGNED_OUT) => self.mark_signed_out(),
                    Err(e) => self.browse_error = format!("Couldn't load the artist: {e}"),
                }
                self.push_browse();
            }
            Ev::LibraryLoaded(res) => {
                self.library_loading = false;
                match res {
                    Ok(l) => {
                        eprintln!("[library] {} playlists, {} albums, {} songs, {} artists", l.playlists.len(), l.albums.len(), l.songs.len(), l.artists.len());
                        self.library_error.clear();
                        self.library = Some(l);
                    }
                    Err(e) if e.contains(yt::SIGNED_OUT) => self.mark_signed_out(),
                    Err(e) if self.library.is_some() => eprintln!("[library] reload: {e}"),
                    Err(e) => self.library_error = format!("Couldn't load your library: {e}"),
                }
                if self.view == View::Library {
                    self.push_browse();
                }
                if self.smoke.take().as_deref() == Some("library") {
                    // Scripted input (YTM_SMOKE_INPUT) gets time to click around.
                    smoke_screenshot(self.ui.clone(), if std::env::var_os("YTM_SMOKE_INPUT").is_some() { 25 } else { 4 });
                }
            }
            Ev::ArtistButton(b) => {
                let Some((_, a)) = self.artist.as_ref() else { return };
                let name = a.name.clone();
                match b.as_str() {
                    "mix" => if let Some(w) = a.mix.clone() { self.start_watch(name, "RADIO", w) },
                    "shuffle" => if let Some(w) = a.shuffle.clone() { self.start_watch(format!("{name} (shuffled)"), "ARTIST", w) },
                    "all" => if let Some(id) = a.all_songs.clone() { self.open_collection(id, "PLAYLIST") },
                    _ => {}
                }
            }
            Ev::GoToPlayingArtist => {
                if let Some(id) = self.current.and_then(|c| self.queue.get(c)).and_then(|s| s.artist_id.clone()) {
                    if self.view != View::Artist || self.artist.as_ref().is_none_or(|(aid, _)| *aid != id) {
                        self.open_artist(id);
                    }
                }
            }
            Ev::ShowView(v) => {
                let view = match v {
                    0 => View::Home,
                    1 => View::Liked,
                    2 => View::Search,
                    4 => View::Lyrics,
                    6 => View::Settings,
                    7 => View::Library,
                    _ => return,
                };
                self.show_view(view);
            }
            Ev::PlayAll => {
                let (c, badge) = match self.view {
                    View::Liked => (self.liked.as_ref(), "PLAYLIST"),
                    View::Collection => (self.collection.as_ref().map(|(_, c)| c), self.collection_badge),
                    _ => (None, ""),
                };
                if let Some(c) = c.cloned() {
                    self.play_list(&c, badge, 0, false);
                }
            }
            Ev::ShuffleAll => {
                let (c, badge) = match self.view {
                    View::Liked => (self.liked.as_ref(), "PLAYLIST"),
                    View::Collection => (self.collection.as_ref().map(|(_, c)| c), self.collection_badge),
                    _ => (None, ""),
                };
                if let Some(c) = c.cloned() {
                    self.play_list(&c, badge, 0, true);
                }
            }
            Ev::RowAction(list, i, action) => self.row_action(list, i, &action),
            Ev::RowCollection(action, badge, res) => match res {
                Ok(c) => match action.as_str() {
                    "play" | "shuffle" => self.play_list(&c, badge, 0, action == "shuffle"),
                    "next" => self.insert_songs(c.tracks, true),
                    _ => self.insert_songs(c.tracks, false),
                },
                Err(e) if e.contains(yt::SIGNED_OUT) => self.mark_signed_out(),
                Err(e) => self.set_error(format!("Couldn't load it: {e}")),
            },
            Ev::Back => self.go_back(),
            Ev::Loaded(generation, id, res) => self.on_loaded(generation, id, res),
            Ev::Prefetched(id, res) => {
                self.prefetching.remove(&id);
                if res.as_ref().is_err_and(|e| e.contains(yt::SIGNED_OUT)) {
                    self.mark_signed_out();
                }
                eprintln!("[prefetch] {id}: {}", match &res { Ok(t) => format!("itag {}, {} KiB, downloading", t.itag, t.data.len() / 1024), Err(e) => e.clone() });
                if let Ok(d) = res {
                    self.prefetch.insert(id, d);
                }
            }
            Ev::ArtLoaded(url, evicted) => {
                self.art_pending.remove(&url);
                for k in evicted {
                    self.art_pending.remove(&k);
                }
                self.push_browse();
                self.push_queue();
                self.push_now();
            }
            Ev::Tick => self.tick(),
        }
    }
}

/// A downloaded song, ready to play.
#[derive(Clone)]
struct Track {
    /// Fills while it downloads; playback can start right away.
    data: Arc<audio::Stream>,
    itag: u64,
    duration: f64,
}

/// Find the song's stream and start downloading it in the background. Returns as soon as the
/// download has started; a failed download shows up as `Track::data.failed()`.
async fn fetch_audio(yt: &Arc<yt::Client>, id: &str) -> Result<Track> {
    let s = yt.stream(id).await?;
    let (itag, duration) = (s.itag, s.duration);
    let data = audio::Stream::new(s.content_length);
    let (yt, into, vid) = (yt.clone(), data.clone(), id.to_string());
    tokio::spawn(async move {
        let started = std::time::Instant::now();
        match yt.download_into(&s, &into).await {
            Ok(()) => eprintln!("[download] {vid}: {} KiB in {:.1} s", into.downloaded() / 1024, started.elapsed().as_secs_f64()),
            Err(e) => {
                eprintln!("[download] {vid}: {e}");
                into.fail(e.to_string());
            }
        }
    });
    Ok(Track { data, itag, duration })
}

/// Dev smoke test input script, `YTM_SMOKE_INPUT="<secs>:<action>[:<arg>]; ..."` (times from
/// start). Actions: `click:<x>,<y>` / `rclick:<x>,<y>` (logical px), `wheel:<x>,<y>,<dy>`, `key:<name>` (space, left,
/// right, up, down, esc, a letter; prefix `ctrl+` for Ctrl), `shot:<name>` (saves
/// `<YTM_SMOKE_SHOT minus .png>-<name>.png`). Goes through Slint's own event dispatch.
fn smoke_input(ui: slint::Weak<MainWindow>) {
    use slint::platform::{Key, PointerEventButton, WindowEvent};
    let Ok(script) = std::env::var("YTM_SMOKE_INPUT") else { return };
    let shot = std::env::var("YTM_SMOKE_SHOT").unwrap_or_else(|_| "smoke.png".into());
    let mut steps: Vec<(f64, String, String)> = script.split(';').filter_map(|step| {
        let mut parts = step.trim().splitn(3, ':');
        let t = parts.next()?.trim().parse().ok()?;
        Some((t, parts.next()?.trim().to_string(), parts.next().unwrap_or("").trim().to_string()))
    }).collect();
    steps.sort_by(|a, b| a.0.total_cmp(&b.0));
    std::thread::spawn(move || {
        let start = std::time::Instant::now();
        for (t, action, arg) in steps {
            if let Some(wait) = Duration::from_secs_f64(t).checked_sub(start.elapsed()) {
                std::thread::sleep(wait);
            }
            let (ui, shot) = (ui.clone(), shot.clone());
            let _ = slint::invoke_from_event_loop(move || {
                let Some(ui) = ui.upgrade() else { return };
                let w = ui.window();
                match action.as_str() {
                    "click" | "rclick" => {
                        let Some((x, y)) = arg.split_once(',').and_then(|(x, y)| Some((x.parse::<f32>().ok()?, y.parse::<f32>().ok()?))) else { return };
                        let position = slint::LogicalPosition::new(x, y);
                        let button = if action == "rclick" { PointerEventButton::Right } else { PointerEventButton::Left };
                        w.dispatch_event(WindowEvent::PointerMoved { position });
                        w.dispatch_event(WindowEvent::PointerPressed { position, button });
                        w.dispatch_event(WindowEvent::PointerReleased { position, button });
                    }
                    "wheel" => {
                        let v: Vec<f32> = arg.split(',').filter_map(|n| n.parse().ok()).collect();
                        let [x, y, dy] = v[..] else { return };
                        let position = slint::LogicalPosition::new(x, y);
                        w.dispatch_event(WindowEvent::PointerMoved { position });
                        w.dispatch_event(WindowEvent::PointerScrolled { position, delta_x: 0.0, delta_y: dy });
                    }
                    "key" => {
                        let (ctrl, name) = match arg.strip_prefix("ctrl+") { Some(n) => (true, n), None => (false, arg.as_str()) };
                        let text: SharedString = match name {
                            "space" => " ".into(),
                            "left" => Key::LeftArrow.into(),
                            "right" => Key::RightArrow.into(),
                            "up" => Key::UpArrow.into(),
                            "down" => Key::DownArrow.into(),
                            "esc" => Key::Escape.into(),
                            other => other.into(),
                        };
                        if ctrl {
                            w.dispatch_event(WindowEvent::KeyPressed { text: Key::Control.into() });
                        }
                        w.dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
                        w.dispatch_event(WindowEvent::KeyReleased { text });
                        if ctrl {
                            w.dispatch_event(WindowEvent::KeyReleased { text: Key::Control.into() });
                        }
                    }
                    "shot" => {
                        if let Ok(px) = w.take_snapshot()
                            && let Some(img) = image::RgbaImage::from_raw(px.width(), px.height(), px.as_bytes().to_vec())
                        {
                            let path = shot.replace(".png", &format!("-{arg}.png"));
                            eprintln!("[smoke] input shot -> {path} {:?}", img.save(&path).is_ok());
                        }
                    }
                    other => eprintln!("[smoke] unknown input action {other:?}"),
                }
                eprintln!("[smoke] input {t:.1}s {action} {arg}");
            });
        }
    });
}

/// Dev smoke test: save a screenshot of the window after `secs`, then quit.
fn smoke_screenshot(ui: slint::Weak<MainWindow>, secs: u64) {
    let shot = std::env::var("YTM_SMOKE_SHOT").unwrap_or_else(|_| "smoke.png".into());
    // YTM_SMOKE_BURST=n: also save n frames 120 ms apart before the final shot (animation checks).
    let burst: u32 = std::env::var("YTM_SMOKE_BURST").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(secs)).await;
        for i in 0..burst {
            let (ui, shot) = (ui.clone(), shot.clone());
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui.upgrade()
                    && let Ok(px) = ui.window().take_snapshot()
                    && let Some(img) = image::RgbaImage::from_raw(px.width(), px.height(), px.as_bytes().to_vec())
                {
                    let _ = img.save(shot.replace(".png", &format!("-{i:02}.png")));
                }
            });
            tokio::time::sleep(Duration::from_millis(120)).await;
        }
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui.upgrade() {
                match ui.window().take_snapshot() {
                    Ok(px) => {
                        let img = image::RgbaImage::from_raw(px.width(), px.height(), px.as_bytes().to_vec());
                        let r = img.map(|i| i.save(&shot));
                        eprintln!("[smoke] screenshot -> {shot} {:?}", r.map(|r| r.is_ok()));
                    }
                    Err(e) => eprintln!("[smoke] snapshot failed: {e}"),
                }
                let _ = slint::quit_event_loop();
            }
        });
    });
}

/// Import a cookies.txt: check it's a working login, store it encrypted, delete the original.
/// Returns the connected client and a notice for the user.
async fn import_login(path: PathBuf, solver: Solver) -> Result<(Arc<yt::Client>, String)> {
    let cookies = yt::load_cookies(&path)?;
    let client = yt::Client::connect(cookies.clone(), solver).await?;
    let c = cookies.clone();
    tokio::task::spawn_blocking(move || store::save(&c)).await??;
    let notice = match std::fs::remove_file(&path) {
        Ok(()) => format!(
            "Signed in. Your login is stored encrypted (key in the system keyring) and {} was deleted. \
             If you made other copies of it, delete those too.",
            path.display()
        ),
        Err(e) => format!("Signed in and stored encrypted, but deleting {} failed ({e}). Delete it yourself.", path.display()),
    };
    Ok((Arc::new(client), notice))
}

/// Developer override: use a plain cookies.txt (argument or $YTM_COOKIES) instead of the store.
fn dev_cookies_file() -> Option<PathBuf> {
    std::env::args().nth(1).filter(|a| !a.starts_with("--")).map(PathBuf::from).or_else(|| std::env::var("YTM_COOKIES").ok().map(PathBuf::from))
}

fn main() -> Result<()> {
    // Developer tool: `ytm --dump <browseId> <out.json>` saves a raw browse response using the
    // stored login (for working out response formats without exporting cookies).
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--version") {
        println!("ytm-native {} ({}, updates: {:?})", update::current_version(), update::platform(), update::mode());
        return Ok(());
    }
    if args.get(1).map(String::as_str) == Some("--dump") {
        let (id, out) = (args.get(2).cloned().unwrap_or_default(), args.get(3).cloned().unwrap_or("dump.json".into()));
        let rt = tokio::runtime::Runtime::new()?;
        return rt.block_on(async move {
            let cookies = tokio::task::spawn_blocking(store::load).await??.ok_or_else(|| anyhow::anyhow!("no stored login"))?;
            let cache = paths::cache_dir();
            let yt = yt::Client::connect(cookies, Solver::start(cache)).await?;
            let v = if let Some(c) = id.strip_prefix("cont:") { yt.browse(None, Some(c)).await? } else { yt.browse(Some(&id), None).await? };
            std::fs::write(&out, serde_json::to_string_pretty(&v)?)?;
            eprintln!("saved {out}");
            Ok(())
        });
    }
    // CPU rendering: this UI is mostly static, and the GPU renderer pulls ~300 MB of
    // driver libraries into the process (NVIDIA + Mesa/LLVM). SLINT_BACKEND overrides it.
    if std::env::var_os("SLINT_BACKEND").is_none() {
        slint::BackendSelector::new().backend_name("winit".into()).renderer_name("software".into()).select()?;
    }
    // Matches ytm-native.desktop, which is how KDE/Wayland finds the app's icon and name.
    let _ = slint::set_xdg_app_id("ytm-native");
    let ui = MainWindow::new()?;
    kde::apply(&ui);
    ui.set_cfg_linux(cfg!(target_os = "linux"));
    ui.set_browse(ModelRc::new(VecModel::<SongRow>::default()));
    ui.set_queue(ModelRc::new(VecModel::<SongRow>::default()));
    ui.set_account_text("Signing in…".into());

    let (tx, mut rx) = mpsc::unbounded_channel::<Ev>();
    {
        let t = tx.clone();
        ui.on_search(move |q| { let _ = t.send(Ev::Search(q.into())); });
        let t = tx.clone();
        ui.on_open_row(move |i| { let _ = t.send(Ev::OpenRow(i as usize)); });
        let t = tx.clone();
        ui.on_check_updates(move || { let _ = t.send(Ev::UpdateCheck(true)); });
        let t = tx.clone();
        ui.on_install_update(move || { let _ = t.send(Ev::UpdateInstall); });
        let t = tx.clone();
        ui.on_restart_app(move || { let _ = t.send(Ev::Restart); });
        let (t, w) = (tx.clone(), ui.as_weak());
        ui.on_settings_changed(move || {
            let Some(ui) = w.upgrade() else { return };
            let _ = t.send(Ev::SettingsChanged(settings::Settings {
                pause_history: ui.get_set_pause_history(),
                check_updates: ui.get_set_check_updates(),
                auto_update: ui.get_set_auto_update(),
                show_translation: ui.get_set_show_translation(),
            }));
        });
        let t = tx.clone();
        ui.on_near_end(move || { let _ = t.send(Ev::NearEnd); });
        let t = tx.clone();
        ui.on_show_view(move |v| { let _ = t.send(Ev::ShowView(v)); });
        let t = tx.clone();
        ui.on_play_all(move || { let _ = t.send(Ev::PlayAll); });
        let t = tx.clone();
        ui.on_shuffle_all(move || { let _ = t.send(Ev::ShuffleAll); });
        let t2 = tx.clone();
        ui.on_lyrics_search(move |title, artist| { let _ = t2.send(Ev::LyricsSearch(title.to_string(), artist.to_string())); });
        let t2 = tx.clone();
        ui.on_lyrics_pick(move |i| { let _ = t2.send(Ev::LyricsPick(i)); });
        let t = tx.clone();
        ui.on_artist_button(move |b| { let _ = t.send(Ev::ArtistButton(b.to_string())); });
        let t = tx.clone();
        ui.on_go_to_artist(move || { let _ = t.send(Ev::GoToPlayingArtist); });
        let t = tx.clone();
        ui.on_row_action(move |list, i, action| { let _ = t.send(Ev::RowAction(list, i as usize, action.to_string())); });
        let t = tx.clone();
        ui.on_back(move || { let _ = t.send(Ev::Back); });
        let t = tx.clone();
        ui.on_jump(move |i| { let _ = t.send(Ev::Jump(i as usize)); });
        let t = tx.clone();
        ui.on_toggle_play(move || { let _ = t.send(Ev::TogglePlay); });
        let t = tx.clone();
        ui.on_next(move || { let _ = t.send(Ev::Next); });
        let t = tx.clone();
        ui.on_prev(move || { let _ = t.send(Ev::Prev); });
        let t = tx.clone();
        ui.on_seek(move |s| { let _ = t.send(Ev::Seek(s)); });
        let t = tx.clone();
        ui.on_choose_cookies(move || { let _ = t.send(Ev::ChooseCookies); });
        let t = tx.clone();
        ui.on_sign_out(move || { let _ = t.send(Ev::SignOut); });
        let t = tx.clone();
        ui.on_set_speed(move |v| { let _ = t.send(Ev::Speed(v)); });
        let t = tx.clone();
        let w = ui.as_weak();
        ui.on_view_image(move |img, key| {
            if let Some(ui) = w.upgrade() {
                ui.set_lightbox_image(img);
                ui.set_lightbox_open(true);
            }
            let _ = t.send(Ev::ViewArt(key.into()));
        });
        let t = tx.clone();
        ui.on_like_clicked(move || { let _ = t.send(Ev::LikeClicked(Like::Like)); });
        let t = tx.clone();
        ui.on_dislike_clicked(move || { let _ = t.send(Ev::LikeClicked(Like::Dislike)); });
        let t = tx.clone();
        ui.on_set_volume(move |v| { let _ = t.send(Ev::Volume(v)); });
    }

    let smoke_no_state = std::env::var_os("YTM_SMOKE").is_some() && std::env::var_os("YTM_SMOKE_STATE").is_none();
    let saved = if smoke_no_state { state::Saved::default() } else { state::load() };
    let settings = if smoke_no_state { settings::Settings::default() } else { settings::load() };
    ui.set_set_pause_history(settings.pause_history);
    ui.set_set_check_updates(settings.check_updates);
    ui.set_set_auto_update(settings.auto_update);
    ui.set_set_show_translation(settings.show_translation);
    ui.set_app_version(env!("CARGO_PKG_VERSION").into());
    ui.set_repo_url(repo_url().into());
    ui.set_volume(saved.volume);
    if std::env::var("YTM_SMOKE").is_ok_and(|s| !s.is_empty()) {
        ui.set_volume(0.0); // smoke tests run silently
        // YTM_SMOKE_SIZE=<w>x<h>: window size in logical px (narrow-window checks).
        if let Some((w, h)) = std::env::var("YTM_SMOKE_SIZE").ok().and_then(|s| {
            let (w, h) = s.split_once('x')?;
            Some((w.parse::<f32>().ok()?, h.parse::<f32>().ok()?))
        }) {
            ui.window().set_size(slint::LogicalSize::new(w, h));
        }
        smoke_input(ui.as_weak());
    }
    update::clean_up();
    let audio = match Audio::start() {
        Ok(a) => Some(a),
        Err(e) => {
            ui.set_error_text(format!("No audio output: {e}").into());
            None
        }
    };
    if let Some(a) = &audio {
        a.set_volume(ui.get_volume());
        if (saved.speed - 1.0).abs() > 0.001 {
            a.set_speed(saved.speed);
        }
    }
    ui.set_speed(saved.speed);

    let cache_dir = paths::cache_dir();
    let data_dir = paths::data_dir();
    let dev_file = dev_cookies_file();
    let persist = dev_file.is_none();

    let mpris_tx = {
        let t = tx.clone();
        mpris::start(move |c| {
            let _ = t.send(Ev::Mpris(c));
        })
    };
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build()?;
    let ui_weak = ui.as_weak();
    let tx2 = tx.clone();
    rt.spawn(async move {
        let solver = Solver::start(cache_dir);
        {
            let (tx, solver) = (tx2.clone(), solver.clone());
            tokio::spawn(async move {
                let cookies = match dev_file {
                    Some(f) => yt::load_cookies(&f).map(Some),
                    None => tokio::task::spawn_blocking(store::load).await.unwrap_or_else(|e| Err(e.into())),
                };
                let ev = match cookies {
                    Ok(Some(c)) => Ev::Connected(yt::Client::connect(c, solver).await.map(Arc::new).map_err(|e| format!("{e:#}"))),
                    Ok(None) => Ev::NeedSignIn(String::new()),
                    Err(e) => Ev::NeedSignIn(format!("Couldn't load the stored login: {e:#}")),
                };
                let _ = tx.send(ev);
            });
        }
        // First update check soon after startup (smoke tests only with a test server).
        if std::env::var_os("YTM_SMOKE").is_none() || std::env::var_os("YTM_UPDATE_API").is_some() {
            let tx = tx2.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(8)).await;
                let _ = tx.send(Ev::UpdateCheck(false));
            });
        }
        {
            let tx = tx2.clone();
            tokio::spawn(async move {
                let mut iv = tokio::time::interval(Duration::from_millis(250));
                loop {
                    iv.tick().await;
                    if tx.send(Ev::Tick).is_err() {
                        break;
                    }
                }
            });
        }
        let mut app = App {
            ui: ui_weak,
            tx: tx2,
            yt: None,
            audio,
            results: vec![],
            view: View::Home,
            back: Vec::new(),
            artist: None,
            rows: vec![],
            home: None,
            home_next: None,
            home_more_loading: false,
            home_error: String::new(),
            liked: None,
            library: None,
            library_error: String::new(),
            library_loading: false,
            collection: None,
            collection_badge: "PLAYLIST",
            browse_gen: 0,
            browse_error: String::new(),
            queue: vec![],
            manual_end: 0,
            current: None,
            played: HashSet::new(),
            radio: None,
            history: History::load(data_dir.join("history.json")),
            art: Default::default(),
            art_pending: HashSet::new(),
            art_limit: Arc::new(Semaphore::new(6)),
            prefetch: HashMap::new(),
            playing_stream: None,
            load_started: None,
            kde_changed_at: kde::settings_changed_at(),
            prefetching: HashSet::new(),
            search_gen: 0,
            play_gen: 0,
            radio_gen: 0,
            loading: false,
            paused: false,
            duration: 0.0,
            pending_advance: false,
            report: None,
            finder: lyrics::Finder::new().ok().map(Arc::new),
            lyrics: HashMap::new(),
            lyrics_pending: HashSet::new(),
            lyrics_found: HashMap::new(),
            settings: settings.clone(),
            updater: update::Updater::new().ok().map(Arc::new),
            update: None,
            update_busy: false,
            update_ready: false,
            lyrics_searching: false,
            mpris: Some(mpris_tx),
            mpris_status: None,
            volume: saved.volume,
            speed: saved.speed,
            lightbox_gen: 0,
            saved: Some(saved),
            last_saved: None,
            resume_at: None,
            signed_out: false,
            solver: solver.clone(),
            persist,
            importing: false,
            ticks: 0,
            fail_streak: 0,
            like: None,
            smoke_seek_done: false,
            smoke: std::env::var("YTM_SMOKE").ok().filter(|s| !s.is_empty()),
        };
        while let Some(ev) = rx.recv().await {
            app.handle(ev);
        }
    });

    ui.run()?;
    // Closing: report the last bit of listening and save where we were.
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    if tx.send(Ev::Shutdown(done_tx)).is_ok() {
        let _ = rt.block_on(async { tokio::time::timeout(Duration::from_secs(2), done_rx).await });
    }
    drop(tx);
    rt.shutdown_timeout(Duration::from_millis(500));
    if RESTART.load(std::sync::atomic::Ordering::Relaxed) {
        update::relaunch();
    }
    Ok(())
}
