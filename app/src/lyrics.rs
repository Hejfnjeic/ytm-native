//! Synced lyrics: per-syllable/word when a source has it, per-line otherwise, plain last.
//!
//! Sources (title/artist/duration lookups; matched by duration):
//! - BetterLyrics (lyrics-api.boidu.dev): Apple-Music-style TTML, often word-timed.
//! - lrc.red: TTML (Apple Music lyrics, some AI-timed); spicy-lyrics credits it.
//! - LRCLIB (lrclib.net): community LRC, line-timed; best coverage.
//! - YouTube Music's own lyrics (by video id, so always the right song): line-timed through
//!   the Android app's identity, plain otherwise. Asked for without the login cookies.
//!
//! The data model follows spicy-lyrics' Syllable / Line / Static split. These requests use
//! their own HTTP client: YouTube cookies never go to lyric sites.

use std::{collections::HashMap, time::Duration};

use anyhow::{Result, anyhow, bail};
use regex::Regex;

use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sync {
    /// Timed per word/syllable.
    Syllable,
    /// Timed per line.
    Line,
    /// No timing.
    Static,
}

#[derive(Clone, Debug)]
pub struct Word {
    pub text: String,
    pub start: f64,
    pub end: f64,
}

#[derive(Clone, Debug)]
pub struct Line {
    pub start: f64,
    pub end: f64,
    /// Words with timing (Syllable), or a single word spanning the line.
    pub words: Vec<Word>,
    /// Sung by the second voice in a duet (shown on the right).
    pub opposite: bool,
    /// Background vocals (shown smaller).
    pub background: bool,
    /// English translation of the line, when the source has one (lrc.red).
    pub translation: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Lyrics {
    pub sync: Sync,
    pub lines: Vec<Line>,
    /// Who provided them, for the credit line.
    pub source: String,
}

impl Lyrics {
    /// The first sung line, to tell versions apart.
    pub fn preview(&self) -> String {
        self.lines.iter().filter(|l| !l.background)
            .map(|l| l.words.iter().map(|w| w.text.as_str()).collect::<String>().trim().to_string())
            .find(|t| !t.is_empty())
            .unwrap_or_default()
    }

    /// Same text, timing type and (roughly) timing: the same version, wherever it came from.
    pub fn signature(&self) -> String {
        let text: String = self.lines.iter().flat_map(|l| l.words.iter().map(|w| w.text.as_str())).collect();
        let times: Vec<i64> = self.lines.iter().take(8).map(|l| (l.start * 2.0).round() as i64).collect();
        format!("{:?}|{times:?}|{text}", self.sync)
    }

    pub fn has_translation(&self) -> bool {
        self.lines.iter().any(|l| l.translation.is_some())
    }

    pub fn kind(&self) -> &'static str {
        match self.sync {
            Sync::Syllable => "synced per word",
            Sync::Line => "synced per line",
            Sync::Static => "not synced",
        }
    }

    fn text_len(&self) -> usize {
        self.lines.iter().map(|l| l.words.len()).sum()
    }
}

/// A character with the time it's sung.
pub type TimedChar = (String, f32, f32);

/// Split a line into pieces that may not wrap internally: a Latin word (with its trailing
/// space) stays together; every CJK character is its own piece, with closing punctuation
/// kept on the character before it. Each character gets a share of its word's time.
pub fn line_units(line: &Line) -> Vec<Vec<TimedChar>> {
    let closing = |c: char| "、。，．！？!?,.)）」』】〕〉》ー〜~…・".contains(c);
    let mut units: Vec<Vec<TimedChar>> = Vec::new();
    let mut prev_space = true;
    for w in &line.words {
        let chars: Vec<char> = w.text.chars().collect();
        let sung = chars.iter().filter(|c| !c.is_whitespace()).count().max(1) as f64;
        let step = (w.end - w.start).max(0.0) / sung;
        let mut k = 0.0;
        for c in chars {
            let (start, end) = if c.is_whitespace() { (w.end, w.end) } else { (w.start + step * k, w.start + step * (k + 1.0)) };
            if !c.is_whitespace() {
                k += 1.0;
            }
            let tc = (c.to_string(), start as f32, end as f32);
            // Spaces and closing punctuation always stay with the piece before them.
            let attach = !units.is_empty() && (c.is_whitespace() || closing(c));
            let new_unit = units.is_empty() || prev_space || is_cjk(c);
            if new_unit && !attach {
                units.push(vec![tc]);
            } else {
                units.last_mut().unwrap().push(tc);
            }
            prev_space = c.is_whitespace();
            if is_cjk(c) {
                // The next character starts a new piece unless it's closing punctuation.
                prev_space = true;
            }
        }
    }
    units
}

// ---------- time & parsing ----------

/// TTML/LRC clock values: "24.365", "0:31.136", "1:02:03.5", "12.5s", "500ms".
fn parse_time(s: &str) -> Option<f64> {
    let s = s.trim();
    if let Some(ms) = s.strip_suffix("ms") {
        return ms.parse::<f64>().ok().map(|v| v / 1000.0);
    }
    let s = s.strip_suffix('s').unwrap_or(s);
    let mut total = 0.0;
    for part in s.split(':') {
        total = total * 60.0 + part.parse::<f64>().ok()?;
    }
    Some(total)
}

/// Apple-style TTML (BetterLyrics, lrc.red).
pub fn parse_ttml(ttml: &str, source: &str) -> Result<Lyrics> {
    if ttml.len() > 1024 * 1024 {
        bail!("lyrics file too large");
    }
    let doc = roxmltree::Document::parse(ttml)?;
    let root = doc.root_element();
    let timing = root
        .attributes()
        .find(|a| a.name() == "timing")
        .map(|a| a.value().to_ascii_lowercase())
        .unwrap_or_default();
    let ai_timed = root.attributes().any(|a| a.name() == "byModel" && a.value() == "true");
    // The first agent mentioned is the main voice; others go on the opposite side.
    let mut first_agent: Option<String> = None;
    // Translations (lrc.red/Apple style): <translation xml:lang="en-US"><text for="L1">…</text>.
    // English first; any language otherwise. Transliterations (romanization) are ignored.
    let mut translations: HashMap<String, String> = HashMap::new();
    let mut tr: Vec<_> = root.descendants().filter(|n| n.has_tag_name("translation")).collect();
    tr.sort_by_key(|t| !t.attributes().any(|a| a.name() == "lang" && a.value().starts_with("en")));
    if let Some(t) = tr.first() {
        for text in t.children().filter(|n| n.has_tag_name("text")) {
            let line: String = text.descendants().filter(|n| n.is_text()).filter_map(|n| n.text()).collect();
            if let Some(key) = text.attribute("for").filter(|_| !line.trim().is_empty()) {
                translations.insert(key.to_string(), line.trim().to_string());
            }
        }
    }

    let mut lines = Vec::new();
    for p in root.descendants().filter(|n| n.has_tag_name("p")) {
        let (Some(pb), Some(pe)) = (p.attribute("begin").and_then(parse_time), p.attribute("end").and_then(parse_time)) else { continue };
        let agent = p.attributes().find(|a| a.name() == "agent").map(|a| a.value().to_string());
        let key = p.attributes().find(|a| a.name() == "key").map(|a| a.value());
        if first_agent.is_none() {
            first_agent = agent.clone();
        }
        let opposite = agent.is_some() && agent != first_agent;

        let mut words: Vec<Word> = Vec::new();
        let mut bg_words: Vec<Word> = Vec::new();
        for child in p.children() {
            if child.is_text() {
                // Whitespace between spans marks a word boundary.
                if child.text().is_some_and(|t| t.chars().any(char::is_whitespace))
                    && let Some(w) = words.last_mut()
                    && !w.text.ends_with(' ')
                {
                    w.text.push(' ');
                }
                continue;
            }
            if !child.has_tag_name("span") {
                continue;
            }
            let is_bg = child.attributes().any(|a| a.name() == "role" && a.value() == "x-bg");
            if is_bg {
                // Background vocals: a span containing timed spans.
                for s in child.children() {
                    if s.is_text() {
                        if s.text().is_some_and(|t| t.chars().any(char::is_whitespace))
                            && let Some(w) = bg_words.last_mut()
                            && !w.text.ends_with(' ')
                        {
                            w.text.push(' ');
                        }
                    } else if let (Some(b), Some(e)) = (s.attribute("begin").and_then(parse_time), s.attribute("end").and_then(parse_time)) {
                        let text: String = s.descendants().filter(|n| n.is_text()).filter_map(|n| n.text()).collect();
                        bg_words.push(Word { text, start: b, end: e });
                    }
                }
                continue;
            }
            let text: String = child.descendants().filter(|n| n.is_text()).filter_map(|n| n.text()).collect();
            match (child.attribute("begin").and_then(parse_time), child.attribute("end").and_then(parse_time)) {
                (Some(b), Some(e)) => words.push(Word { text, start: b, end: e }),
                _ => words.push(Word { text, start: pb, end: pe }),
            }
        }
        if words.is_empty() {
            // Line-timed TTML: the text sits directly in <p>.
            let text: String = p.children().filter(|c| c.is_text()).filter_map(|c| c.text()).collect::<String>().trim().to_string();
            if text.is_empty() {
                continue;
            }
            words.push(Word { text, start: pb, end: pe });
        }
        // Parentheses around background words are part of the text; strip trailing space on the last word.
        for ws in [&mut words, &mut bg_words] {
            if let Some(w) = ws.last_mut() {
                w.text = w.text.trim_end().to_string();
            }
        }
        lines.push(Line { start: pb, end: pe, words, opposite, background: false, translation: key.and_then(|k| translations.get(k).cloned()) });
        if !bg_words.is_empty() {
            let (b, e) = (bg_words[0].start, bg_words.last().unwrap().end);
            lines.push(Line { start: b, end: e, words: bg_words, opposite, background: true, translation: None });
        }
    }
    if lines.is_empty() {
        bail!("no lyric lines in TTML");
    }
    let sync = if timing == "word" || timing == "syllable" || lines.iter().any(|l| l.words.len() > 1) { Sync::Syllable } else { Sync::Line };
    let source = if ai_timed { format!("{source} (AI-timed)") } else { source.to_string() };
    Ok(Lyrics { sync, lines, source })
}

/// LRC, including "enhanced" LRC with <mm:ss.xx> word tags.
pub fn parse_lrc(lrc: &str, source: &str) -> Lyrics {
    let tag = Regex::new(r"\[(\d+):(\d+(?:\.\d+)?)\]").unwrap();
    let word_tag = Regex::new(r"<(\d+):(\d+(?:\.\d+)?)>").unwrap();
    let mut raw: Vec<(f64, String)> = Vec::new();
    for line in lrc.lines() {
        let times: Vec<f64> = tag.captures_iter(line).filter_map(|c| Some(c[1].parse::<f64>().ok()? * 60.0 + c[2].parse::<f64>().ok()?)).collect();
        if times.is_empty() {
            continue;
        }
        let text = tag.replace_all(line, "").trim().to_string();
        for t in times {
            raw.push((t, text.clone()));
        }
    }
    raw.sort_by(|a, b| a.0.total_cmp(&b.0));
    // Credit lines some LRC files start with ("作词 : …", "Composer: …") aren't sung.
    let credit = Regex::new(r"^(?i)(作词|作詞|作曲|编曲|編曲|词|詞|曲|演唱|lyrics|lyricist|composer|arranger|written by)\s*[:：]").unwrap();
    raw.retain(|(_, t)| !credit.is_match(t));
    // …and any other "short label : value" lines before the first sung line.
    let label = Regex::new(r"^[^:：<]{1,14}\s[:：]\s*\S|^[^:：\s<]{1,8}[:：]\s*\S").unwrap();
    let lead = raw.iter().take_while(|(_, t)| t.is_empty() || label.is_match(t)).count();
    raw.drain(..lead);
    let mut lines = Vec::new();
    let mut any_words = false;
    for i in 0..raw.len() {
        let (start, text) = &raw[i];
        let end = raw.get(i + 1).map_or(start + 5.0, |n| n.0);
        if text.is_empty() {
            continue; // instrumental gap marker
        }
        let mut words = Vec::new();
        if word_tag.is_match(text) {
            // <t1>word <t2>word ...
            let marks: Vec<(f64, usize, usize)> = word_tag.captures_iter(text).filter_map(|c| {
                let m = c.get(0)?;
                Some((c[1].parse::<f64>().ok()? * 60.0 + c[2].parse::<f64>().ok()?, m.start(), m.end()))
            }).collect();
            for (j, (t, _, e)) in marks.iter().enumerate() {
                let next = marks.get(j + 1);
                let w = &text[*e..next.map_or(text.len(), |n| n.1)];
                if w.trim().is_empty() {
                    continue;
                }
                words.push(Word { text: w.to_string(), start: *t, end: next.map_or(end, |n| n.0) });
            }
            any_words |= !words.is_empty();
        }
        if words.is_empty() {
            words.push(Word { text: word_tag.replace_all(text, "").to_string(), start: *start, end });
        }
        if let Some(w) = words.last_mut() {
            w.text = w.text.trim_end().to_string();
        }
        lines.push(Line { start: *start, end, words, opposite: false, background: false, translation: None });
    }
    Lyrics { sync: if any_words { Sync::Syllable } else { Sync::Line }, lines, source: source.to_string() }
}

fn parse_plain(text: &str, source: &str) -> Lyrics {
    let lines = text
        .lines()
        .map(|l| Line { start: 0.0, end: 0.0, words: vec![Word { text: l.trim().to_string(), start: 0.0, end: 0.0 }], opposite: false, background: false, translation: None })
        .collect();
    Lyrics { sync: Sync::Static, lines, source: source.to_string() }
}

/// YouTube Music's lyrics page, as the Android app gets it: line-timed when YouTube has
/// timing, else plain text with a "Source: …" footer.
fn parse_youtube(v: &Value) -> Option<Lyrics> {
    let credit = |src: &str| match src.trim().strip_prefix("Source:").map(str::trim) {
        Some(who) if !who.is_empty() => format!("YouTube Music ({who})"),
        _ => "YouTube Music".to_string(),
    };
    if let Some(m) = v.pointer("/contents/elementRenderer/newElement/type/componentType/model/timedLyricsModel/lyricsData") {
        let cues: Vec<(f64, Option<f64>, String)> = m["timedLyricsData"].as_array().into_iter().flatten().filter_map(|c| {
            let ms = |k: &str| c["cueRange"][k].as_str().and_then(|x| x.parse::<f64>().ok()).map(|x| x / 1000.0);
            Some((ms("startTimeMilliseconds")?, ms("endTimeMilliseconds"), c["lyricLine"].as_str()?.trim().to_string()))
        }).collect();
        let mut lines = Vec::new();
        for (i, (start, end, text)) in cues.iter().enumerate() {
            if text.is_empty() || text == "♪" {
                continue; // instrumental gap marker
            }
            let end = end.filter(|e| e > start).or_else(|| cues.get(i + 1).map(|n| n.0)).unwrap_or(start + 5.0);
            let words = vec![Word { text: text.clone(), start: *start, end }];
            lines.push(Line { start: *start, end, words, opposite: false, background: false, translation: None });
        }
        if !lines.is_empty() {
            return Some(Lyrics { sync: Sync::Line, lines, source: credit(m["sourceMessage"].as_str().unwrap_or("")) });
        }
    }
    let shelf = v.pointer("/contents/sectionListRenderer/contents")?.as_array()?.iter()
        .find_map(|c| c.get("musicDescriptionShelfRenderer"))?;
    let text: String = shelf["description"]["runs"].as_array()?.iter().filter_map(|r| r["text"].as_str()).collect();
    let footer: String = shelf["footer"]["runs"].as_array().into_iter().flatten().filter_map(|r| r["text"].as_str()).collect();
    (!text.trim().is_empty()).then(|| parse_plain(text.trim(), &credit(&footer)))
}

// ---------- matching ----------

/// Title variants worth searching for. YT Music often titles Japanese songs
/// "原題 - Romanized" and adds "(feat. …)" / "(TV Size)" style tags.
fn title_variants(title: &str) -> Vec<String> {
    let tags = Regex::new(r"(?i)\s*[\(\[（【](?:feat\.?|ft\.?|with|prod\.?|official|music video|mv|lyric|audio|tv[ -]?size|short ver)[^\)\]）】]*[\)\]）】]").unwrap();
    let base = tags.replace_all(title, "").trim().to_string();
    let mut out = vec![base.clone()];
    if let Some((a, b)) = base.split_once(" - ") {
        out.push(a.trim().to_string());
        out.push(b.trim().to_string());
    }
    if title != base {
        out.push(title.to_string());
    }
    out.retain(|s| !s.is_empty());
    out.dedup();
    out
}

/// Main artist only ("A & B", "A, B", "A feat. B", "A x B").
pub fn main_artist(artist: &str) -> String {
    let re = Regex::new(r"(?i)\s*(?:,|&|、|×| x | feat\.? | ft\.? | with ).*$").unwrap();
    re.replace(artist, "").trim().to_string()
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32, 0x3040..=0x30ff | 0x3400..=0x4dbf | 0x4e00..=0x9fff | 0xac00..=0xd7af | 0xff66..=0xff9f)
}

/// Looks like a translation: the title is in Japanese/Chinese/Korean script but the
/// lyrics have hardly any of it (LRCLIB has English translations filed under JP titles).
fn looks_translated(title: &str, l: &Lyrics) -> bool {
    if !title.chars().any(is_cjk) {
        return false;
    }
    let (mut letters, mut cjk) = (0usize, 0usize);
    for c in l.lines.iter().flat_map(|ln| ln.words.iter()).flat_map(|w| w.text.chars()) {
        if c.is_alphabetic() {
            letters += 1;
            cjk += usize::from(is_cjk(c));
        }
    }
    letters > 20 && cjk * 10 < letters
}

fn norm(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

/// Same song? Normalized title of either contains the other.
fn title_matches(candidate: &str, variants: &[String]) -> bool {
    let c = norm(candidate);
    !c.is_empty() && variants.iter().map(|v| norm(v)).any(|v| !v.is_empty() && (c.contains(&v) || v.contains(&c)))
}

pub struct Finder {
    http: reqwest::Client,
}

impl Finder {
    pub fn new() -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent("ytm-native/0.1 (personal desktop music player)")
            .connect_timeout(Duration::from_secs(8))
            .timeout(Duration::from_secs(15))
            .build()?;
        Ok(Self { http })
    }

    async fn get_text(&self, url: url::Url) -> Result<Option<String>> {
        let r = self.http.get(url).header("Accept", "application/json").send().await?;
        match r.status().as_u16() {
            200 => Ok(Some(r.text().await?)),
            401 | 404 => Ok(None),
            s => Err(anyhow!("HTTP {s}")),
        }
    }

    async fn better_lyrics(&self, title: &str, artist: &str, duration: f64) -> Result<Option<Lyrics>> {
        let mut u = url::Url::parse("https://lyrics-api.boidu.dev/getLyrics")?;
        u.query_pairs_mut().append_pair("s", title).append_pair("a", artist);
        if duration > 0.0 {
            u.query_pairs_mut().append_pair("d", &format!("{}", duration.round() as i64));
        }
        let Some(body) = self.get_text(u).await? else { return Ok(None) };
        let v: Value = serde_json::from_str(&body)?;
        let Some(ttml) = v["ttml"].as_str().filter(|t| !t.is_empty()) else { return Ok(None) };
        // An empty/invalid document just means "no lyrics here".
        let Ok(l) = parse_ttml(ttml, "BetterLyrics") else { return Ok(None) };
        // Sanity: lyrics must fit inside the song.
        if duration > 0.0 && l.lines.last().is_some_and(|x| x.end > duration + 5.0) {
            return Ok(None);
        }
        Ok(Some(l))
    }

    async fn lrc_red(&self, variants: &[String], artist: &str, duration: f64, manual: bool) -> Result<Option<Lyrics>> {
        let mut u = url::Url::parse("https://lrc.red/api/v1")?;
        u.query_pairs_mut().append_pair("q", &format!("{} {artist}", variants[0]));
        let Some(body) = self.get_text(u).await? else { return Ok(None) };
        let v: Value = serde_json::from_str(&body)?;
        let results = v["results"].as_array().cloned().unwrap_or_default();
        // Same song and length within 3 s (avoids live/remix versions); prefer word timing.
        let pick = results
            .iter()
            .filter(|r| r["lyricsUrl"].as_str().is_some_and(|s| !s.is_empty()))
            .filter(|r| title_matches(r["track_name"].as_str().unwrap_or(""), variants))
            .filter(|r| manual || duration <= 0.0 || r["duration"].as_f64().is_some_and(|d| (d - duration).abs() <= 3.0))
            .max_by_key(|r| match r["timing_type"].as_str().unwrap_or("") {
                "word" | "syllable" => 2,
                "line" => 1,
                _ => 0,
            });
        let Some(pick) = pick else { return Ok(None) };
        if pick["timing_type"] == "none" {
            return Ok(None);
        }
        let path = pick["lyricsUrl"].as_str().unwrap();
        let full = if path.starts_with('/') { format!("https://lrc.red{path}") } else { path.to_string() };
        let u = url::Url::parse(&full)?;
        if u.scheme() != "https" || u.host_str() != Some("lrc.red") {
            return Ok(None);
        }
        let Some(ttml) = self.get_text(u).await? else { return Ok(None) };
        let mut l = parse_ttml(&ttml, "lrc.red")?;
        note_length(&mut l, pick["duration"].as_f64(), duration);
        Ok(Some(l))
    }

    /// The best match (automatic), or up to 5 candidates of any length (searching by hand).
    /// YouTube Music's own lyrics for a video: the lyrics tab id from `next`, then that page as
    /// the Android app (which gets timing). No cookies: lyrics don't need a login.
    async fn youtube(&self, video_id: &str) -> Result<Option<Lyrics>> {
        const WEB_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/142.0.0.0 Safari/537.36";
        const ANDROID_VERSION: &str = "7.27.52";
        let post = |endpoint: &str, ua: &str, client: Value, body: Value| {
            let mut body = body;
            body["context"] = serde_json::json!({ "client": client });
            self.http.post(format!("https://music.youtube.com/youtubei/v1/{endpoint}?prettyPrint=false"))
                .header("User-Agent", ua)
                .json(&body)
                .send()
        };
        let web = serde_json::json!({ "clientName": "WEB_REMIX", "clientVersion": "1.20251110.01.00", "hl": "en", "gl": "US" });
        let r = post("next", WEB_UA, web, serde_json::json!({ "videoId": video_id, "isAudioOnly": true })).await?;
        if !r.status().is_success() {
            bail!("next: HTTP {}", r.status());
        }
        let v: Value = r.json().await?;
        let tabs = v.pointer("/contents/singleColumnMusicWatchNextResultsRenderer/tabbedRenderer/watchNextTabbedResultsRenderer/tabs")
            .and_then(Value::as_array).cloned().unwrap_or_default();
        let Some(browse_id) = tabs.iter().find_map(|t| {
            t.pointer("/tabRenderer/endpoint/browseEndpoint/browseId").and_then(Value::as_str).filter(|id| id.starts_with("MPLY")).map(str::to_string)
        }) else {
            return Ok(None); // no lyrics tab: YouTube has none
        };
        let android = serde_json::json!({ "clientName": "ANDROID_MUSIC", "clientVersion": ANDROID_VERSION, "androidSdkVersion": 34,
                                          "osName": "Android", "osVersion": "14", "hl": "en", "gl": "US" });
        let ua = format!("com.google.android.apps.youtube.music/{ANDROID_VERSION} (Linux; U; Android 14) gzip");
        let r = post("browse", &ua, android, serde_json::json!({ "browseId": browse_id })).await?;
        if !r.status().is_success() {
            bail!("lyrics: HTTP {}", r.status());
        }
        Ok(parse_youtube(&r.json().await?))
    }

    async fn lrclib(&self, variants: &[String], artist: &str, album: &str, duration: f64, manual: bool) -> Result<Vec<Lyrics>> {
        // Exact lookup first (fast, precise), then search.
        for title in variants.iter().take(if manual { 0 } else { 2 }) {
            let mut u = url::Url::parse("https://lrclib.net/api/get")?;
            u.query_pairs_mut().append_pair("track_name", title).append_pair("artist_name", artist);
            if !album.is_empty() {
                u.query_pairs_mut().append_pair("album_name", album);
            }
            if duration > 0.0 {
                u.query_pairs_mut().append_pair("duration", &format!("{}", duration.round() as i64));
            }
            if let Some(body) = self.get_text(u).await? {
                let v: Value = serde_json::from_str(&body)?;
                if let Some(l) = lrclib_entry(&v).filter(|l| !looks_translated(&variants[0], l)) {
                    return Ok(vec![l]);
                }
            }
        }
        // With the artist first; then title only (the same artist is often written differently,
        // e.g. in Japanese script on YouTube but romanized on LRCLIB), where only matching length counts.
        let with_artist = variants.iter().take(3).map(|t| (t, true));
        let title_only = variants.iter().take(2).map(|t| (t, false)).filter(|_| duration > 0.0 || manual);
        let mut found = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for (title, use_artist) in with_artist.chain(title_only) {
            let mut u = url::Url::parse("https://lrclib.net/api/search")?;
            u.query_pairs_mut().append_pair("track_name", title);
            if use_artist {
                u.query_pairs_mut().append_pair("artist_name", artist);
            }
            let Some(body) = self.get_text(u).await? else { continue };
            let v: Value = serde_json::from_str(&body)?;
            let mut cands: Vec<&Value> = v.as_array().into_iter().flatten()
                .filter(|c| !c["instrumental"].as_bool().unwrap_or(false))
                .filter(|c| title_matches(c["trackName"].as_str().unwrap_or(""), variants))
                .filter(|c| manual || duration <= 0.0 || c["duration"].as_f64().is_some_and(|d| (d - duration).abs() <= 3.0))
                .collect();
            // Synced first, then same artist, then closest length.
            let artist_norm = norm(artist);
            cands.sort_by(|a, b| {
                let s = |c: &Value| c["syncedLyrics"].as_str().is_some_and(|s| !s.is_empty());
                let same = |c: &Value| !artist_norm.is_empty() && norm(c["artistName"].as_str().unwrap_or("")).contains(&artist_norm);
                s(b).cmp(&s(a)).then_with(|| same(b).cmp(&same(a))).then_with(|| {
                    let d = |c: &Value| (c["duration"].as_f64().unwrap_or(0.0) - duration).abs();
                    d(a).total_cmp(&d(b))
                })
            });
            if !manual {
                if let Some(l) = cands.iter().filter_map(|c| lrclib_entry(c)).find(|l| !looks_translated(&variants[0], l)) {
                    return Ok(vec![l]);
                }
                continue;
            }
            for c in cands {
                if found.len() >= 5 {
                    break;
                }
                if !seen.insert(c["id"].as_i64().unwrap_or(-1)) {
                    continue;
                }
                if let Some(mut l) = lrclib_entry(c) {
                    // Their own title, artist and length, to tell candidates apart.
                    let d = c["duration"].as_f64().unwrap_or(0.0).round() as i64;
                    l.source = format!("{} ({} – {}, {}:{:02})", l.source, c["trackName"].as_str().unwrap_or("?"),
                        c["artistName"].as_str().unwrap_or("?"), d / 60, d % 60);
                    note_length(&mut l, c["duration"].as_f64(), duration);
                    found.push(l);
                }
            }
        }
        Ok(found)
    }

    /// Best available lyrics for a song, or None.
    #[cfg(test)]
    pub async fn find(&self, title: &str, artist: &str, album: &str, duration: f64) -> Option<Lyrics> {
        self.find_all(title, artist, album, duration, None).await.into_iter().next()
    }

    /// Every version the sources have, best first. `manual`: a search by hand, which takes
    /// versions of any length (labelled) and several LRCLIB candidates.
    /// `video_id`: the playing song, for YouTube Music's own lyrics (None for a search by hand).
    pub async fn find_all(&self, title: &str, artist: &str, album: &str, duration: f64, video_id: Option<&str>) -> Vec<Lyrics> {
        let manual = video_id.is_none();
        let variants = title_variants(title);
        let artist = main_artist(artist);
        // Each source gets 6 s; one slow site mustn't hold up the others.
        let limit = Duration::from_secs(if manual { 10 } else { 6 });
        async fn deadline<T: Default>(limit: Duration, f: impl std::future::Future<Output = T>) -> T {
            tokio::time::timeout(limit, f).await.unwrap_or_default()
        }
        let (better, red, lib, yt) = tokio::join!(
            deadline(limit, async {
                for t in variants.iter().take(2) {
                    match self.better_lyrics(t, &artist, duration).await {
                        Ok(Some(l)) => return Some(l),
                        Ok(None) => {}
                        Err(e) => eprintln!("[lyrics] BetterLyrics: {e}"),
                    }
                }
                None
            }),
            deadline(limit, async { self.lrc_red(&variants, &artist, duration, manual).await.unwrap_or_else(|e| { eprintln!("[lyrics] lrc.red: {e}"); None }) }),
            deadline(limit, async { self.lrclib(&variants, &artist, album, duration, manual).await.unwrap_or_else(|e| { eprintln!("[lyrics] LRCLIB: {e}"); vec![] }) }),
            deadline(limit, async {
                let id = video_id?;
                self.youtube(id).await.unwrap_or_else(|e| { eprintln!("[lyrics] YouTube Music: {e}"); None })
            }),
        );
        // Word timing beats line timing beats plain; human timing beats AI timing.
        let mut all: Vec<Lyrics> = [better, red].into_iter().flatten().chain(lib).chain(yt).filter(|l| l.text_len() > 0).collect();
        for l in &mut all {
            if looks_translated(title, l) {
                l.source = format!("{} (looks like a translation)", l.source);
            }
        }
        // Order: right length, not a translation in place of the original, word > line > plain
        // timing, human > AI timing, then the one that comes with a translation.
        all.sort_by_key(|l| {
            let rank = match l.sync {
                Sync::Syllable => 0,
                Sync::Line => 1,
                Sync::Static => 2,
            };
            (
                l.source.contains(" longer") || l.source.contains(" shorter"),
                l.source.contains("looks like a translation"),
                rank,
                l.source.contains("AI-timed"),
                !l.has_translation(),
            )
        });
        let mut seen = std::collections::HashSet::new();
        all.retain(|l| seen.insert(l.signature()));
        all
    }
}

/// Label a version whose length is off by more than 3 s ("12 s longer").
fn note_length(l: &mut Lyrics, theirs: Option<f64>, song: f64) {
    let Some(d) = theirs.filter(|_| song > 0.0) else { return };
    let diff = (d - song).round();
    if diff.abs() > 3.0 {
        l.source = format!("{} ({} s {})", l.source, diff.abs(), if diff > 0.0 { "longer" } else { "shorter" });
    }
}

fn lrclib_entry(v: &Value) -> Option<Lyrics> {
    if let Some(s) = v["syncedLyrics"].as_str().filter(|s| !s.trim().is_empty()) {
        let l = parse_lrc(s, "LRCLIB");
        if !l.lines.is_empty() {
            return Some(l);
        }
    }
    v["plainLyrics"].as_str().filter(|s| !s.trim().is_empty()).map(|s| parse_plain(s, "LRCLIB"))
}

#[cfg(test)]
#[path = "lyrics_tests.rs"]
mod tests;
