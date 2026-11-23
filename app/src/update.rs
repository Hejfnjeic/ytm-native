//! Updates from the project's GitHub releases.
//!
//! Each release carries `update-manifest.json`: per platform, the full executable and binary
//! patches (bsdiff, `BSDIFF40` format) from earlier versions, all with SHA-256 sums. An update
//! downloads only the patch from the running version, applies it to the running executable and
//! checks the result's hash; if anything doesn't match, it downloads the full executable instead
//! (also hash-checked). The new executable takes the old one's place and runs from the next
//! start. Release builds are made by `scripts/release.sh`, which also writes the patches.
//!
//! Only official release builds replace themselves (`YTM_RELEASE_BUILD` set at compile time).
//! Builds from source and distro packages (`YTM_PACKAGED`, e.g. the Arch package) only say that
//! a new version exists: replacing them would overwrite the user's own build or files a
//! package manager owns.

use std::{
    collections::HashMap,
    io::Read,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// `owner/repo` on GitHub.
pub const REPO: &str = "Hejfnjeic/ytm-native";
pub const MANIFEST: &str = "update-manifest.json";

/// How this build gets updated.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// Official release build: downloads and installs updates itself.
    SelfUpdate,
    /// Installed by a package manager (pacman): tell the user, don't touch the files.
    Packaged(&'static str),
    /// Built from source: tell the user, don't overwrite their build.
    FromSource,
}

pub fn mode() -> Mode {
    if let Some(p) = option_env!("YTM_PACKAGED") {
        Mode::Packaged(p)
    } else if option_env!("YTM_RELEASE_BUILD").is_some() {
        Mode::SelfUpdate
    } else {
        Mode::FromSource
    }
}

/// Release asset platform key ("linux-x86_64", "windows-x86_64").
pub fn platform() -> String {
    let os = if cfg!(windows) { "windows" } else if cfg!(target_os = "macos") { "macos" } else { "linux" };
    format!("{os}-{}", std::env::consts::ARCH)
}

/// The executable's path, as it was at startup. Asked again after an update, Linux would
/// answer "<path> (deleted)": the running process still points at the replaced file.
pub fn exe_path() -> Option<PathBuf> {
    static EXE: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    EXE.get_or_init(|| std::env::current_exe().ok()).clone()
}

pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Asset {
    pub name: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Patch {
    /// Version the patch applies to.
    pub from: String,
    /// SHA-256 of that version's executable (the patch only fits that exact file).
    pub from_sha256: String,
    #[serde(flatten)]
    pub asset: Asset,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct PlatformFiles {
    /// The new executable; its sha256 is also what a patched executable must hash to.
    pub full: Asset,
    #[serde(default)]
    pub patches: Vec<Patch>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Manifest {
    pub version: String,
    pub platforms: HashMap<String, PlatformFiles>,
}

/// A newer release than the running one.
#[derive(Clone, Debug)]
pub struct Release {
    pub version: String,
    /// The release's web page (notes, manual downloads).
    pub page: String,
    /// The release notes (GitHub Markdown).
    pub notes: String,
    manifest: Manifest,
    /// Asset name -> download URL.
    urls: HashMap<String, String>,
}

impl Release {
    /// Download size for this platform: the patch if one fits, otherwise the full file.
    pub fn download_size(&self, exe_sha256: Option<&str>) -> Option<u64> {
        let files = self.manifest.platforms.get(&platform())?;
        Some(pick_patch(files, exe_sha256).map_or(files.full.size, |p| p.asset.size))
    }
}

fn pick_patch<'a>(files: &'a PlatformFiles, exe_sha256: Option<&str>) -> Option<&'a Patch> {
    files.patches.iter().find(|p| p.from == current_version() && exe_sha256.is_none_or(|h| h == p.from_sha256))
}

/// "1.2.3" -> (1, 2, 3); anything after a '-' is ignored.
fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let v = v.trim().trim_start_matches('v');
    let core = v.split('-').next()?;
    let mut it = core.split('.').map(|n| n.parse::<u64>().ok());
    Some((it.next()??, it.next().flatten().unwrap_or(0), it.next().flatten().unwrap_or(0)))
}

pub fn is_newer(candidate: &str, current: &str) -> bool {
    matches!((parse_version(candidate), parse_version(current)), (Some(a), Some(b)) if a > b)
}

pub struct Updater {
    http: reqwest::Client,
    /// GitHub API base; `YTM_UPDATE_API` overrides it (tests serve a fake release locally).
    api: String,
}

impl Updater {
    pub fn new() -> Result<Self> {
        // Its own client: nothing from the YouTube session goes to GitHub.
        let http = reqwest::Client::builder()
            .user_agent(concat!("ytm-native/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(600))
            .build()?;
        let api = std::env::var("YTM_UPDATE_API").unwrap_or_else(|_| "https://api.github.com".into());
        Ok(Self { http, api })
    }

    /// The latest release, if it's newer than this build.
    /// (Its notes are shown with `note_lines`.)
    pub async fn check(&self) -> Result<Option<Release>> {
        let url = format!("{}/repos/{REPO}/releases/latest", self.api);
        let resp = self.http.get(&url).header("Accept", "application/vnd.github+json").send().await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None); // no releases yet
        }
        let v: serde_json::Value = resp.error_for_status()?.json().await?;
        let tag = v["tag_name"].as_str().ok_or_else(|| anyhow!("release without a tag"))?;
        if !is_newer(tag, current_version()) {
            return Ok(None);
        }
        let urls: HashMap<String, String> = v["assets"].as_array().into_iter().flatten()
            .filter_map(|a| Some((a["name"].as_str()?.to_string(), a["browser_download_url"].as_str()?.to_string())))
            .collect();
        let page = v["html_url"].as_str().unwrap_or_default().to_string();
        let murl = urls.get(MANIFEST).ok_or_else(|| anyhow!("release {tag} has no {MANIFEST}"))?;
        let manifest: Manifest = self.http.get(murl).send().await?.error_for_status()?.json().await?;
        let notes = v["body"].as_str().unwrap_or_default().to_string();
        Ok(Some(Release { version: manifest.version.clone(), page, notes, manifest, urls }))
    }

    async fn download(&self, asset: &Asset, urls: &HashMap<String, String>) -> Result<Vec<u8>> {
        let url = urls.get(&asset.name).ok_or_else(|| anyhow!("release is missing {}", asset.name))?;
        let bytes = self.http.get(url).send().await?.error_for_status()?.bytes().await?.to_vec();
        if bytes.len() as u64 != asset.size || sha256_hex(&bytes) != asset.sha256 {
            bail!("{} didn't match its checksum", asset.name);
        }
        Ok(bytes)
    }

    /// Download and install `release` over the running executable. Returns a short summary
    /// ("patch, 3.1 MB"). The new version runs from the next start.
    pub async fn install(&self, release: &Release) -> Result<String> {
        if mode() != Mode::SelfUpdate {
            bail!("this build doesn't update itself");
        }
        let files = release.manifest.platforms.get(&platform())
            .ok_or_else(|| anyhow!("release {} has nothing for {}", release.version, platform()))?;
        let exe = exe_path().ok_or_else(|| anyhow!("can't tell where the app is installed"))?;
        let old = std::fs::read(&exe).with_context(|| format!("reading {}", exe.display()))?;
        let old_sha = sha256_hex(&old);

        let mut how = String::new();
        let mut new = None;
        if let Some(p) = pick_patch(files, Some(&old_sha)) {
            match self.download(&p.asset, &release.urls).await {
                Ok(patch) => {
                    let built = tokio::task::spawn_blocking(move || bspatch(&old, &patch)).await?;
                    match built {
                        Ok(b) if sha256_hex(&b) == files.full.sha256 => {
                            how = format!("patch, {:.1} MB", p.asset.size as f64 / 1e6);
                            new = Some(b);
                        }
                        Ok(_) => eprintln!("[update] patched file didn't match; downloading the full version"),
                        Err(e) => eprintln!("[update] patch failed ({e}); downloading the full version"),
                    }
                }
                Err(e) => eprintln!("[update] {e}; downloading the full version"),
            }
        }
        let new = match new {
            Some(n) => n,
            None => {
                how = format!("full download, {:.1} MB", files.full.size as f64 / 1e6);
                self.download(&files.full, &release.urls).await?
            }
        };
        replace_exe(&exe, &new)?;
        Ok(how)
    }
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// Put `new` in place of the running executable. Linux: write next to it, then rename over it
/// (the running process keeps the old file). Windows can't replace a running .exe, but can
/// rename it: the old one moves to `<name>.old` (removed at the next start).
fn replace_exe(exe: &Path, new: &[u8]) -> Result<()> {
    let tmp = exe.with_extension("update-new");
    std::fs::write(&tmp, new).with_context(|| format!("writing {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    }
    #[cfg(windows)]
    {
        let old = old_path(exe);
        let _ = std::fs::remove_file(&old);
        std::fs::rename(exe, &old).context("moving the running version aside")?;
        if let Err(e) = std::fs::rename(&tmp, exe) {
            let _ = std::fs::rename(&old, exe);
            return Err(e).context("putting the new version in place");
        }
    }
    #[cfg(not(windows))]
    std::fs::rename(&tmp, exe).with_context(|| format!("replacing {}", exe.display()))?;
    Ok(())
}

fn old_path(exe: &Path) -> PathBuf {
    exe.with_extension("old")
}

/// Remove what an earlier update left behind (Windows keeps the replaced .exe until then).
pub fn clean_up() {
    if let Some(exe) = exe_path() {
        let _ = std::fs::remove_file(old_path(&exe));
        let _ = std::fs::remove_file(exe.with_extension("update-new"));
    }
}

/// Start the (updated) executable again; called after the app has shut down.
pub fn relaunch() {
    let Some(exe) = exe_path() else { return };
    match std::process::Command::new(&exe).spawn() {
        Ok(_) => eprintln!("[update] started the new version"),
        Err(e) => eprintln!("[update] couldn't start {}: {e}", exe.display()),
    }
}

/// Apply a bsdiff patch (`BSDIFF40`: header, then bzip2-compressed control, diff and extra
/// streams). Written here so the app needs no native code for it (bzip2 is pure Rust).
pub fn bspatch(old: &[u8], patch: &[u8]) -> Result<Vec<u8>> {
    fn int(b: &[u8]) -> i64 {
        let x = u64::from_le_bytes(b[..8].try_into().unwrap());
        let mag = (x & !(1 << 63)) as i64;
        if x >> 63 == 1 { -mag } else { mag }
    }
    if patch.len() < 32 || &patch[..8] != b"BSDIFF40" {
        bail!("not a bsdiff patch");
    }
    let (clen, dlen, new_len) = (int(&patch[8..16]), int(&patch[16..24]), int(&patch[24..32]));
    if clen < 0 || dlen < 0 || !(0..=1 << 31).contains(&new_len) || 32 + clen as u64 + dlen as u64 > patch.len() as u64 {
        bail!("corrupt patch header");
    }
    let body = &patch[32..];
    let (ctrl, rest) = body.split_at(clen as usize);
    let (diff, extra) = rest.split_at(dlen as usize);
    let mut ctrl = bzip2::read::BzDecoder::new(ctrl);
    let mut diff = bzip2::read::BzDecoder::new(diff);
    let mut extra = bzip2::read::BzDecoder::new(extra);

    let new_len = new_len as usize;
    let mut new = Vec::with_capacity(new_len);
    let mut old_pos: i64 = 0;
    let mut buf = [0u8; 24];
    while new.len() < new_len {
        ctrl.read_exact(&mut buf).context("patch ended early")?;
        let (add, copy, seek) = (int(&buf[0..8]), int(&buf[8..16]), int(&buf[16..24]));
        if add < 0 || copy < 0 || new.len() as i64 + add + copy > new_len as i64 {
            bail!("corrupt patch");
        }
        // Diff bytes are added to the old file's bytes (out-of-range old bytes count as 0).
        let start = new.len();
        new.resize(start + add as usize, 0);
        diff.read_exact(&mut new[start..]).context("patch diff data ended early")?;
        for (i, b) in new[start..].iter_mut().enumerate() {
            let at = old_pos + i as i64;
            if at >= 0 && (at as usize) < old.len() {
                *b = b.wrapping_add(old[at as usize]);
            }
        }
        old_pos += add;
        let start = new.len();
        new.resize(start + copy as usize, 0);
        extra.read_exact(&mut new[start..]).context("patch extra data ended early")?;
        old_pos += seek;
    }
    Ok(new)
}

/// Kind of a line in the release notes, as shown in the "What's new" window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoteKind {
    Heading,
    Bullet,
    Text,
}

/// Release notes (GitHub Markdown) as plain lines for the "What's new" window: headings,
/// bullets (wrapped lines joined) and text, without the "Install" section (it's about the
/// downloads) or Markdown marks. Links keep their text.
pub fn note_lines(md: &str) -> Vec<(NoteKind, String)> {
    let link = regex::Regex::new(r"\[([^\]]*)\]\([^)]*\)").unwrap();
    let clean = |t: &str| link.replace_all(t, "$1").replace('*', "").replace('`', "").replace("__", "").trim().to_string();
    let mut out: Vec<(NoteKind, String)> = Vec::new();
    let mut joinable = false;
    for raw in md.lines() {
        let line = raw.trim_end();
        let t = line.trim_start();
        if let Some(h) = t.strip_prefix('#') {
            let h = clean(h.trim_start_matches('#'));
            if h.eq_ignore_ascii_case("install") || h.eq_ignore_ascii_case("installation") {
                break;
            }
            out.push((NoteKind::Heading, h));
            joinable = false;
        } else if let Some(b) = t.strip_prefix("- ").or_else(|| t.strip_prefix("* ")) {
            out.push((NoteKind::Bullet, clean(b)));
            joinable = true;
        } else if t.is_empty() {
            joinable = false;
        } else if joinable || (line.starts_with(' ') && matches!(out.last(), Some((NoteKind::Bullet, _)))) {
            // A wrapped line of the previous bullet/paragraph.
            if let Some((_, prev)) = out.last_mut() {
                prev.push(' ');
                prev.push_str(&clean(t));
            }
        } else {
            out.push((NoteKind::Text, clean(t)));
            joinable = true;
        }
    }
    out.retain(|(_, t)| !t.is_empty());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_notes() {
        let md = "## What's new\n\n- **Library page**: *saved* things,\n  in one place.\n- See [the page](https://example.com) and `code`.\n\nSome text.\n\n## Install\n- download it\n";
        let lines = note_lines(md);
        assert_eq!(lines, vec![
            (NoteKind::Heading, "What's new".to_string()),
            (NoteKind::Bullet, "Library page: saved things, in one place.".to_string()),
            (NoteKind::Bullet, "See the page and code.".to_string()),
            (NoteKind::Text, "Some text.".to_string()),
        ]);
    }

    fn sample(seed: u64, len: usize) -> Vec<u8> {
        let mut x = seed;
        (0..len).map(|_| { x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407); (x >> 33) as u8 }).collect()
    }

    #[test]
    fn applies_qbsdiff_patches() {
        // An "old build" and a "new build" that shares most of it, shifted and edited.
        let old = sample(1, 300_000);
        let mut new = old[1000..200_000].to_vec();
        new.extend(sample(2, 5_000));
        new.extend_from_slice(&old[150_000..290_000]);
        for i in (0..new.len()).step_by(997) {
            new[i] = new[i].wrapping_add(3);
        }
        let mut patch = Vec::new();
        qbsdiff::Bsdiff::new(&old, &new).compare(std::io::Cursor::new(&mut patch)).unwrap();
        assert!(patch.len() < new.len() / 4, "patch is {} bytes", patch.len());
        assert_eq!(bspatch(&old, &patch).unwrap(), new);
        // A wrong base file gives a different result (caught by the hash check), not a panic.
        let other = sample(9, 300_000);
        assert_ne!(bspatch(&other, &patch).ok(), Some(new));
        assert!(bspatch(&old, b"BSDIFF40 too short").is_err());
    }

    #[test]
    fn versions() {
        assert!(is_newer("v0.2.0", "0.1.9"));
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(!is_newer("0.2.0", "0.2.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        assert!(!is_newer("garbage", "0.1.0"));
    }
}
