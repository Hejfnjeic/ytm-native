//! Radio probe: asks YT Music for the same radio (seed song) under different client
//! identities, with and without your account, and compares what comes back.
//!
//! Usage: probe <cookies.txt> <seed video id> [pages]
//!
//! The cookies file is read locally and only sent to music.youtube.com. Its contents are
//! never printed. Raw responses are saved to ./probe-output/ for inspection.

use std::{collections::HashMap, fs, path::Path, time::Duration};

use anyhow::{Context, Result, anyhow, bail};
use regex::Regex;
use serde_json::{Value, json};
use sha1::{Digest, Sha1};

const ORIGIN: &str = "https://music.youtube.com";
const UA_WEB: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:140.0) Gecko/20100101 Firefox/140.0";

/// A client identity, as the official apps present themselves to InnerTube.
struct Client {
    label: &'static str,
    name: &'static str,
    id: &'static str, // X-Youtube-Client-Name
    version: String,
    user_agent: &'static str,
    extra: Value, // extra fields for context.client
    web: bool,    // sends Origin/Referer like the website
}

fn clients(web_version: &str) -> Vec<Client> {
    vec![
        Client {
            label: "web",
            name: "WEB_REMIX",
            id: "67",
            version: web_version.to_string(),
            user_agent: UA_WEB,
            extra: json!({ "platform": "DESKTOP" }),
            web: true,
        },
        Client {
            label: "android",
            name: "ANDROID_MUSIC",
            id: "21",
            version: "5.34.51".into(),
            user_agent: "com.google.android.apps.youtube.music/5.34.51 (Linux; U; Android 11) gzip",
            extra: json!({ "osName": "Android", "osVersion": "11", "androidSdkVersion": 30, "platform": "MOBILE" }),
            web: false,
        },
        Client {
            label: "ios",
            name: "IOS_MUSIC",
            id: "26",
            version: "7.27.0".into(),
            user_agent: "com.google.ios.youtubemusic/7.27.0 (iPhone16,2; U; CPU iOS 18_1_0 like Mac OS X;)",
            extra: json!({ "osName": "iPhone", "osVersion": "18.1.0.22B83", "deviceMake": "Apple",
                           "deviceModel": "iPhone16,2", "platform": "MOBILE" }),
            web: false,
        },
    ]
}

/// Netscape cookies.txt → name/value pairs for youtube.com.
fn load_cookies(path: &Path) -> Result<HashMap<String, String>> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut cookies = HashMap::new();
    for line in text.lines() {
        let line = line.strip_prefix("#HttpOnly_").unwrap_or(line);
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() >= 7 && fields[0].trim_start_matches('.').ends_with("youtube.com") {
            cookies.insert(fields[5].to_string(), fields[6].trim_end().to_string());
        }
    }
    if !cookies.contains_key("SAPISID") && !cookies.contains_key("__Secure-3PAPISID") {
        bail!("no SAPISID cookie found: is this a logged-in music.youtube.com export?");
    }
    Ok(cookies)
}

struct Session {
    cookie_header: String,
    sapisid: Option<String>,
    sapisid_1p: Option<String>,
    sapisid_3p: Option<String>,
    visitor_data: String,
    datasync_id: Option<String>,
    web_version: String,
}

impl Session {
    /// The website's Authorization header: SHA-1 over timestamp, cookie and origin.
    fn auth_header(&self) -> Option<String> {
        let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
        let hash = |secret: &str| hex::encode(Sha1::digest(format!("{ts} {secret} {ORIGIN}")));
        let mut parts = Vec::new();
        if let Some(s) = &self.sapisid {
            parts.push(format!("SAPISIDHASH {ts}_{}", hash(s)));
        }
        if let Some(s) = &self.sapisid_1p {
            parts.push(format!("SAPISID1PHASH {ts}_{}", hash(s)));
        }
        if let Some(s) = &self.sapisid_3p {
            parts.push(format!("SAPISID3PHASH {ts}_{}", hash(s)));
        }
        (!parts.is_empty()).then(|| parts.join(" "))
    }
}

/// Load the website once (logged in) to learn visitor data, account id and current web version.
async fn open_session(http: &reqwest::Client, cookies: &HashMap<String, String>) -> Result<Session> {
    let cookie_header = cookies.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("; ");
    let html = http
        .get(ORIGIN)
        .header("User-Agent", UA_WEB)
        .header("Cookie", &cookie_header)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;

    let field = |key: &str| {
        Regex::new(&format!(r#""{key}":"([^"]+)""#)).ok()?.captures(&html).map(|c| c[1].to_string())
    };
    let logged_in = html.contains(r#""LOGGED_IN":true"#);
    if !logged_in {
        bail!("music.youtube.com doesn't see you as logged in with these cookies (expired or wrong export?)");
    }
    Ok(Session {
        sapisid: cookies.get("SAPISID").cloned(),
        sapisid_1p: cookies.get("__Secure-1PAPISID").cloned(),
        sapisid_3p: cookies.get("__Secure-3PAPISID").cloned(),
        cookie_header,
        visitor_data: field("VISITOR_DATA").ok_or_else(|| anyhow!("no VISITOR_DATA in page"))?,
        // Brand accounts: "<datasync>||<page id>" – the first part is what onBehalfOfUser wants.
        datasync_id: field("DATASYNC_ID").map(|d| d.split("||").next().unwrap_or(&d).to_string()),
        web_version: field("INNERTUBE_CLIENT_VERSION").unwrap_or_else(|| "1.20260707.12.00".into()),
    })
}

struct Track {
    id: String,
    title: String,
    artist: String,
}

/// Pull tracks + continuation token out of a `next` response (first page or continuation).
fn parse_panel(v: &Value) -> (Vec<Track>, Option<String>) {
    let panel = v
        .pointer("/contents/singleColumnMusicWatchNextResultsRenderer/tabbedRenderer/watchNextTabbedResultsRenderer/tabs/0/tabRenderer/content/musicQueueRenderer/content/playlistPanelRenderer")
        .or_else(|| v.pointer("/continuationContents/playlistPanelContinuation"));
    let Some(panel) = panel else { return (vec![], None) };

    let tracks = panel["contents"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let r = item.get("playlistPanelVideoRenderer")
                .or_else(|| item.pointer("/playlistPanelVideoWrapperRenderer/primaryRenderer/playlistPanelVideoRenderer"))?;
            let runs_text = |p: &str| {
                r.pointer(p)?.as_array().map(|runs| runs.iter().filter_map(|x| x["text"].as_str()).collect::<String>())
            };
            Some(Track {
                id: r["videoId"].as_str()?.to_string(),
                title: runs_text("/title/runs").unwrap_or_default(),
                artist: runs_text("/longBylineText/runs")
                    .map(|s| s.split(" • ").next().unwrap_or("").to_string())
                    .unwrap_or_default(),
            })
        })
        .collect();
    let continuation = panel
        .pointer("/continuations/0/nextRadioContinuationData/continuation")
        .or_else(|| panel.pointer("/continuations/0/nextContinuationData/continuation"))
        .and_then(Value::as_str)
        .map(str::to_string);
    (tracks, continuation)
}

async fn fetch_radio(
    http: &reqwest::Client,
    session: &Session,
    client: &Client,
    logged_in: bool,
    seed: &str,
    pages: usize,
    out_dir: &Path,
) -> Result<(Vec<Track>, usize, bool)> {
    let mut client_ctx = json!({
        "clientName": client.name,
        "clientVersion": client.version,
        "hl": "en",
        "gl": "US",
        "userAgent": client.user_agent,
    });
    if logged_in || client.web {
        client_ctx["visitorData"] = json!(session.visitor_data);
    }
    for (k, v) in client.extra.as_object().unwrap() {
        client_ctx[k] = v.clone();
    }
    let mut context = json!({ "client": client_ctx });
    if logged_in {
        if let Some(id) = &session.datasync_id {
            context["user"] = json!({ "onBehalfOfUser": id });
        }
    }

    let mut tracks = Vec::new();
    let mut continuation: Option<String> = None;
    let mut fetched = 0;
    for page in 0..pages {
        fetched = page + 1;
        let body = match &continuation {
            None => json!({
                "context": context,
                "videoId": seed,
                "playlistId": format!("RDAMVM{seed}"),
                "isAudioOnly": true,
                "enablePersistentPlaylistPanel": true,
                "tunerSettingValue": "AUTOMIX_SETTING_NORMAL",
            }),
            Some(token) => json!({ "context": context, "continuation": token, "isAudioOnly": true,
                                    "enablePersistentPlaylistPanel": true }),
        };
        let mut req = http
            .post(format!("{ORIGIN}/youtubei/v1/next?prettyPrint=false"))
            .header("User-Agent", client.user_agent)
            .header("Content-Type", "application/json")
            .header("X-Youtube-Client-Name", client.id)
            .header("X-Youtube-Client-Version", &client.version)
            .json(&body);
        if client.web {
            req = req.header("Origin", ORIGIN).header("Referer", format!("{ORIGIN}/"));
        }
        if logged_in {
            req = req
                .header("Cookie", &session.cookie_header)
                .header("X-Goog-AuthUser", "0")
                .header("X-Goog-Visitor-Id", &session.visitor_data);
            if let Some(auth) = session.auth_header() {
                req = req.header("Authorization", auth);
            }
        }

        let resp = req.send().await?;
        let status = resp.status();
        let text = resp.text().await?;
        let mode = if logged_in { "account" } else { "anon" };
        fs::write(out_dir.join(format!("{}-{mode}-page{page}.json", client.label)), &text)?;
        if !status.is_success() {
            bail!("HTTP {status}: {}", text.chars().take(200).collect::<String>());
        }
        let v: Value = serde_json::from_str(&text)?;
        let (mut page_tracks, next) = parse_panel(&v);
        if page_tracks.is_empty() && page == 0 {
            bail!("no radio in response (saved to probe-output/)");
        }
        tracks.append(&mut page_tracks);
        continuation = next;
        if continuation.is_none() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(700)).await; // keep it app-like
    }
    Ok((tracks, fetched, continuation.is_none()))
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        bail!("usage: probe <cookies.txt> <seed video id> [pages]");
    }
    let seed = &args[2];
    if !Regex::new(r"^[A-Za-z0-9_-]{11}$")?.is_match(seed) {
        bail!("seed must be an 11-character video id (the v=... part of the URL)");
    }
    let pages: usize = args.get(3).map(|p| p.parse()).transpose()?.unwrap_or(2);

    let out_dir = Path::new("probe-output");
    fs::create_dir_all(out_dir)?;
    let http = reqwest::Client::builder().http1_only().gzip(true).brotli(true).timeout(Duration::from_secs(20)).build()?;
    let cookies = load_cookies(Path::new(&args[1]))?;
    let session = open_session(&http, &cookies).await?;
    println!(
        "Logged in. web client version {}, account id {}\n",
        session.web_version,
        if session.datasync_id.is_some() { "found" } else { "not found" }
    );

    let mut results: Vec<(String, Vec<Track>)> = Vec::new();
    for client in clients(&session.web_version) {
        for logged_in in [true, false] {
            let label = format!("{}/{}", client.label, if logged_in { "account" } else { "anon" });
            match fetch_radio(&http, &session, &client, logged_in, seed, pages, out_dir).await {
                Ok((tracks, fetched, ended)) => {
                    let uniq: std::collections::HashSet<_> = tracks.iter().map(|t| &t.id).collect();
                    println!("{label:<16} {:>4} tracks ({} unique) over {fetched} pages, {}", tracks.len(), uniq.len(),
                        if ended { "RADIO ENDED (no continuation)" } else { "still going at page limit" });
                    results.push((label, tracks));
                }
                Err(e) => println!("{label:<16} FAILED: {e}"),
            }
            tokio::time::sleep(Duration::from_millis(700)).await;
        }
    }

    // Overlap matrix: how many tracks each pair of results share.
    println!("\nShared tracks (row vs column):");
    print!("{:<16}", "");
    for (label, _) in &results {
        print!("{label:>16}");
    }
    println!();
    for (a_label, a) in &results {
        print!("{a_label:<16}");
        for (_, b) in &results {
            let shared = a.iter().filter(|t| b.iter().any(|u| u.id == t.id)).count();
            print!("{shared:>16}");
        }
        println!();
    }

    for (label, tracks) in &results {
        println!("\n== {label}");
        for (i, t) in tracks.iter().take(25).enumerate() {
            println!("{:>3}. {} — {}", i + 1, t.artist, t.title);
        }
    }
    Ok(())
}
