//! Stream probe: gets a playable audio URL for one song the way a Samsung smart TV's
//! YouTube app would, logged in with your cookies, then downloads it to check it works.
//!
//! Usage: stream <cookies.txt> <video id> [itag]
//!
//! The URL's `sig` and `n` parameters are solved by running yt-dlp's EJS solver against
//! YouTube's player JavaScript inside an embedded QuickJS engine (no Node needed).
//! Cookie values are never printed.

use std::{collections::HashMap, fs, path::Path, time::{Duration, Instant}};

use anyhow::{Context, Result, anyhow, bail};
use regex::Regex;
use serde_json::{Value, json};
use sha1::{Digest, Sha1};

const YT: &str = "https://www.youtube.com";
const UA_WEB: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:140.0) Gecko/20100101 Firefox/140.0";

// Same identity yt-dlp currently uses for logged-in Premium playback ("tv_samsung").
const TV_NAME: &str = "TVHTML5";
const TV_ID: &str = "7";
const TV_VERSION: &str = "5.20260707";
const TV_UA: &str = "Mozilla/5.0 (SMART-TV; Linux; Tizen 2.4.0) AppleWebKit/538.1 (KHTML, like Gecko) Version/2.4.0 TV Safari/538.1";

const EJS_LIB: &str = include_str!("../../../vendor/ejs/lib.min.js");
const EJS_CORE: &str = include_str!("../../../vendor/ejs/core.min.js");

fn load_cookies(path: &Path) -> Result<HashMap<String, String>> {
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
        bail!("no SAPISID cookie: is this a logged-in youtube.com export?");
    }
    Ok(cookies)
}

fn sapisid_auth(cookies: &HashMap<String, String>, origin: &str) -> String {
    let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let hash = |s: &str| hex::encode(Sha1::digest(format!("{ts} {s} {origin}")));
    [("SAPISIDHASH", "SAPISID"), ("SAPISID1PHASH", "__Secure-1PAPISID"), ("SAPISID3PHASH", "__Secure-3PAPISID")]
        .iter()
        .filter_map(|(scheme, name)| cookies.get(*name).map(|v| format!("{scheme} {ts}_{}", hash(v))))
        .collect::<Vec<_>>()
        .join(" ")
}

fn capture(re: &str, text: &str) -> Option<String> {
    Regex::new(re).ok()?.captures(text).map(|c| c[1].to_string())
}

/// Run the EJS solver over the player JS for a batch of challenges.
/// Returns the solver's per-request outputs (each maps challenge -> answer).
fn solve(player_js: &str, cache: &Path, requests: Value) -> Result<Vec<Value>> {
    let input = match fs::read_to_string(cache) {
        Ok(pre) => json!({ "type": "preprocessed", "preprocessed_player": pre, "requests": requests }),
        Err(_) => json!({ "type": "player", "player": player_js, "requests": requests, "output_preprocessed": true }),
    };
    let script = format!(
        "{EJS_LIB}\nObject.assign(globalThis, lib);\n{EJS_CORE}\nJSON.stringify(jsc({input}));"
    );
    let rt = rquickjs::Runtime::new()?;
    rt.set_memory_limit(512 * 1024 * 1024);
    rt.set_max_stack_size(8 * 1024 * 1024);
    let ctx = rquickjs::Context::full(&rt)?;
    let out: String = ctx.with(|ctx| {
        ctx.eval::<String, _>(script).map_err(|e| {
            let detail = ctx.catch().as_exception().map(|x| format!("{:?} {:?}", x.message(), x.stack()));
            anyhow!("QuickJS error: {e} {}", detail.unwrap_or_default())
        })
    })?;
    let v: Value = serde_json::from_str(&out)?;
    if v["type"] == "error" {
        bail!("solver error: {}", v["error"]);
    }
    if let Some(pre) = v["preprocessed_player"].as_str() {
        fs::write(cache, pre)?;
    }
    let responses = v["responses"].as_array().cloned().unwrap_or_default();
    for r in &responses {
        if r["type"] == "error" {
            bail!("solver challenge error: {}", r["error"]);
        }
    }
    Ok(responses)
}

fn query_map(q: &str) -> HashMap<String, String> {
    url::form_urlencoded::parse(q.as_bytes()).into_owned().collect()
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        bail!("usage: stream <cookies.txt> <video id> [itag]");
    }
    let video_id = &args[2];
    if !Regex::new(r"^[A-Za-z0-9_-]{11}$")?.is_match(video_id) {
        bail!("video id must be 11 characters (the v=... part of the URL)");
    }
    let wanted_itag: Option<u64> = args.get(3).map(|s| s.parse()).transpose()?;
    let cookies = load_cookies(Path::new(&args[1]))?;
    let cookie_header = cookies.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("; ");
    let out_dir = Path::new("probe-output");
    fs::create_dir_all(out_dir)?;

    // HTTP/2 to YouTube currently fails from reqwest here (see probe notes); HTTP/1.1 works.
    let http = reqwest::Client::builder().http1_only().gzip(true).brotli(true).timeout(Duration::from_secs(30)).build()?;

    // 1. Session info (visitor data, account id) from the logged-in YT Music page.
    let page = http.get("https://music.youtube.com").header("User-Agent", UA_WEB).header("Cookie", &cookie_header)
        .send().await?.error_for_status()?.text().await?;
    if !page.contains(r#""LOGGED_IN":true"#) {
        bail!("not logged in with these cookies (expired export?)");
    }
    let visitor = capture(r#""VISITOR_DATA":"([^"]+)""#, &page).ok_or_else(|| anyhow!("no VISITOR_DATA"))?;
    let datasync = capture(r#""DATASYNC_ID":"([^"]+)""#, &page).map(|d| d.split("||").next().unwrap_or("").to_string());

    // 2. Current player JS and its signature timestamp.
    let t = Instant::now();
    let iframe = http.get(format!("{YT}/iframe_api")).header("User-Agent", UA_WEB).send().await?.text().await?;
    let player_id = capture(r#"player\\?/([0-9a-fA-F]{8})\\?/"#, &iframe).ok_or_else(|| anyhow!("player id not found"))?;
    let player_url = format!("{YT}/s/player/{player_id}/player_ias.vflset/en_US/base.js");
    let player_js = http.get(&player_url).header("User-Agent", UA_WEB).send().await?.error_for_status()?.text().await?;
    let sts: u64 = capture(r"(?:signatureTimestamp|sts)\s*[:=]\s*(\d{5})", &player_js)
        .ok_or_else(|| anyhow!("signatureTimestamp not found in player"))?
        .parse()?;
    println!("player {player_id} ({} KiB), sts {sts}, fetched in {:?}", player_js.len() / 1024, t.elapsed());

    // 3. Player request as the TV app.
    let mut context = json!({ "client": {
        "clientName": TV_NAME, "clientVersion": TV_VERSION, "userAgent": TV_UA,
        "deviceMake": "Samsung", "deviceModel": "UKS9800", "osName": "Tizen", "osVersion": "2.4.0",
        "hl": "en", "gl": "US", "visitorData": visitor,
    }});
    if let Some(id) = &datasync {
        context["user"] = json!({ "onBehalfOfUser": id });
    }
    let body = json!({
        "context": context,
        "videoId": video_id,
        "playbackContext": { "contentPlaybackContext": { "signatureTimestamp": sts, "html5Preference": "HTML5_PREF_WANTS" } },
        "contentCheckOk": true,
        "racyCheckOk": true,
    });
    let resp = http.post(format!("{YT}/youtubei/v1/player?prettyPrint=false"))
        .header("User-Agent", TV_UA)
        .header("Content-Type", "application/json")
        .header("Origin", YT)
        .header("X-Origin", YT)
        .header("X-Youtube-Client-Name", TV_ID)
        .header("X-Youtube-Client-Version", TV_VERSION)
        .header("X-Goog-Visitor-Id", &visitor)
        .header("X-Goog-AuthUser", "0")
        .header("Cookie", &cookie_header)
        .header("Authorization", sapisid_auth(&cookies, YT))
        .json(&body).send().await?;
    let status = resp.status();
    let text = resp.text().await?;
    fs::write(out_dir.join(format!("player-{video_id}.json")), &text)?;
    if !status.is_success() {
        bail!("player HTTP {status}: {}", text.chars().take(300).collect::<String>());
    }
    let v: Value = serde_json::from_str(&text)?;
    let ps = &v["playabilityStatus"];
    println!("playability: {} {}", ps["status"], ps["reason"].as_str().unwrap_or(""));
    if ps["status"] != "OK" {
        bail!("not playable");
    }
    println!("title: {} — {}", v["videoDetails"]["author"].as_str().unwrap_or("?"), v["videoDetails"]["title"].as_str().unwrap_or("?"));

    let formats = v["streamingData"]["adaptiveFormats"].as_array().cloned().unwrap_or_default();
    let audio: Vec<&Value> = formats.iter().filter(|f| f["mimeType"].as_str().is_some_and(|m| m.starts_with("audio/"))).collect();
    println!("\naudio formats:");
    for f in &audio {
        println!("  itag {:>3}  {:<28} {:>4} kbps  {}", f["itag"], f["mimeType"].as_str().unwrap_or("").split(';').next().unwrap_or(""),
            f["bitrate"].as_u64().unwrap_or(0) / 1000,
            if f.get("url").is_some() { "direct url" } else if f.get("signatureCipher").is_some() { "signatureCipher" } else { "no url" });
    }
    let pick = match wanted_itag {
        Some(i) => audio.iter().find(|f| f["itag"] == i).copied(),
        None => [774u64, 251, 141, 140].iter().find_map(|i| audio.iter().find(|f| f["itag"] == *i).copied()),
    }
    .ok_or_else(|| anyhow!("wanted audio format not offered"))?;
    let itag = pick["itag"].as_u64().unwrap();

    // 4. Solve sig (if ciphered) and n.
    let (base_url, sig) = if let Some(url) = pick["url"].as_str() {
        (url.to_string(), None)
    } else {
        let c = query_map(pick["signatureCipher"].as_str().ok_or_else(|| anyhow!("no url or cipher"))?);
        let url = c.get("url").cloned().ok_or_else(|| anyhow!("cipher without url"))?;
        let s = c.get("s").cloned().ok_or_else(|| anyhow!("cipher without s"))?;
        (url, Some((c.get("sp").cloned().unwrap_or_else(|| "signature".into()), s)))
    };
    let mut parsed = url::Url::parse(&base_url)?;
    let params: HashMap<String, String> = parsed.query_pairs().into_owned().collect();
    let n = params.get("n").cloned();

    let mut reqs = Vec::new();
    if let Some((_, s)) = &sig {
        reqs.push(json!({ "type": "sig", "challenges": [s] }));
    }
    if let Some(n) = &n {
        reqs.push(json!({ "type": "n", "challenges": [n] }));
    }
    let t = Instant::now();
    let answers = if reqs.is_empty() { vec![] } else { solve(&player_js, &out_dir.join(format!("player-{player_id}.pre.js")), Value::Array(reqs))? };
    println!("\nsolved {} challenge(s) in {:?}", answers.len(), t.elapsed());

    let mut i = 0;
    let mut new_pairs: Vec<(String, String)> = params.into_iter().collect();
    if let Some((sp, s)) = &sig {
        let solved = answers[i]["data"][s].as_str().ok_or_else(|| anyhow!("no sig answer: {}", answers[i]))?.to_string();
        new_pairs.push((sp.clone(), solved));
        i += 1;
    }
    if let Some(n) = &n {
        let solved = answers[i]["data"][n].as_str().ok_or_else(|| anyhow!("no n answer: {}", answers[i]))?.to_string();
        for p in new_pairs.iter_mut().filter(|p| p.0 == "n") {
            p.1 = solved.clone();
        }
    }
    parsed.query_pairs_mut().clear().extend_pairs(&new_pairs);

    // 5. Download it in chunks, like the players do.
    let total = pick["contentLength"].as_str().and_then(|s| s.parse::<u64>().ok()).ok_or_else(|| anyhow!("no contentLength"))?;
    let ext = if pick["mimeType"].as_str().unwrap_or("").contains("webm") { "webm" } else { "m4a" };
    let path = out_dir.join(format!("{video_id}-{itag}.{ext}"));
    let mut data = Vec::with_capacity(total as usize);
    let chunk = 2 * 1024 * 1024;
    let t = Instant::now();
    while (data.len() as u64) < total {
        let start = data.len() as u64;
        let end = (start + chunk - 1).min(total - 1);
        let mut u = parsed.clone();
        u.query_pairs_mut().append_pair("range", &format!("{start}-{end}"));
        let r = http.get(u).header("User-Agent", TV_UA).send().await?;
        if !r.status().is_success() {
            bail!("stream HTTP {} at byte {start}", r.status());
        }
        let bytes = r.bytes().await?;
        if bytes.is_empty() {
            bail!("empty chunk at byte {start}");
        }
        data.extend_from_slice(&bytes);
    }
    fs::write(&path, &data)?;
    let secs = t.elapsed().as_secs_f64();
    println!("downloaded itag {itag}: {:.2} MiB in {secs:.1}s ({:.1} MiB/s) -> {}", total as f64 / 1048576.0, total as f64 / 1048576.0 / secs, path.display());
    Ok(())
}
