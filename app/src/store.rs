//! Encrypted storage for the YouTube login (cookies).
//!
//! The cookies are encrypted with XChaCha20-Poly1305 under a random 256-bit key.
//! Where the key lives:
//! - Linux: the desktop's secret store (Secret Service: KWallet on Plasma, GNOME Keyring
//!   elsewhere), never on disk next to the data.
//! - Windows: a file next to the data, itself encrypted with DPAPI (bound to the Windows user
//!   account; not the Credential Manager). Copied elsewhere, it can't be decrypted.
//!
//! File layout: `YTMN1` | 24-byte nonce | ciphertext.
//! These calls block (D-Bus, disk): run them via `spawn_blocking`.

use std::{collections::HashMap, fs, io::Write, path::PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead, Generate, Key, KeyInit},
};

const MAGIC: &[u8] = b"YTMN1";

pub type Cookies = HashMap<String, String>;

fn path() -> PathBuf {
    crate::paths::data_dir().join("session.enc")
}

#[cfg(target_os = "linux")]
mod keystore {
    use anyhow::{Result, anyhow};

    fn entry() -> Result<keyring::Entry> {
        keyring::Entry::new("ytm-native", "session-encryption-key").map_err(|e| anyhow!("system keyring unavailable: {e}"))
    }
    pub fn get() -> Result<Option<Vec<u8>>> {
        match entry()?.get_secret() {
            Ok(b) => Ok(Some(b)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(anyhow!("can't read the key from the system keyring: {e}")),
        }
    }
    pub fn set(key: &[u8]) -> Result<()> {
        entry()?.set_secret(key).map_err(|e| anyhow!("can't save the key to the system keyring: {e}"))
    }
    pub fn delete() -> Result<()> {
        match entry()?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(anyhow!("can't delete the key from the system keyring: {e}")),
        }
    }
}

#[cfg(windows)]
mod keystore {
    //! DPAPI (CryptProtectData) for the current Windows user; the protected key is a file.
    use std::{fs, path::PathBuf, ptr};

    use anyhow::{Result, bail};
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::Cryptography::{CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData},
    };

    fn path() -> PathBuf {
        crate::paths::data_dir().join("session.key")
    }

    fn run(input: &[u8], protect: bool) -> Result<Vec<u8>> {
        let inp = CRYPT_INTEGER_BLOB { cbData: input.len() as u32, pbData: input.as_ptr() as *mut u8 };
        let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: ptr::null_mut() };
        // SAFETY: valid input blob; the output buffer is allocated by Windows and freed below.
        let ok = unsafe {
            if protect {
                CryptProtectData(&inp, ptr::null(), ptr::null(), ptr::null(), ptr::null(), CRYPTPROTECT_UI_FORBIDDEN, &mut out)
            } else {
                CryptUnprotectData(&inp, ptr::null_mut(), ptr::null(), ptr::null(), ptr::null(), CRYPTPROTECT_UI_FORBIDDEN, &mut out)
            }
        };
        if ok == 0 || out.pbData.is_null() {
            bail!("Windows couldn't {} the login key (different user or PC?)", if protect { "protect" } else { "unlock" });
        }
        // SAFETY: Windows returned `cbData` bytes at `pbData`; copied out, then freed once.
        let data = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec() };
        unsafe { LocalFree(out.pbData as _) };
        Ok(data)
    }

    pub fn get() -> Result<Option<Vec<u8>>> {
        match fs::read(path()) {
            Ok(blob) => Ok(Some(run(&blob, false)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
    pub fn set(key: &[u8]) -> Result<()> {
        let blob = run(key, true)?;
        fs::create_dir_all(crate::paths::data_dir())?;
        fs::write(path(), blob)?;
        Ok(())
    }
    pub fn delete() -> Result<()> {
        match fs::remove_file(path()) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

fn existing_key() -> Result<Option<Key<XChaCha20Poly1305>>> {
    match keystore::get()? {
        Some(bytes) => {
            let key: [u8; 32] = bytes.try_into().map_err(|_| anyhow!("stored key has the wrong length"))?;
            Ok(Some(key.into()))
        }
        None => Ok(None),
    }
}

fn key_or_create() -> Result<Key<XChaCha20Poly1305>> {
    if let Some(k) = existing_key()? {
        return Ok(k);
    }
    let key = Key::<XChaCha20Poly1305>::generate();
    keystore::set(key.as_slice())?;
    Ok(key)
}

/// Encrypt and save (atomically, owner-only permissions).
pub fn save(cookies: &Cookies) -> Result<()> {
    let key = key_or_create()?;
    let cipher = XChaCha20Poly1305::new(&key);
    let nonce = XNonce::generate();
    let plain = serde_json::to_vec(cookies)?;
    let sealed = cipher.encrypt(&nonce, plain.as_ref()).map_err(|_| anyhow!("encryption failed"))?;

    let p = path();
    let dir = p.parent().unwrap();
    fs::create_dir_all(dir)?;
    let tmp = p.with_extension("enc.tmp");
    {
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(MAGIC)?;
        f.write_all(nonce.as_slice())?;
        f.write_all(&sealed)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, &p)?;
    Ok(())
}

/// Load the stored login, if there is one.
pub fn load() -> Result<Option<Cookies>> {
    let p = path();
    let data = match fs::read(&p) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).context("reading stored login"),
    };
    if data.len() < MAGIC.len() + 24 || !data.starts_with(MAGIC) {
        bail!("stored login file is damaged");
    }
    let key = existing_key()?.ok_or_else(|| anyhow!("the stored login's key is missing from the system keyring"))?;
    let nonce = XNonce::try_from(&data[MAGIC.len()..MAGIC.len() + 24]).map_err(|_| anyhow!("bad nonce"))?;
    let plain = XChaCha20Poly1305::new(&key)
        .decrypt(&nonce, &data[MAGIC.len() + 24..])
        .map_err(|_| anyhow!("stored login can't be decrypted (wrong key or damaged file)"))?;
    Ok(Some(serde_json::from_slice(&plain)?))
}

/// Forget the login: delete the encrypted file and the key.
pub fn clear() -> Result<()> {
    match fs::remove_file(path()) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    keystore::delete()
}

#[cfg(test)]
mod tests {
    // Touches the real keyring and data dir; run explicitly: cargo test -p ytm-native -- --ignored store
    #[test]
    #[ignore]
    fn store_roundtrip() {
        assert!(super::load().unwrap().is_none(), "a real login is stored; not overwriting it");
        let mut c = super::Cookies::new();
        c.insert("TEST".into(), "dummy-value".into());
        super::save(&c).unwrap();
        let raw = std::fs::read(super::path()).unwrap();
        assert!(!raw.windows(11).any(|w| w == b"dummy-value"), "plaintext visible on disk");
        assert_eq!(super::load().unwrap().unwrap(), c);
        super::clear().unwrap();
        assert!(super::load().unwrap().is_none());
    }
}

