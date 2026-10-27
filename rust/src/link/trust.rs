//! Who is who: this PC's identity (a self-signed certificate) and the
//! tablets the user paired with (their certificates' fingerprints), in the
//! same files as plusone/link/trust.py: pairings made by either stay.
//!
//! Pairing happens only when the user asks for it: both screens show a
//! six-digit code made from the two certificates and a random number from
//! each side (the app's committed to before it sees the PC's), and the user
//! confirms it on both.

use openssl::asn1::Asn1Time;
use openssl::bn::{BigNum, MsbOption};
use openssl::ec::{EcGroup, EcKey};
use openssl::hash::MessageDigest;
use openssl::nid::Nid;
use openssl::pkey::PKey;
use openssl::sha::sha256;
use openssl::x509::{X509NameBuilder, X509};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub const MAX_NAME: usize = 64;

/// Named after the app's first name: the identity and pairings made then stay.
pub fn config_dir() -> PathBuf {
    glib::user_config_dir().join("plus-one").join("companion")
}

/// Six digits from two certificates (DER, in either order) and the PC's and
/// the app's random numbers; Identity.kt pairingCode computes the same.
pub fn pairing_code(a: &[u8], b: &[u8], ours: &[u8], theirs: &[u8]) -> String {
    let (first, second) = if a <= b { (a, b) } else { (b, a) };
    let digest = sha256(&[first, second, ours, theirs].concat());
    let n = u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]);
    format!("{:06}", n % 1_000_000)
}

/// What the app sends before it learns the PC's number: binds its own.
pub fn commitment(nonce: &[u8], app_cert: &[u8], pc_cert: &[u8]) -> String {
    hex(&sha256(&[nonce, app_cert, pc_cert].concat()))
}

pub fn fingerprint(der: &[u8]) -> String {
    hex(&sha256(der))
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn unhex(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 {
        return None;
    }
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok()).collect()
}

/// This PC's certificate and key (PEM files) and the certificate's DER.
pub struct Identity {
    pub cert: PathBuf,
    pub key: PathBuf,
    pub der: Vec<u8>,
}

static MAKING: Mutex<()> = Mutex::new(());

/// This PC's identity, made on first use: an EC P-256 key and a
/// self-signed certificate (as trust.py makes it with openssl).
pub fn identity(directory: Option<&Path>) -> io::Result<Identity> {
    let directory = directory.map(Path::to_path_buf).unwrap_or_else(config_dir);
    let (cert, key) = (directory.join("cert.pem"), directory.join("key.pem"));
    let _making = MAKING.lock().unwrap();  // two PDFs at once: one key, one certificate
    if !(cert.exists() && key.exists()) {
        make_identity(&directory, &cert, &key).map_err(io::Error::other)?;
    }
    let der = X509::from_pem(&std::fs::read(&cert)?)
        .and_then(|c| c.to_der())
        .map_err(io::Error::other)?;
    Ok(Identity { cert, key, der })
}

fn make_identity(directory: &Path, cert: &Path, key: &Path) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(directory)?;
    std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;  // the key in it is ours only
    let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1)?;
    let pkey = PKey::from_ec_key(EcKey::generate(&group)?)?;
    let mut name = X509NameBuilder::new()?;
    name.append_entry_by_nid(Nid::COMMONNAME, &format!("Plus One on {}", hostname()))?;
    let name = name.build();
    let mut serial = BigNum::new()?;
    serial.rand(128, MsbOption::MAYBE_ZERO, false)?;
    let mut x509 = X509::builder()?;
    x509.set_version(2)?;
    let serial = serial.to_asn1_integer()?;
    x509.set_serial_number(&serial)?;
    x509.set_subject_name(&name)?;
    x509.set_issuer_name(&name)?;
    x509.set_pubkey(&pkey)?;
    let (from, to) = (Asn1Time::days_from_now(0)?, Asn1Time::days_from_now(10950)?);
    x509.set_not_before(&from)?;
    x509.set_not_after(&to)?;
    x509.sign(&pkey, MessageDigest::sha256())?;
    let (key_part, cert_part) = (key.with_extension("pem.part"), cert.with_extension("pem.part"));
    write_private_bytes(&key_part, &pkey.private_key_to_pem_pkcs8()?)?;
    std::fs::write(&cert_part, x509.build().to_pem()?)?;
    std::fs::rename(&key_part, key)?;
    std::fs::rename(&cert_part, cert)?;
    Ok(())
}

/// The kernel's host name, as Python's socket.gethostname() says it.
pub fn hostname() -> String {
    let mut buf = [0u8; 256];
    unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

/// path's new contents at once, for this user only (the directory too).
pub fn write_private_bytes(path: &Path, data: &[u8]) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let directory = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(directory)?;
    std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
    let tmp = directory.join(format!(
        ".{}.{}.part",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("file"),
        std::process::id()
    ));
    let mut file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
    file.write_all(data)?;
    file.sync_all()?;
    std::fs::rename(&tmp, path)
}

pub fn write_private(path: &Path, data: &Value) -> io::Result<()> {
    write_private_bytes(path, &serde_json::to_vec(data).map_err(io::Error::other)?)
}

/// A paired tablet: its name, and where its app was last on the network.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub name: String,
    pub app_at: Option<String>,
}

/// Tablets the user paired with, by certificate fingerprint.
#[derive(Clone)]
pub struct Paired {
    path: PathBuf,
}

impl Default for Paired {
    fn default() -> Self {
        Paired::new(None)
    }
}

impl Paired {
    pub fn new(directory: Option<&Path>) -> Paired {
        Paired { path: directory.map(Path::to_path_buf).unwrap_or_else(config_dir).join("paired.json") }
    }

    pub fn entries(&self) -> BTreeMap<String, Entry> {
        let Ok(text) = std::fs::read(&self.path) else { return BTreeMap::new() };
        let Ok(Value::Object(data)) = serde_json::from_slice::<Value>(&text) else { return BTreeMap::new() };
        let mut entries = BTreeMap::new();
        for (fp, entry) in data {
            match entry {
                Value::String(name) => {
                    entries.insert(fp, Entry { name, app_at: None });  // 0.5 kept the name only
                }
                Value::Object(fields) => {
                    if let Some(Value::String(name)) = fields.get("name") {
                        let app_at = fields.get("app_at").and_then(Value::as_str).map(str::to_owned);
                        entries.insert(fp, Entry { name: name.clone(), app_at });
                    }
                }
                _ => {}
            }
        }
        entries
    }

    /// {fingerprint: name}
    pub fn all(&self) -> BTreeMap<String, String> {
        self.entries().into_iter().map(|(fp, e)| (fp, e.name)).collect()
    }

    fn save(&self, entries: &BTreeMap<String, Entry>) -> io::Result<()> {
        let mut data = Map::new();
        for (fp, entry) in entries {
            let mut fields = Map::new();
            fields.insert("name".into(), Value::String(entry.name.clone()));
            if let Some(at) = &entry.app_at {
                fields.insert("app_at".into(), Value::String(at.clone()));
            }
            data.insert(fp.clone(), Value::Object(fields));
        }
        write_private(&self.path, &Value::Object(data))
    }

    pub fn add(&self, fp: &str, name: &str) -> io::Result<()> {
        let mut data = self.entries();
        let app_at = data.get(fp).and_then(|e| e.app_at.clone());
        data.insert(fp.to_owned(), Entry { name: name.to_owned(), app_at });
        self.save(&data)
    }

    /// Where a paired tablet's app was last on the network ("ip:port").
    pub fn learn_app_at(&self, fp: &str, at: &str) -> io::Result<()> {
        let mut data = self.entries();
        match data.get_mut(fp) {
            Some(entry) if entry.app_at.as_deref() != Some(at) => {
                entry.app_at = Some(at.to_owned());
                self.save(&data)
            }
            _ => Ok(()),
        }
    }

    pub fn forget(&self, fp: &str) -> io::Result<()> {
        let mut data = self.entries();
        if data.remove(fp).is_some() {
            self.save(&data)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_code_is_the_same_whichever_certificate_comes_first() {
        let code = pairing_code(b"aa", b"bb", b"n1", b"n2");
        assert_eq!(code, pairing_code(b"bb", b"aa", b"n1", b"n2"));
        assert_eq!(code.len(), 6);
        // As trust.py: sha256(b"aabbn1n2")[:4] big-endian % 1_000_000.
        let digest = sha256(b"aabbn1n2");
        let n = u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]) % 1_000_000;
        assert_eq!(code, format!("{n:06}"));
    }

    #[test]
    fn pairings_of_0_5_are_kept() {
        let dir = std::env::temp_dir().join(format!("plusone-trust-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("paired.json"), br#"{"aaaa": "Tab", "bbbb": 3}"#).unwrap();
        let store = Paired::new(Some(&dir));
        assert_eq!(store.all().into_iter().collect::<Vec<_>>(), vec![("aaaa".into(), "Tab".into())]);
        store.learn_app_at("aaaa", "10.0.0.7:47236").unwrap();
        store.add("aaaa", "Tab S10").unwrap();  // paired again: what was learned stays
        assert_eq!(store.entries()["aaaa"], Entry { name: "Tab S10".into(), app_at: Some("10.0.0.7:47236".into()) });
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn hex_both_ways() {
        assert_eq!(hex(&[0, 255, 16]), "00ff10");
        assert_eq!(unhex("00ff10"), Some(vec![0, 255, 16]));
        assert_eq!(unhex("0g"), None);
        assert_eq!(unhex("abc"), None);
    }
}
