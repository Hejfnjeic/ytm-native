//! Secrets (the YouTube sign-in, a proxy's password) kept encrypted in the data folder.
//!
//! Each secret is a file, `<name>.enc`: `YTMN1` | 24-byte nonce | XChaCha20-Poly1305
//! ciphertext, under one random 256-bit key. Where the key lives:
//! - Linux: the desktop's secret store (Secret Service: KWallet on Plasma, GNOME Keyring
//!   elsewhere), never on disk next to the data. Without one running, a file only this
//!   user can read, in the data folder.
//! - Windows: a file in the data folder, itself encrypted with DPAPI (bound to the Windows
//!   user account; not the Credential Manager). Copied elsewhere, it can't be decrypted.
//! - Elsewhere: a file only this user can read, in the data folder.
//!
//! The key is the one ytm-native kept its login under before this design moved here
//! (`session.enc`), so that login carries over: see [`legacy_cookie`].
//! These calls block (D-Bus, disk): keep them off the UI thread where it matters.

use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow, bail};
use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead, Generate, Key, KeyInit},
};

const MAGIC: &[u8] = b"YTMN1";
const NONCE: usize = 24;
/// The keyring entry's service and user, as the Slint version of ytm-native named them.
#[cfg(target_os = "linux")]
const SERVICE: &str = "ytm-native";
#[cfg(target_os = "linux")]
const KEY_USER: &str = "session-encryption-key";

/// One key operation at a time: two first saves at once would each make a key.
static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn file(name: &str, dir: &Path) -> PathBuf {
    let safe: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect();
    dir.join(format!("{safe}.enc"))
}

/// Whether `dir` is the usual data folder: a `YTM_DATA_DIR` dev profile (or a test's
/// folder) gets a key of its own, so it never shares or touches the real sign-in's.
#[cfg(target_os = "linux")]
fn default_dir(dir: &Path) -> bool {
    crate::default_data_dir().as_deref() == Some(dir)
}

#[cfg(target_os = "linux")]
mod keystore {
    use std::path::Path;

    /// Tests leave the desktop's keyring alone: a locked one doesn't refuse, it waits on
    /// its unlock prompt, and the test run with it.
    fn use_keyring(dir: &Path) -> bool {
        !cfg!(test) && super::default_dir(dir)
    }

    fn entry() -> anyhow::Result<keyring::Entry> {
        keyring::Entry::new(super::SERVICE, super::KEY_USER).map_err(|e| anyhow::anyhow!("system keyring unavailable: {e}"))
    }

    pub fn get(dir: &Path) -> anyhow::Result<Option<Vec<u8>>> {
        if use_keyring(dir) {
            match entry().and_then(|e| e.get_secret().map_err(Into::into)) {
                Ok(b) => return Ok(Some(b)),
                Err(e) if e.downcast_ref::<keyring::Error>().is_some_and(|k| matches!(k, keyring::Error::NoEntry)) => {}
                Err(e) => log::info!("system keyring unavailable, looking for the key file: {e:#}"),
            }
        }
        super::plain::get(dir)
    }

    pub fn set(dir: &Path, key: &[u8]) -> anyhow::Result<()> {
        if use_keyring(dir) {
            match entry().and_then(|e| e.set_secret(key).map_err(Into::into)) {
                Ok(()) => return Ok(()),
                Err(e) => log::info!("no system keyring, keeping the key in a file: {e:#}"),
            }
        }
        super::plain::set(dir, key)
    }
}

#[cfg(windows)]
mod keystore {
    //! DPAPI (CryptProtectData) for the current Windows user; the protected key is a file.
    use std::path::{Path, PathBuf};
    use std::ptr;

    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData};

    fn path(dir: &Path) -> PathBuf {
        dir.join("session.key")
    }

    fn run(input: &[u8], protect: bool) -> anyhow::Result<Vec<u8>> {
        let inp = CRYPT_INTEGER_BLOB { cbData: input.len() as u32, pbData: input.as_ptr() as *mut u8 };
        let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: ptr::null_mut() };
        // SAFETY: a valid input blob; Windows allocates the output, freed below.
        let ok = unsafe {
            if protect {
                CryptProtectData(&inp, ptr::null(), ptr::null(), ptr::null(), ptr::null(), CRYPTPROTECT_UI_FORBIDDEN, &mut out)
            } else {
                CryptUnprotectData(&inp, ptr::null_mut(), ptr::null(), ptr::null(), ptr::null(), CRYPTPROTECT_UI_FORBIDDEN, &mut out)
            }
        };
        if ok == 0 || out.pbData.is_null() {
            anyhow::bail!("Windows couldn't {} the login key (different user or PC?)", if protect { "protect" } else { "unlock" });
        }
        // SAFETY: Windows returned `cbData` bytes at `pbData`; copied out, then freed once.
        let data = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec() };
        unsafe { LocalFree(out.pbData as _) };
        Ok(data)
    }

    pub fn get(dir: &Path) -> anyhow::Result<Option<Vec<u8>>> {
        match std::fs::read(path(dir)) {
            Ok(blob) => Ok(Some(run(&blob, false)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn set(dir: &Path, key: &[u8]) -> anyhow::Result<()> {
        let blob = run(key, true)?;
        std::fs::create_dir_all(dir)?;
        std::fs::write(path(dir), blob)?;
        Ok(())
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
mod keystore {
    pub use super::plain::{get, set};
}

/// The key in a file only this user can read, in the data folder: where there's no
/// keyring, and for tests and dev profiles.
#[cfg_attr(windows, allow(dead_code))]
mod plain {
    use std::path::{Path, PathBuf};

    fn path(dir: &Path) -> PathBuf {
        dir.join("vault.key")
    }

    pub fn get(dir: &Path) -> anyhow::Result<Option<Vec<u8>>> {
        match std::fs::read(path(dir)) {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn set(dir: &Path, key: &[u8]) -> anyhow::Result<()> {
        std::fs::create_dir_all(dir)?;
        super::write_private(&path(dir), key)
    }
}

/// Write a file only this user can read, replacing what was there in one step.
fn write_private(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    let tmp = path.with_extension("tmp");
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn existing_key(dir: &Path) -> anyhow::Result<Option<Key<XChaCha20Poly1305>>> {
    match keystore::get(dir)? {
        Some(bytes) => {
            let key: [u8; 32] = bytes.try_into().map_err(|_| anyhow!("the stored key has the wrong length"))?;
            Ok(Some(key.into()))
        }
        None => Ok(None),
    }
}

fn key_or_create(dir: &Path) -> anyhow::Result<Key<XChaCha20Poly1305>> {
    if let Some(k) = existing_key(dir)? {
        return Ok(k);
    }
    let key = Key::<XChaCha20Poly1305>::generate();
    keystore::set(dir, key.as_slice())?;
    Ok(key)
}

fn seal(dir: &Path, plain: &[u8]) -> anyhow::Result<Vec<u8>> {
    let _one = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let key = key_or_create(dir)?;
    let nonce = XNonce::generate();
    let sealed = XChaCha20Poly1305::new(&key).encrypt(&nonce, plain).map_err(|_| anyhow!("encryption failed"))?;
    Ok([MAGIC, nonce.as_slice(), &sealed].concat())
}

fn open(dir: &Path, data: &[u8]) -> anyhow::Result<Vec<u8>> {
    if data.len() < MAGIC.len() + NONCE || !data.starts_with(MAGIC) {
        bail!("the stored file is damaged");
    }
    let _one = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let key = existing_key(dir)?.ok_or_else(|| anyhow!("its key is missing from the system keyring"))?;
    let nonce = XNonce::try_from(&data[MAGIC.len()..MAGIC.len() + NONCE]).map_err(|_| anyhow!("bad nonce"))?;
    XChaCha20Poly1305::new(&key).decrypt(&nonce, &data[MAGIC.len() + NONCE..]).map_err(|_| anyhow!("it can't be decrypted (wrong key or damaged file)"))
}

fn read(path: &Path, dir: &Path) -> anyhow::Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(data) => open(dir, &data).map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).context("reading a stored secret"),
    }
}

/// The secret saved under `name`, if any.
pub fn get(name: &str, data_dir: &Path) -> Option<Vec<u8>> {
    read(&file(name, data_dir), data_dir).unwrap_or_else(|e| {
        log::warn!("couldn't read {name}: {e:#}");
        None
    })
}

pub fn set(name: &str, data_dir: &Path, secret: &[u8]) -> anyhow::Result<()> {
    std::fs::create_dir_all(data_dir)?;
    write_private(&file(name, data_dir), &seal(data_dir, secret)?)
}

/// Forget the secret. The key stays: others may be sealed under it.
pub fn delete(name: &str, data_dir: &Path) {
    match std::fs::remove_file(file(name, data_dir)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => log::warn!("couldn't delete {name}: {e:#}"),
        _ => {}
    }
}

/// The login the Slint version of ytm-native kept (`session.enc`, the youtube.com
/// cookies as a JSON map), as a `Cookie` header, for signing in once with it.
pub fn legacy_cookie(data_dir: &Path) -> Option<String> {
    let plain = read(&data_dir.join("session.enc"), data_dir)
        .inspect_err(|e| log::warn!("the earlier version's login: {e:#}"))
        .ok()
        .flatten()?;
    let cookies: std::collections::BTreeMap<String, String> = serde_json::from_slice(&plain).ok()?;
    cookies.contains_key("SAPISID").then(|| cookies.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("; "))
}

#[cfg(test)]
mod tests {
    #[test]
    fn round_trip_without_plain_text_on_disk() {
        let dir = std::env::temp_dir().join(format!("ytm-native-vault-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(super::get("account", &dir).is_none());
        super::set("account", &dir, b"cookie=dummy-value").unwrap();
        let raw = std::fs::read(super::file("account", &dir)).unwrap();
        assert!(!raw.windows(11).any(|w| w == b"dummy-value"), "plain text on disk");
        assert_eq!(super::get("account", &dir).as_deref(), Some(&b"cookie=dummy-value"[..]));
        super::delete("account", &dir);
        assert!(super::get("account", &dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_earlier_versions_login_becomes_a_cookie_header() {
        let dir = std::env::temp_dir().join(format!("ytm-native-vault-legacy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sealed = super::seal(&dir, br#"{"SID":"b","SAPISID":"a"}"#).unwrap();
        std::fs::write(dir.join("session.enc"), sealed).unwrap();
        assert_eq!(super::legacy_cookie(&dir).as_deref(), Some("SAPISID=a; SID=b"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
