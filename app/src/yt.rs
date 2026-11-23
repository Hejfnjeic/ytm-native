//! YouTube Music InnerTube client: session, search, radio, and audio streams.
//!
//! Identities used:
//! - `WEB_REMIX` (the music.youtube.com website) for search and radio. It's what the site
//!   itself sends, with your login, so recommendations are personal.
//! - `TVHTML5` as a Samsung TV for playback only: logged in, it serves the Premium 256 kbps
//!   Opus stream without needing a proof-of-origin token.

use std::{collections::HashMap, fs, path::Path, sync::Arc, time::Duration};

use anyhow::{Context, Result, anyhow, bail};
use regex::Regex;
use serde_json::{Value, json};
use sha1::{Digest, Sha1};
use tokio::sync::Mutex;

use crate::solver::Solver;

const MUSIC: &str = "https://music.youtube.com";
const YT: &str = "https://www.youtube.com";
/// A Chrome user agent. Not Firefox: YouTube's front end resets HTTP/2 requests that claim to
/// be Firefox but don't look like Firefox on the wire (found Nov 2026; Chrome's is accepted).
const UA_WEB: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/142.0.0.0 Safari/537.36";

const TV_VERSION: &str = "5.20260707";
const TV_UA: &str = "Mozilla/5.0 (SMART-TV; Linux; Tizen 2.4.0) AppleWebKit/538.1 (KHTML, like Gecko) Version/2.4.0 TV Safari/538.1";

/// Error text for a login YouTube no longer accepts (HTTP 401). The UI matches on it.
pub const SIGNED_OUT: &str = "YouTube no longer accepts this login (HTTP 401)";

/// "Songs" filter for search.
const SEARCH_SONGS: &str = "EgWKAQIIAWoQEAMQBBAJEAoQBRAREBAQFQ%3D%3D";

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Song {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration: String,
    pub thumb: Option<String>,
    /// Browse ids of the (first) artist and the album, when YouTube links them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artist_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album_id: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Song,
    Album,
    Playlist,
    Artist,
}

/// Something on a Home shelf: a song, or something that opens (album, playlist, artist).
#[derive(Clone, Debug)]
pub struct Item {
    pub kind: Kind,
    /// Song data for songs; for others: title/artist(=subtitle)/thumb, id = browse id.
    pub song: Song,
}

pub struct Shelf {
    pub title: String,
    pub items: Vec<Item>,
}

#[derive(Clone)]
pub struct Collection {
    pub title: String,
    pub subtitle: String,
    pub thumb: Option<String>,
    pub artist: String,
    pub tracks: Vec<Song>,
    pub continuation: Option<String>,
}

/// A "watch" playlist YouTube builds on request (an artist's Mix or Shuffle).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Watch {
    pub video_id: String,
    pub playlist_id: String,
    pub params: String,
}

impl Watch {
    fn from_endpoint(we: &Value) -> Option<Self> {
        Some(Watch {
            video_id: we["videoId"].as_str()?.to_string(),
            playlist_id: we["playlistId"].as_str()?.to_string(),
            params: we["params"].as_str().unwrap_or_default().to_string(),
        })
    }
}

pub struct Artist {
    pub name: String,
    /// "25.9M monthly audience · 148K subscribers"
    pub subtitle: String,
    pub thumb: Option<String>,
    /// Top songs, albums, singles, videos, playlists, similar artists.
    pub shelves: Vec<Shelf>,
    /// Playlist with all of the artist's songs ("VL…").
    pub all_songs: Option<String>,
    pub mix: Option<Watch>,
    pub shuffle: Option<Watch>,
}

pub struct RadioPage {
    pub songs: Vec<Song>,
    pub continuation: Option<String>,
}

pub struct Stream {
    pub url: url::Url,
    pub content_length: u64,
    pub itag: u64,
    /// Seconds (0 if YouTube didn't say).
    pub duration: f64,
}

#[derive(Clone)]
pub struct Tracking {
    pub playback: String,
    pub watchtime: String,
    /// Watch-time pings at these seconds of real time, then every `interval` seconds.
    pub scheduled: Vec<f64>,
    pub interval: f64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Like {
    Like,
    Dislike,
    Indifferent,
}

struct Player {
    checked: std::time::Instant,
    id: String,
    js: Arc<String>,
    sts: u64,
}

pub struct Client {
    /// HTTP/2 when the server offers it, …
    http: reqwest::Client,
    /// … and HTTP/1.1 only, used for the rest of the session after an HTTP/2 protocol error
    /// (a safety net; see UA_WEB for the one cause found so far). `YTM_HTTP1=1` forces it.
    http1: reqwest::Client,
    h2_off: std::sync::atomic::AtomicBool,
    /// Live cookie set: starts from the import, then follows YouTube's Set-Cookie updates.
    cookies: std::sync::Mutex<HashMap<String, String>>,
    /// Set when YouTube updated a cookie since the last `take_changed`.
    changed: std::sync::atomic::AtomicBool,
    visitor: String,
    datasync: Option<String>,
    web_version: String,
    player: Mutex<Option<Player>>,
    solver: Solver,
}

/// An HTTP/2-level failure (not a timeout or a refused connection).
fn is_protocol_error(e: &reqwest::Error) -> bool {
    let mut src: Option<&dyn std::error::Error> = Some(e);
    while let Some(err) = src {
        let text = err.to_string();
        if text.contains("protocol error") || text.contains("stream error") || text.contains("GOAWAY") {
            return true;
        }
        src = err.source();
    }
    false
}

fn capture(re: &str, text: &str) -> Option<String> {
    Regex::new(re).ok()?.captures(text).map(|c| c[1].to_string())
}

fn runs_text(v: &Value) -> String {
    v["runs"].as_array().map(|r| r.iter().filter_map(|x| x["text"].as_str()).collect()).unwrap_or_default()
}

/// First artist and album browse ids linked from text runs (artist names, album names).
fn run_links(runs: &[&Value]) -> (Option<String>, Option<String>) {
    let (mut artist, mut album) = (None, None);
    for r in runs.iter().flat_map(|v| v["runs"].as_array().into_iter().flatten()) {
        let be = &r["navigationEndpoint"]["browseEndpoint"];
        let Some(id) = be["browseId"].as_str() else { continue };
        match be.pointer("/browseEndpointContextSupportedConfigs/browseEndpointContextMusicConfig/pageType").and_then(Value::as_str) {
            Some("MUSIC_PAGE_TYPE_ARTIST" | "MUSIC_PAGE_TYPE_USER_CHANNEL") if artist.is_none() => artist = Some(id.to_string()),
            Some("MUSIC_PAGE_TYPE_ALBUM") if album.is_none() => album = Some(id.to_string()),
            _ => {}
        }
    }
    (artist, album)
}

fn last_thumb(v: &Value) -> Option<String> {
    v.as_array()?.last()?["url"].as_str().map(str::to_string)
}

/// A Home shelf item (two-row card or list row).
fn parse_item(it: &Value) -> Option<Item> {
    if let Some(r) = it.get("musicTwoRowItemRenderer") {
        let nav = &r["navigationEndpoint"];
        let thumb = last_thumb(&r["thumbnailRenderer"]["musicThumbnailRenderer"]["thumbnail"]["thumbnails"]);
        let title = runs_text(&r["title"]);
        let subtitle = runs_text(&r["subtitle"]);
        if let Some(id) = nav["watchEndpoint"]["videoId"].as_str() {
            // Subtitle is like "Song • Artist" or "Artist • 1.2M views".
            let parts: Vec<&str> = subtitle.split(" • ").filter(|p| !matches!(*p, "Song" | "Video" | "Single")).collect();
            let (artist_id, album_id) = run_links(&[&r["subtitle"]]);
            return Some(Item { kind: Kind::Song, song: Song {
                id: id.into(), title, artist: parts.first().unwrap_or(&"").to_string(), thumb, artist_id, album_id, ..Default::default()
            }});
        }
        let be = &nav["browseEndpoint"];
        let id = be["browseId"].as_str()?;
        let kind = match be["browseEndpointContextSupportedConfigs"]["browseEndpointContextMusicConfig"]["pageType"].as_str().unwrap_or("") {
            "MUSIC_PAGE_TYPE_ALBUM" | "MUSIC_PAGE_TYPE_AUDIOBOOK" => Kind::Album,
            "MUSIC_PAGE_TYPE_PLAYLIST" => Kind::Playlist,
            "MUSIC_PAGE_TYPE_ARTIST" | "MUSIC_PAGE_TYPE_USER_CHANNEL" => Kind::Artist,
            _ => return None,
        };
        return Some(Item { kind, song: Song { id: id.into(), title, artist: subtitle, thumb, ..Default::default() } });
    }
    let r = it.get("musicResponsiveListItemRenderer")?;
    let song = parse_list_song(r, "", &None)?;
    Some(Item { kind: Kind::Song, song })
}

/// A song row in a list (search results, playlists, albums, Quick picks).
fn parse_list_song(r: &Value, fallback_artist: &str, fallback_thumb: &Option<String>) -> Option<Song> {
    let id = r["playlistItemData"]["videoId"].as_str()
        .or_else(|| r.pointer("/overlay/musicItemThumbnailOverlayRenderer/content/musicPlayButtonRenderer/playNavigationEndpoint/watchEndpoint/videoId").and_then(Value::as_str))?;
    let col = |i: usize| runs_text(&r["flexColumns"][i]["musicResponsiveListItemFlexColumnRenderer"]["text"]);
    let title = col(0);
    let c1 = col(1);
    let (artist, album, mut duration) = if c1.contains(" • ") {
        // Search style: "Artist • Album • 3:45" (or "Song • Artist • …").
        let parts: Vec<&str> = c1.split(" • ").filter(|p| !matches!(*p, "Song" | "Video")).collect();
        let dur = parts.last().filter(|p| p.contains(':')).map(|p| p.to_string()).unwrap_or_default();
        let album = if parts.len() >= 3 { parts[1].to_string() } else { String::new() };
        (parts.first().unwrap_or(&"").to_string(), album, dur)
    } else {
        // Playlist/album style: separate columns (album rows leave the artist empty).
        let c2 = col(2);
        let album = if c2.contains("plays") || c2.contains("views") { String::new() } else { c2 };
        (if c1.is_empty() { fallback_artist.to_string() } else { c1 }, album, String::new())
    };
    if duration.is_empty() {
        duration = runs_text(&r["fixedColumns"][0]["musicResponsiveListItemFixedColumnRenderer"]["text"]);
    }
    let thumb = last_thumb(&r["thumbnail"]["musicThumbnailRenderer"]["thumbnail"]["thumbnails"]).or_else(|| fallback_thumb.clone());
    let cols: Vec<&Value> = (1..4).map(|i| &r["flexColumns"][i]["musicResponsiveListItemFlexColumnRenderer"]["text"]).collect();
    let (artist_id, album_id) = run_links(&cols);
    Some(Song { id: id.to_string(), title, artist, album, duration, thumb, artist_id, album_id })
}

/// Track rows plus the continuation token (last item) of a playlist/album shelf.
fn parse_track_list(items: &Value, artist: &str, thumb: &Option<String>) -> (Vec<Song>, Option<String>) {
    let mut songs = Vec::new();
    let mut continuation = None;
    for it in items.as_array().into_iter().flatten() {
        if let Some(r) = it.get("musicResponsiveListItemRenderer") {
            // Unavailable tracks have no video id and are skipped.
            if let Some(s) = parse_list_song(r, artist, thumb) {
                songs.push(s);
            }
        } else if let Some(t) = it.pointer("/continuationItemRenderer/continuationEndpoint/continuationCommand/token").and_then(Value::as_str) {
            continuation = Some(t.to_string());
        }
    }
    (songs, continuation)
}

/// Pages fetched per library list (a page is 25–100 entries).
const LIBRARY_MAX_PAGES: u32 = 40;

/// The user's saved things (Library).
#[derive(Default)]
pub struct Library {
    pub playlists: Vec<Item>,
    pub albums: Vec<Item>,
    pub songs: Vec<Song>,
    pub artists: Vec<Item>,
}

/// A library row that opens something: an artist (followed, or "artist of saved songs"),
/// album or playlist. Library-artist ids ("MPLA" + channel id) become the artist's own page.
fn parse_library_row(r: &Value) -> Option<Item> {
    let be = &r["navigationEndpoint"]["browseEndpoint"];
    let id = be["browseId"].as_str()?;
    let page = be.pointer("/browseEndpointContextSupportedConfigs/browseEndpointContextMusicConfig/pageType").and_then(Value::as_str)?;
    let col = |i: usize| runs_text(&r["flexColumns"][i]["musicResponsiveListItemFlexColumnRenderer"]["text"]);
    let (kind, id) = match page {
        "MUSIC_PAGE_TYPE_ARTIST" | "MUSIC_PAGE_TYPE_USER_CHANNEL" => (Kind::Artist, id),
        "MUSIC_PAGE_TYPE_LIBRARY_ARTIST" => (Kind::Artist, id.strip_prefix("MPLA").unwrap_or(id)),
        "MUSIC_PAGE_TYPE_ALBUM" => (Kind::Album, id),
        "MUSIC_PAGE_TYPE_PLAYLIST" => (Kind::Playlist, id),
        _ => return None,
    };
    let thumb = last_thumb(&r["thumbnail"]["musicThumbnailRenderer"]["thumbnail"]["thumbnails"]);
    Some(Item { kind, song: Song { id: id.into(), title: col(0), artist: col(1), thumb, ..Default::default() } })
}

/// Items and the continuation of a library page (first page or a continuation; grid of cards
/// or a list of rows; old- and new-style continuations).
fn parse_library_page(v: &Value) -> (Vec<Item>, Option<String>) {
    let mut lists: Vec<&Value> = Vec::new();
    if let Some(sections) = v.pointer("/contents/singleColumnBrowseResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer/contents").and_then(Value::as_array) {
        for s in sections {
            if let Some(x) = s.get("gridRenderer").or_else(|| s.get("musicShelfRenderer")) {
                lists.push(x);
            }
        }
    }
    for key in ["gridContinuation", "musicShelfContinuation"] {
        if let Some(x) = v["continuationContents"].get(key) {
            lists.push(x);
        }
    }
    let appended = v.pointer("/onResponseReceivedActions/0/appendContinuationItemsAction/continuationItems");
    let mut items = Vec::new();
    let mut next = None;
    let mut take = |entries: &Value| {
        for it in entries.as_array().into_iter().flatten() {
            if let Some(t) = it.pointer("/continuationItemRenderer/continuationEndpoint/continuationCommand/token").and_then(Value::as_str) {
                next = Some(t.to_string());
            } else if let Some(r) = it.get("musicResponsiveListItemRenderer") {
                // Songs have a video id; the "Shuffle all" row and artists don't.
                if let Some(song) = parse_list_song(r, "", &None) {
                    items.push(Item { kind: Kind::Song, song });
                } else if let Some(item) = parse_library_row(r) {
                    items.push(item);
                }
            } else if let Some(item) = parse_item(it) {
                items.push(item);
            }
        }
    };
    for l in &lists {
        take(l.get("items").unwrap_or(&l["contents"]));
    }
    if let Some(a) = appended {
        take(a);
    }
    if next.is_none() {
        next = lists.iter().find_map(|l| l.pointer("/continuations/0/nextContinuationData/continuation")).and_then(Value::as_str).map(str::to_string);
    }
    (items, next)
}

/// Netscape cookies.txt → name/value pairs for youtube.com.
pub fn load_cookies(path: &Path) -> Result<HashMap<String, String>> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut cookies = HashMap::new();
    for line in text.lines() {
        let line = line.strip_prefix("#HttpOnly_").unwrap_or(line);
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() >= 7 && f[0].trim_start_matches('.').ends_with("youtube.com") {
            cookies.insert(f[5].to_string(), f[6].trim_end().to_string());
        }
    }
    if !cookies.contains_key("SAPISID") {
        bail!("no SAPISID cookie in {}: is it a logged-in youtube.com export?", path.display());
    }
    Ok(cookies)
}

impl Client {
    pub async fn connect(cookies: HashMap<String, String>, solver: Solver) -> Result<Self> {
        let cookie_header = cookies.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("; ");
        // HTTP/2 to YouTube fails from reqwest here with a protocol error; HTTP/1.1 works.
        let builder = || reqwest::Client::builder()
            .gzip(true)
            .brotli(true)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30));
        let http = builder().build()?;
        let http1 = builder().http1_only().build()?;
        let h2_off = std::sync::atomic::AtomicBool::new(std::env::var_os("YTM_HTTP1").is_some());
        let first = if h2_off.load(std::sync::atomic::Ordering::Relaxed) { &http1 } else { &http };

        let resp = first.get(MUSIC).header("User-Agent", UA_WEB).header("Cookie", &cookie_header).send().await?.error_for_status()?;
        eprintln!("[http] connected to YouTube over {:?}", resp.version());
        let page = resp.text().await?;
        if !page.contains(r#""LOGGED_IN":true"#) {
            bail!("YouTube doesn't see you as logged in with these cookies (expired, or you logged out?)");
        }
        Ok(Self {
            visitor: capture(r#""VISITOR_DATA":"([^"]+)""#, &page).ok_or_else(|| anyhow!("no VISITOR_DATA in page"))?,
            datasync: capture(r#""DATASYNC_ID":"([^"]+)""#, &page).map(|d| d.split("||").next().unwrap_or("").to_string()),
            web_version: capture(r#""INNERTUBE_CLIENT_VERSION":"([^"]+)""#, &page).unwrap_or_else(|| "1.20261117.06.00".into()),
            http,
            http1,
            h2_off,
            cookies: std::sync::Mutex::new(cookies),
            changed: Default::default(),
            player: Mutex::new(None),
            solver,
        })
    }

    /// True once after YouTube refreshed any cookie (time to save them again).
    /// Send a request; after an HTTP/2 protocol error, retry it (and everything after it) over
    /// HTTP/1.1.
    async fn send(&self, req: impl Fn(&reqwest::Client) -> reqwest::RequestBuilder) -> reqwest::Result<reqwest::Response> {
        use std::sync::atomic::Ordering::Relaxed;
        if self.h2_off.load(Relaxed) {
            return req(&self.http1).send().await;
        }
        match req(&self.http).send().await {
            Err(e) if is_protocol_error(&e) => {
                eprintln!("[http] HTTP/2 failed ({e}); using HTTP/1.1 from now on");
                self.h2_off.store(true, Relaxed);
                req(&self.http1).send().await
            }
            r => r,
        }
    }

    pub fn take_changed(&self) -> bool {
        self.changed.swap(false, std::sync::atomic::Ordering::Relaxed)
    }

    pub fn cookies(&self) -> HashMap<String, String> {
        self.cookies.lock().unwrap().clone()
    }

    fn cookie_header(&self) -> String {
        self.cookies.lock().unwrap().iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("; ")
    }

    fn auth(&self, origin: &str) -> String {
        let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        let cookies = self.cookies.lock().unwrap();
        [("SAPISIDHASH", "SAPISID"), ("SAPISID1PHASH", "__Secure-1PAPISID"), ("SAPISID3PHASH", "__Secure-3PAPISID")]
            .iter()
            .filter_map(|(scheme, name)| cookies.get(*name).map(|v| (scheme, v)))
            .map(|(s, v)| format!("{s} {ts}_{}", hex::encode(Sha1::digest(format!("{ts} {v} {origin}")))))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Apply YouTube's cookie updates (Google refreshes some login cookies over time), so our
    /// copy keeps up instead of going stale. Deletions are ignored: if YouTube is clearing the
    /// login, the session is gone anyway and keeping the old values loses nothing.
    /// In memory only for now; persisting them belongs to the encrypted store (TODO.md).
    fn absorb(&self, resp: &reqwest::Response) {
        let mut changed = Vec::new();
        let mut cookies = self.cookies.lock().unwrap();
        for h in resp.headers().get_all(reqwest::header::SET_COOKIE) {
            let Ok(h) = h.to_str() else { continue };
            let mut parts = h.split(';').map(str::trim);
            let Some((name, value)) = parts.next().and_then(|kv| kv.split_once('=')) else { continue };
            let attrs: Vec<String> = parts.map(str::to_ascii_lowercase).collect();
            let deleting = value.is_empty()
                || attrs.iter().any(|a| a == "max-age=0" || a.starts_with("expires=") && (a.contains("1970") || a.contains("1990")));
            let domain_ok = attrs.iter().filter_map(|a| a.strip_prefix("domain=")).all(|d| d.trim_start_matches('.').ends_with("youtube.com"));
            if deleting || !domain_ok {
                continue;
            }
            if cookies.get(name).map(String::as_str) != Some(value) {
                cookies.insert(name.to_string(), value.to_string());
                changed.push(name.to_string());
            }
        }
        if !changed.is_empty() {
            self.changed.store(true, std::sync::atomic::Ordering::Relaxed);
            eprintln!("[auth] YouTube refreshed cookies: {}", changed.join(", "));
        }
    }

    fn web_context(&self) -> Value {
        let mut c = json!({ "client": {
            "clientName": "WEB_REMIX", "clientVersion": self.web_version, "hl": "en", "gl": "US",
            "platform": "DESKTOP", "userAgent": UA_WEB, "visitorData": self.visitor,
        }});
        if let Some(id) = &self.datasync {
            c["user"] = json!({ "onBehalfOfUser": id });
        }
        c
    }

    async fn web_post(&self, endpoint: &str, mut body: Value) -> Result<Value> {
        body["context"] = self.web_context();
        let resp = self.send(|c| c.post(format!("{MUSIC}/youtubei/v1/{endpoint}?prettyPrint=false"))
            .header("User-Agent", UA_WEB)
            .header("Origin", MUSIC)
            .header("Referer", format!("{MUSIC}/"))
            .header("X-Youtube-Client-Name", "67")
            .header("X-Youtube-Client-Version", &self.web_version)
            .header("X-Goog-Visitor-Id", &self.visitor)
            .header("X-Goog-AuthUser", "0")
            .header("Cookie", self.cookie_header())
            .header("Authorization", self.auth(MUSIC))
            .json(&body)
            ).await?;
        self.absorb(&resp);
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            if status == reqwest::StatusCode::UNAUTHORIZED {
                bail!("{SIGNED_OUT}");
            }
            bail!("{endpoint}: HTTP {status}");
        }
        Ok(serde_json::from_str(&text)?)
    }

    pub async fn search(&self, query: &str) -> Result<Vec<Song>> {
        let params = url::form_urlencoded::parse(SEARCH_SONGS.as_bytes()).next().map(|(k, _)| k.into_owned()).unwrap_or_default();
        let v = self.web_post("search", json!({ "query": query, "params": params })).await?;
        let sections = v.pointer("/contents/tabbedSearchResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer/contents")
            .and_then(Value::as_array).cloned().unwrap_or_default();
        let mut songs = Vec::new();
        for s in sections {
            for item in s["musicShelfRenderer"]["contents"].as_array().into_iter().flatten() {
                if let Some(song) = item.get("musicResponsiveListItemRenderer").and_then(|r| parse_list_song(r, "", &None)) {
                    songs.push(song);
                }
            }
        }
        Ok(songs)
    }

    // ---------- library: Home feed, albums, playlists ----------

    /// One page of the Home feed (personal shelves). Pass the returned continuation for more.
    pub async fn home(&self, continuation: Option<&str>) -> Result<(Vec<Shelf>, Option<String>)> {
        let v = self.browse(Some("FEmusic_home"), continuation).await?;
        let list = v
            .pointer("/contents/singleColumnBrowseResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer")
            .or_else(|| v.pointer("/continuationContents/sectionListContinuation"))
            .ok_or_else(|| anyhow!("unexpected Home response"))?;
        let shelves = list["contents"].as_array().into_iter().flatten().filter_map(|c| {
            let shelf = c.get("musicCarouselShelfRenderer")?;
            let title = runs_text(&shelf["header"]["musicCarouselShelfBasicHeaderRenderer"]["title"]);
            let items: Vec<Item> = shelf["contents"].as_array()?.iter().filter_map(parse_item).collect();
            (!items.is_empty()).then_some(Shelf { title, items })
        }).collect();
        let next = list.pointer("/continuations/0/nextContinuationData/continuation").and_then(Value::as_str).map(str::to_string);
        Ok((shelves, next))
    }

    /// An album or playlist page ("MPREb_…" album ids, "VL…" playlist ids).
    pub async fn collection(&self, browse_id: &str) -> Result<Collection> {
        let v = self.browse(Some(browse_id), None).await?;
        let c = &v["contents"]["twoColumnBrowseResultsRenderer"];
        let h = c.pointer("/tabs/0/tabRenderer/content/sectionListRenderer/contents/0/musicResponsiveHeaderRenderer")
            .ok_or_else(|| anyhow!("unexpected album/playlist response"))?;
        let title = runs_text(&h["title"]);
        let subtitle = [runs_text(&h["subtitle"]), runs_text(&h["straplineTextOne"]), runs_text(&h["secondSubtitle"])]
            .into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" · ");
        let thumb = last_thumb(&h["thumbnail"]["musicThumbnailRenderer"]["thumbnail"]["thumbnails"]);
        let artist = runs_text(&h["straplineTextOne"]);
        let shelf = c.pointer("/secondaryContents/sectionListRenderer/contents/0")
            .and_then(|s| s.get("musicPlaylistShelfRenderer").or_else(|| s.get("musicShelfRenderer")))
            .ok_or_else(|| anyhow!("no track list"))?;
        let (tracks, continuation) = parse_track_list(&shelf["contents"], &artist, &thumb);
        Ok(Collection { title, subtitle, thumb, artist, tracks, continuation })
    }

    /// More tracks of a long playlist.
    pub async fn collection_more(&self, continuation: &str, artist: &str, thumb: &Option<String>) -> Result<(Vec<Song>, Option<String>)> {
        let v = self.browse(None, Some(continuation)).await?;
        let items = v.pointer("/onResponseReceivedActions/0/appendContinuationItemsAction/continuationItems")
            .ok_or_else(|| anyhow!("unexpected playlist continuation"))?;
        Ok(parse_track_list(items, artist, thumb))
    }

    /// Everything saved in the library: playlists, albums, songs and artists (followed ones
    /// first, then the artists of saved songs). Each list is fetched with all its pages.
    pub async fn library(&self) -> Result<Library> {
        let (playlists, albums, songs, subs, track_artists) = tokio::try_join!(
            self.library_list("FEmusic_liked_playlists"),
            self.library_list("FEmusic_liked_albums"),
            self.library_list("FEmusic_liked_videos"),
            self.library_list("FEmusic_library_corpus_artists"),
            self.library_list("FEmusic_library_corpus_track_artists"),
        )?;
        let mut artists: Vec<Item> = Vec::new();
        for a in subs.into_iter().chain(track_artists).filter(|i| i.kind == Kind::Artist) {
            if !artists.iter().any(|x| x.song.id == a.song.id) {
                artists.push(a);
            }
        }
        Ok(Library {
            // "Episodes for Later" (VLSE) is podcasts, not music.
            playlists: playlists.into_iter().filter(|i| i.kind == Kind::Playlist && i.song.id != "VLSE").collect(),
            albums: albums.into_iter().filter(|i| i.kind == Kind::Album).collect(),
            songs: songs.into_iter().filter(|i| i.kind == Kind::Song).map(|i| i.song).collect(),
            artists,
        })
    }

    /// One library list with all its pages (bounded).
    async fn library_list(&self, browse_id: &str) -> Result<Vec<Item>> {
        let first = self.browse(Some(browse_id), None).await?;
        let (mut items, mut next) = parse_library_page(&first);
        let mut pages = 1;
        while let Some(c) = next.take() {
            if pages >= LIBRARY_MAX_PAGES {
                break;
            }
            let v = self.browse(None, Some(&c)).await?;
            let (more, n) = parse_library_page(&v);
            if more.is_empty() {
                break;
            }
            items.extend(more);
            next = n;
            pages += 1;
        }
        Ok(items)
    }

    /// An artist's page ("UC…" channel ids).
    pub async fn artist(&self, browse_id: &str) -> Result<Artist> {
        let v = self.browse(Some(browse_id), None).await?;
        let h = v["header"].get("musicImmersiveHeaderRenderer").or_else(|| v["header"].get("musicVisualHeaderRenderer"))
            .ok_or_else(|| anyhow!("unexpected artist response"))?;
        let name = runs_text(&h["title"]);
        let subtitle = [runs_text(&h["monthlyListenerCount"]),
                        runs_text(&h["subscriptionButton"]["subscribeButtonRenderer"]["longSubscriberCountText"])]
            .into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" · ");
        let thumb = last_thumb(&h["thumbnail"]["musicThumbnailRenderer"]["thumbnail"]["thumbnails"])
            .or_else(|| last_thumb(&h["foregroundThumbnail"]["musicThumbnailRenderer"]["thumbnail"]["thumbnails"]));
        let watch = |b: &str| Watch::from_endpoint(&h[b]["buttonRenderer"]["navigationEndpoint"]["watchEndpoint"]);
        let (mix, shuffle) = (watch("startRadioButton"), watch("playButton"));
        let mut shelves = Vec::new();
        let mut all_songs = None;
        let sections = v.pointer("/contents/singleColumnBrowseResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer/contents")
            .and_then(Value::as_array).cloned().unwrap_or_default();
        for sec in &sections {
            if let Some(sh) = sec.get("musicShelfRenderer") {
                let title = runs_text(&sh["title"]);
                let items: Vec<Item> = sh["contents"].as_array().into_iter().flatten()
                    .filter_map(|it| parse_list_song(it.get("musicResponsiveListItemRenderer")?, &name, &None))
                    .map(|song| Item { kind: Kind::Song, song }).collect();
                if all_songs.is_none() {
                    all_songs = sh.pointer("/bottomEndpoint/browseEndpoint/browseId").and_then(Value::as_str).map(str::to_string);
                }
                if !items.is_empty() {
                    shelves.push(Shelf { title, items });
                }
            } else if let Some(sh) = sec.get("musicCarouselShelfRenderer") {
                let title = runs_text(&sh["header"]["musicCarouselShelfBasicHeaderRenderer"]["title"]);
                let items: Vec<Item> = sh["contents"].as_array().into_iter().flatten().filter_map(parse_item).collect();
                if !items.is_empty() {
                    shelves.push(Shelf { title, items });
                }
            }
        }
        Ok(Artist { name, subtitle, thumb, shelves, all_songs, mix, shuffle })
    }

    /// Raw browse request (library pages, playlists, continuations).
    pub async fn browse(&self, browse_id: Option<&str>, continuation: Option<&str>) -> Result<Value> {
        match continuation {
            Some(c) => self.web_post("browse", json!({ "continuation": c })).await,
            None => self.web_post("browse", json!({ "browseId": browse_id.unwrap_or_default() })).await,
        }
    }

    /// First radio page for a seed song, or the next page given a continuation.
    /// `watch` starts from a playlist YouTube builds (an artist's Mix/Shuffle) instead of the
    /// seed song's own radio.
    pub async fn radio(&self, seed: &str, watch: Option<&Watch>, continuation: Option<&str>) -> Result<RadioPage> {
        let body = match (continuation, watch) {
            (None, Some(w)) => json!({ "videoId": w.video_id, "playlistId": w.playlist_id, "params": w.params, "isAudioOnly": true,
                                       "enablePersistentPlaylistPanel": true, "tunerSettingValue": "AUTOMIX_SETTING_NORMAL" }),
            (None, None) => json!({ "videoId": seed, "playlistId": format!("RDAMVM{seed}"), "isAudioOnly": true,
                            "enablePersistentPlaylistPanel": true, "tunerSettingValue": "AUTOMIX_SETTING_NORMAL" }),
            (Some(c), _) => json!({ "continuation": c, "isAudioOnly": true, "enablePersistentPlaylistPanel": true }),
        };
        let v = self.web_post("next", body).await?;
        let panel = v
            .pointer("/contents/singleColumnMusicWatchNextResultsRenderer/tabbedRenderer/watchNextTabbedResultsRenderer/tabs/0/tabRenderer/content/musicQueueRenderer/content/playlistPanelRenderer")
            .or_else(|| v.pointer("/continuationContents/playlistPanelContinuation"))
            .ok_or_else(|| anyhow!("no radio in response"))?;
        let songs = panel["contents"].as_array().into_iter().flatten().filter_map(|item| {
            let r = item.get("playlistPanelVideoRenderer")
                .or_else(|| item.pointer("/playlistPanelVideoWrapperRenderer/primaryRenderer/playlistPanelVideoRenderer"))?;
            let byline = runs_text(&r["longBylineText"]);
            let parts: Vec<&str> = byline.split(" • ").collect();
            let (artist_id, album_id) = run_links(&[&r["longBylineText"]]);
            Some(Song {
                artist_id,
                album_id,
                id: r["videoId"].as_str()?.to_string(),
                title: runs_text(&r["title"]),
                artist: parts.first().unwrap_or(&"").to_string(),
                album: if parts.len() >= 3 { parts[1].to_string() } else { String::new() },
                duration: runs_text(&r["lengthText"]),
                thumb: last_thumb(&r["thumbnail"]["thumbnails"]),
            })
        }).collect();
        let continuation = panel
            .pointer("/continuations/0/nextRadioContinuationData/continuation")
            .or_else(|| panel.pointer("/continuations/0/nextContinuationData/continuation"))
            .and_then(Value::as_str)
            .map(str::to_string);
        Ok(RadioPage { songs, continuation })
    }

    /// Current player JS. Re-checks YouTube's current player version at most every 30 minutes.
    async fn player(&self) -> Result<(Arc<String>, String, u64)> {
        let mut guard = self.player.lock().await;
        if let Some(p) = guard.as_ref().filter(|p| p.checked.elapsed() < Duration::from_secs(1800)) {
            return Ok((p.js.clone(), p.id.clone(), p.sts));
        }
        let iframe = self.send(|c| c.get(format!("{YT}/iframe_api")).header("User-Agent", UA_WEB)).await?.text().await?;
        let id = capture(r#"player\\?/([0-9a-fA-F]{8})\\?/"#, &iframe).ok_or_else(|| anyhow!("player id not found"))?;
        if let Some(p) = guard.as_mut().filter(|p| p.id == id) {
            p.checked = std::time::Instant::now();
            return Ok((p.js.clone(), p.id.clone(), p.sts));
        }
        let js = self.send(|c| c.get(format!("{YT}/s/player/{id}/player_ias.vflset/en_US/base.js"))
            .header("User-Agent", UA_WEB)).await?.error_for_status()?.text().await?;
        let sts = capture(r"(?:signatureTimestamp|sts)\s*[:=]\s*(\d{5})", &js)
            .ok_or_else(|| anyhow!("signatureTimestamp not found in player"))?.parse()?;
        let p = Player { checked: std::time::Instant::now(), id: id.clone(), js: Arc::new(js), sts };
        let out = (p.js.clone(), p.id.clone(), p.sts);
        *guard = Some(p);
        Ok(out)
    }

    /// Best audio stream (Premium Opus if available) with a solved, playable URL.
    pub async fn stream(&self, video_id: &str) -> Result<Stream> {
        let (js, player_id, sts) = self.player().await?;
        let mut context = json!({ "client": {
            "clientName": "TVHTML5", "clientVersion": TV_VERSION, "userAgent": TV_UA,
            "deviceMake": "Samsung", "deviceModel": "UKS9800", "osName": "Tizen", "osVersion": "2.4.0",
            "hl": "en", "gl": "US", "visitorData": self.visitor,
        }});
        if let Some(id) = &self.datasync {
            context["user"] = json!({ "onBehalfOfUser": id });
        }
        let body = json!({
            "context": context, "videoId": video_id, "contentCheckOk": true, "racyCheckOk": true,
            "playbackContext": { "contentPlaybackContext": { "signatureTimestamp": sts, "html5Preference": "HTML5_PREF_WANTS" } },
        });
        let resp = self.send(|c| c.post(format!("{YT}/youtubei/v1/player?prettyPrint=false"))
            .header("User-Agent", TV_UA)
            .header("Origin", YT)
            .header("X-Origin", YT)
            .header("X-Youtube-Client-Name", "7")
            .header("X-Youtube-Client-Version", TV_VERSION)
            .header("X-Goog-Visitor-Id", &self.visitor)
            .header("X-Goog-AuthUser", "0")
            .header("Cookie", self.cookie_header())
            .header("Authorization", self.auth(YT))
            .json(&body)).await?;
        self.absorb(&resp);
        let status = resp.status();
        let v: Value = serde_json::from_str(&resp.text().await?)?;
        if !status.is_success() {
            if status == reqwest::StatusCode::UNAUTHORIZED {
                bail!("{SIGNED_OUT}");
            }
            bail!("player: HTTP {status}");
        }
        let ps = &v["playabilityStatus"];
        if ps["status"] != "OK" {
            bail!("not playable: {} {}", ps["status"].as_str().unwrap_or("?"), ps["reason"].as_str().unwrap_or(""));
        }
        let formats = v["streamingData"]["adaptiveFormats"].as_array().cloned().unwrap_or_default();
        // Opus only: that's what the decoder handles. 774 = Premium 256k, 251 = 128k.
        let pick = [774u64, 251, 250, 249]
            .iter()
            .find_map(|i| formats.iter().find(|f| f["itag"] == *i))
            .ok_or_else(|| anyhow!("no Opus audio format offered"))?;
        let itag = pick["itag"].as_u64().unwrap_or(0);
        let content_length: u64 = pick["contentLength"].as_str().and_then(|s| s.parse().ok()).ok_or_else(|| anyhow!("no contentLength"))?;

        let (base, sig) = if let Some(u) = pick["url"].as_str() {
            (u.to_string(), None)
        } else {
            let c: HashMap<String, String> = url::form_urlencoded::parse(
                pick["signatureCipher"].as_str().ok_or_else(|| anyhow!("format has no url"))?.as_bytes(),
            ).into_owned().collect();
            let u = c.get("url").cloned().ok_or_else(|| anyhow!("cipher without url"))?;
            let s = c.get("s").cloned().ok_or_else(|| anyhow!("cipher without s"))?;
            (u, Some((c.get("sp").cloned().unwrap_or_else(|| "signature".into()), s)))
        };
        let mut url = url::Url::parse(&base)?;
        let mut pairs: Vec<(String, String)> = url.query_pairs().into_owned().collect();
        let n = pairs.iter().find(|p| p.0 == "n").map(|p| p.1.clone());

        let solved = self.solver.solve(player_id, js, sig.as_ref().map(|s| s.1.clone()), n.clone()).await?;
        if let (Some((sp, _)), Some(s)) = (&sig, solved.sig) {
            pairs.push((sp.clone(), s));
        }
        if let Some(n) = solved.n {
            for p in pairs.iter_mut().filter(|p| p.0 == "n") {
                p.1 = n.clone();
            }
        }
        url.query_pairs_mut().clear().extend_pairs(&pairs);
        let duration = pick["approxDurationMs"].as_str().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0) / 1000.0;
        Ok(Stream { url, content_length, itag, duration })
    }

    /// Download a whole stream into memory, in 2 MiB ranges like the official players.
    /// Download a stream into `into`, in order, so it can play while it downloads. The first
    /// piece is small (playback starts sooner); each piece is retried twice. Stops early when
    /// nobody but this download holds `into` any more (the song was skipped and dropped).
    pub async fn download_into(&self, s: &Stream, into: &std::sync::Arc<crate::audio::Stream>) -> Result<()> {
        let mut start = 0u64;
        let mut piece = 256 * 1024;
        while start < s.content_length {
            if std::sync::Arc::strong_count(into) == 1 {
                return Ok(());
            }
            let end = (start + piece - 1).min(s.content_length - 1);
            let mut u = s.url.clone();
            u.query_pairs_mut().append_pair("range", &format!("{start}-{end}"));
            let mut attempt = 0;
            let bytes = loop {
                let res = async {
                    let r = self.send(|c| c.get(u.clone()).header("User-Agent", TV_UA)).await?;
                    if !r.status().is_success() {
                        bail!("stream: HTTP {} at byte {start}", r.status());
                    }
                    let bytes = r.bytes().await?;
                    if bytes.is_empty() {
                        bail!("stream: empty piece at byte {start}");
                    }
                    Ok(bytes)
                }.await;
                match res {
                    Ok(b) => break b,
                    Err(e) if attempt < 2 => {
                        attempt += 1;
                        eprintln!("[download] {e}; retrying");
                        tokio::time::sleep(Duration::from_millis(500 * attempt)).await;
                    }
                    Err(e) => return Err(e),
                }
            };
            into.push(&bytes);
            start += bytes.len() as u64;
            piece = 2 * 1024 * 1024;
        }
        Ok(())
    }

    // ---------- account sync: play reporting + likes ----------

    /// Where YouTube wants this play reported (from the website's own player request).
    pub async fn tracking(&self, video_id: &str) -> Result<Tracking> {
        let v = self.web_post("player", json!({
            "videoId": video_id, "contentCheckOk": true, "racyCheckOk": true,
            "playbackContext": { "contentPlaybackContext": { "html5Preference": "HTML5_PREF_WANTS" } },
        })).await?;
        let t = &v["playbackTracking"];
        let url = |k: &str| -> Result<String> {
            let u = t[k]["baseUrl"].as_str().ok_or_else(|| anyhow!("no {k} in player response"))?;
            let parsed = url::Url::parse(u)?;
            let host_ok = matches!(parsed.host_str(), Some("music.youtube.com" | "s.youtube.com" | "www.youtube.com"));
            let path_ok = matches!(parsed.path(), "/api/stats/playback" | "/api/stats/watchtime");
            if parsed.scheme() != "https" || !host_ok || !path_ok {
                bail!("unexpected stats url");
            }
            Ok(u.to_string())
        };
        let scheduled = t["videostatsScheduledFlushWalltimeSeconds"].as_array()
            .map(|a| a.iter().filter_map(Value::as_f64).collect())
            .unwrap_or_else(|| vec![10.0, 20.0, 30.0]);
        Ok(Tracking {
            playback: url("videostatsPlaybackUrl")?,
            watchtime: url("videostatsWatchtimeUrl")?,
            scheduled,
            interval: t["videostatsDefaultFlushIntervalSeconds"].as_f64().unwrap_or(40.0),
        })
    }

    /// Send one stats ping (playback start or watch time), as the website does.
    pub async fn stats(&self, base: &str, cpn: &str, params: &[(&str, String)]) -> Result<()> {
        let mut u = url::Url::parse(base)?;
        let keep: Vec<(String, String)> = u.query_pairs().into_owned().filter(|(k, _)| k != "cpn").collect();
        {
            let mut q = u.query_pairs_mut();
            q.clear().extend_pairs(&keep);
            q.append_pair("ver", "2").append_pair("c", "WEB_REMIX").append_pair("cpn", cpn);
            for (k, v) in params {
                q.append_pair(k, v);
            }
        }
        let r = self.send(|c| c.get(u.clone())
            .header("User-Agent", UA_WEB)
            .header("Origin", MUSIC)
            .header("X-Origin", MUSIC)
            .header("Referer", format!("{MUSIC}/"))
            .header("X-Goog-Api-Format-Version", "1")
            .header("X-Youtube-Client-Name", "67")
            .header("X-Youtube-Client-Version", &self.web_version)
            .header("X-Goog-Visitor-Id", &self.visitor)
            .header("X-Goog-AuthUser", "0")
            .header("Cookie", self.cookie_header())
            .header("Authorization", self.auth(MUSIC))
            ).await?;
        self.absorb(&r);
        if r.status() == reqwest::StatusCode::UNAUTHORIZED {
            bail!("{SIGNED_OUT}");
        }
        if !r.status().is_success() {
            bail!("stats ping: HTTP {}", r.status());
        }
        Ok(())
    }

    pub async fn like_status(&self, video_id: &str) -> Result<Like> {
        let v = self.web_post("next", json!({ "videoId": video_id, "isAudioOnly": true })).await?;
        let actions = v.pointer("/playerOverlays/playerOverlayRenderer/actions").and_then(Value::as_array).cloned().unwrap_or_default();
        let status = actions.iter().find_map(|a| a["likeButtonRenderer"]["likeStatus"].as_str()).unwrap_or("INDIFFERENT");
        Ok(match status {
            "LIKE" => Like::Like,
            "DISLIKE" => Like::Dislike,
            _ => Like::Indifferent,
        })
    }

    pub async fn set_like(&self, video_id: &str, like: Like) -> Result<()> {
        let endpoint = match like {
            Like::Like => "like/like",
            Like::Dislike => "like/dislike",
            Like::Indifferent => "like/removelike",
        };
        self.web_post(endpoint, json!({ "target": { "videoId": video_id } })).await?;
        Ok(())
    }

    pub async fn image(&self, url: &str) -> Result<Vec<u8>> {
        Ok(self.send(|c| c.get(url).header("User-Agent", UA_WEB)).await?.error_for_status()?.bytes().await?.to_vec())
    }
}

#[cfg(test)]
mod parse_tests {
    use super::*;

    fn load(name: &str) -> Option<Value> {
        let dir = std::env::var("YTM_FIXTURES").ok()?;
        serde_json::from_str(&fs::read_to_string(Path::new(&dir).join(name)).ok()?).ok()
    }

    // YTM_FIXTURES=<dir with dumped responses> cargo test -p ytm-native -- --ignored parse --nocapture
    #[test]
    #[ignore]
    fn parse_dumps() {
        let home = load("FEmusic_home.json").expect("fixtures");
        let list = home.pointer("/contents/singleColumnBrowseResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer").unwrap();
        for c in list["contents"].as_array().unwrap() {
            let shelf = &c["musicCarouselShelfRenderer"];
            let items: Vec<Item> = shelf["contents"].as_array().unwrap().iter().filter_map(parse_item).collect();
            let i = &items[0];
            println!("shelf {:?}: {} items; first {:?} {:?} / {:?} thumb={}", runs_text(&shelf["header"]["musicCarouselShelfBasicHeaderRenderer"]["title"]),
                items.len(), i.kind, i.song.title, i.song.artist, i.song.thumb.is_some());
        }
        let home2 = load("home2.json").unwrap();
        for c in home2.pointer("/continuationContents/sectionListContinuation/contents").unwrap().as_array().unwrap() {
            let items: Vec<Item> = c["musicCarouselShelfRenderer"]["contents"].as_array().unwrap().iter().filter_map(parse_item).collect();
            println!("cont shelf: {} items; first {:?} / {:?} / {:?}", items.len(), items[0].song.title, items[0].song.artist, items[0].song.duration);
        }
        for f in ["album.json", "playlist.json", "VLLM.json"] {
            let v = load(f).unwrap();
            let c = &v["contents"]["twoColumnBrowseResultsRenderer"];
            let shelf = c.pointer("/secondaryContents/sectionListRenderer/contents/0").unwrap();
            let shelf = shelf.get("musicPlaylistShelfRenderer").or_else(|| shelf.get("musicShelfRenderer")).unwrap();
            let (t, cont) = parse_track_list(&shelf["contents"], "ALBUMARTIST", &Some("HEADERTHUMB".into()));
            let s = &t[0];
            println!("{f}: {} tracks, cont={}; first {:?} / {:?} / {:?} / {:?} thumb={:?}", t.len(), cont.is_some(), s.title, s.artist, s.album, s.duration, s.thumb.as_deref().map(|x| &x[..x.len().min(12)]));
        }
    }

    fn row(page: &str, id: &str, title: &str, sub: &str) -> Value {
        json!({ "musicResponsiveListItemRenderer": {
            "navigationEndpoint": { "browseEndpoint": { "browseId": id, "browseEndpointContextSupportedConfigs":
                { "browseEndpointContextMusicConfig": { "pageType": page } } } },
            "flexColumns": [
                { "musicResponsiveListItemFlexColumnRenderer": { "text": { "runs": [{ "text": title }] } } },
                { "musicResponsiveListItemFlexColumnRenderer": { "text": { "runs": [{ "text": sub }] } } },
            ],
        }})
    }

    #[test]
    fn library_rows_and_continuations() {
        let song = json!({ "musicResponsiveListItemRenderer": {
            "playlistItemData": { "videoId": "vid1" },
            "flexColumns": [
                { "musicResponsiveListItemFlexColumnRenderer": { "text": { "runs": [{ "text": "Some Song" }] } } },
                { "musicResponsiveListItemFlexColumnRenderer": { "text": { "runs": [{ "text": "Some Artist" }] } } },
            ],
        }});
        let shuffle_row = json!({ "musicResponsiveListItemRenderer": {
            "navigationEndpoint": { "watchPlaylistEndpoint": { "playlistId": "MLCT" } },
            "flexColumns": [{ "musicResponsiveListItemFlexColumnRenderer": { "text": { "runs": [{ "text": "Shuffle all" }] } } }],
        }});
        let first = json!({ "contents": { "singleColumnBrowseResultsRenderer": { "tabs": [{ "tabRenderer": { "content": {
            "sectionListRenderer": { "contents": [{ "musicShelfRenderer": {
                "contents": [shuffle_row, song, row("MUSIC_PAGE_TYPE_LIBRARY_ARTIST", "MPLAUCabc", "An Artist", "3 songs")],
                "continuations": [{ "nextContinuationData": { "continuation": "TOKEN1" } }],
            }}]}}}}]}}});
        let (items, next) = parse_library_page(&first);
        assert_eq!(items.len(), 2);
        assert_eq!((items[0].kind, items[0].song.id.as_str()), (Kind::Song, "vid1"));
        assert_eq!((items[1].kind, items[1].song.id.as_str(), items[1].song.artist.as_str()), (Kind::Artist, "UCabc", "3 songs"));
        assert_eq!(next.as_deref(), Some("TOKEN1"));

        let more = json!({ "onResponseReceivedActions": [{ "appendContinuationItemsAction": { "continuationItems": [
            row("MUSIC_PAGE_TYPE_ALBUM", "MPREb_x", "An Album", "Album"),
            { "continuationItemRenderer": { "continuationEndpoint": { "continuationCommand": { "token": "TOKEN2" } } } },
        ]}}]});
        let (items, next) = parse_library_page(&more);
        assert_eq!((items.len(), items[0].kind), (1, Kind::Album));
        assert_eq!(next.as_deref(), Some("TOKEN2"));

        let old = json!({ "continuationContents": { "gridContinuation": { "items": [{ "musicTwoRowItemRenderer": {
            "title": { "runs": [{ "text": "A Playlist" }] },
            "navigationEndpoint": { "browseEndpoint": { "browseId": "VLPLx", "browseEndpointContextSupportedConfigs":
                { "browseEndpointContextMusicConfig": { "pageType": "MUSIC_PAGE_TYPE_PLAYLIST" } } } },
        }}]}}});
        let (items, next) = parse_library_page(&old);
        assert_eq!((items.len(), items[0].kind, items[0].song.id.as_str()), (1, Kind::Playlist, "VLPLx"));
        assert!(next.is_none());
    }

    // YTM_FIXTURES=<dir with library dumps> cargo test -- --ignored parse_library --nocapture
    #[test]
    #[ignore]
    fn parse_library_dumps() {
        for f in ["FEmusic_liked_playlists", "FEmusic_liked_albums", "FEmusic_liked_videos", "FEmusic_library_corpus_artists", "FEmusic_library_corpus_track_artists"] {
            let v = load(&format!("{f}.json")).expect("fixtures");
            let (items, next) = parse_library_page(&v);
            let kinds: Vec<Kind> = items.iter().map(|i| i.kind).collect();
            println!("{f}: {} items, more: {}, kinds {:?}, thumbs {}", items.len(), next.is_some(), kinds.first(), items.iter().filter(|i| i.song.thumb.is_some()).count());
        }
    }
}
