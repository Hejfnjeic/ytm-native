//! Release helpers for ytm-native: `cargo xtask <command>`.
//!
//!   cargo xtask release-files <dist-dir> <version>
//!       From the new executables in <dist-dir> and earlier releases' executables in
//!       <dist-dir>/previous/, write the update patches and update-manifest.json.
//!   cargo xtask diff <old> <new> <patch>
//!       One bsdiff patch (BSDIFF40), as the app's updater applies it.
//!
//! Executables are named `ytm-native-<version>-<platform>[.exe]`, platform being
//! `linux-x86_64` or `windows-x86_64` (see app/src/update.rs).

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const PLATFORMS: [(&str, &str); 2] = [("linux-x86_64", ""), ("windows-x86_64", ".exe")];

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["diff", old, new, out] => {
            let patch = diff(&fs::read(old)?, &fs::read(new)?)?;
            fs::write(out, &patch)?;
            println!("{out}: {} bytes", patch.len());
        }
        ["release-files", dist, version] => release_files(Path::new(dist), version)?,
        _ => {
            eprintln!("usage:\n  cargo xtask release-files <dist-dir> <version>\n  cargo xtask diff <old> <new> <patch>");
            std::process::exit(2);
        }
    }
    Ok(())
}

fn diff(old: &[u8], new: &[u8]) -> Result<Vec<u8>> {
    let mut patch = Vec::new();
    qbsdiff::Bsdiff::new(old, new).compare(std::io::Cursor::new(&mut patch))?;
    Ok(patch)
}

fn sha256(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

fn asset(path: &Path) -> Result<Value> {
    let data = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(json!({
        "name": path.file_name().unwrap().to_string_lossy(),
        "size": data.len(),
        "sha256": sha256(&data),
    }))
}

/// "ytm-native-0.2.0-linux-x86_64" -> "0.2.0" (for the given platform suffix).
fn version_of(name: &str, platform: &str, ext: &str) -> Option<String> {
    let v = name.strip_prefix("ytm-native-")?.strip_suffix(ext)?.strip_suffix(platform)?.strip_suffix('-')?;
    Some(v.to_string())
}

fn release_files(dist: &Path, version: &str) -> Result<()> {
    let previous = dist.join("previous");
    let mut platforms = serde_json::Map::new();
    for (platform, ext) in PLATFORMS {
        let new_path = dist.join(format!("ytm-native-{version}-{platform}{ext}"));
        if !new_path.exists() {
            bail!("missing {}", new_path.display());
        }
        let new = fs::read(&new_path)?;
        let mut patches = Vec::new();
        let mut olds: Vec<PathBuf> = fs::read_dir(&previous).map(|d| d.flatten().map(|e| e.path()).collect()).unwrap_or_default();
        olds.sort();
        for old_path in olds {
            let name = old_path.file_name().unwrap().to_string_lossy().to_string();
            let Some(from) = version_of(&name, platform, ext) else { continue };
            if from == version {
                continue;
            }
            let old = fs::read(&old_path)?;
            let patch = diff(&old, &new)?;
            let patch_path = dist.join(format!("ytm-native-{from}-to-{version}-{platform}.bsdiff"));
            fs::write(&patch_path, &patch)?;
            println!("{}: {:.1} MB (full: {:.1} MB)", patch_path.display(), patch.len() as f64 / 1e6, new.len() as f64 / 1e6);
            let mut a = asset(&patch_path)?;
            a["from"] = json!(from);
            a["from_sha256"] = json!(sha256(&old));
            patches.push(a);
        }
        platforms.insert(platform.into(), json!({ "full": asset(&new_path)?, "patches": patches }));
    }
    let manifest = json!({ "version": version, "platforms": platforms });
    let out = dist.join("update-manifest.json");
    fs::write(&out, serde_json::to_string_pretty(&manifest)? + "\n")?;
    println!("{}", out.display());
    Ok(())
}
