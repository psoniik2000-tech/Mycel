//! Encrypted backup file `.mycelbak` (DESIGN §10): the history and, optionally, the media of
//! spaces, sealed by a password. No device key and no MLS state: a restored device is a new
//! device that joins its groups again (reusing old ratchet keys would reuse AEAD nonces).
//!
//! Format: `MAGIC | salt 16 | nonce prefix 19 | segments`, a segment `u32 LE len | ciphertext`
//! of the STREAM construction (XChaCha20-Poly1305, Argon2id key), so reordering and truncation
//! fail. The plaintext is a run of `u32 LE len | postcard (tag, a, b)` items.

use anyhow::{Context, Result, bail, ensure};
use chacha20poly1305::aead::stream::{DecryptorBE32, EncryptorBE32};
use chacha20poly1305::{KeyInit, XChaCha20Poly1305};
use mycel_proto::Event;
use mycel_store::Store;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 8] = b"MYCELBK1";
const SEGMENT: usize = 1 << 20;
const FILE_PART: usize = 256 << 10;
/// Item tags: (SPACE, name, -), (EVENT, chat, postcard Event), (FILE, media file name, bytes).
const SPACE: u8 = 0;
const EVENT: u8 = 1;
const FILE: u8 = 2;
pub const BAD_PASSWORD: &str = "wrong password or damaged backup";

fn cipher(password: &str, salt: &[u8]) -> Result<XChaCha20Poly1305> {
    let mut key = [0u8; 32];
    argon2::Argon2::default()
        .hash_password_into(password.as_bytes(), salt, &mut key)
        .map_err(|e| anyhow::anyhow!("argon2: {e}"))?;
    Ok(XChaCha20Poly1305::new(&key.into()))
}

struct Sealer {
    out: BufWriter<std::fs::File>,
    enc: EncryptorBE32<XChaCha20Poly1305>,
    buf: Vec<u8>,
}

impl Sealer {
    fn item(&mut self, tag: u8, a: &[u8], b: &[u8]) -> Result<()> {
        let it = postcard::to_allocvec(&(tag, a, b))?;
        self.buf.extend_from_slice(&(it.len() as u32).to_le_bytes());
        self.buf.extend_from_slice(&it);
        while self.buf.len() >= SEGMENT {
            let rest = self.buf.split_off(SEGMENT);
            let ct = self
                .enc
                .encrypt_next(self.buf.as_slice())
                .map_err(|_| anyhow::anyhow!("encrypt"))?;
            write_segment(&mut self.out, &ct)?;
            self.buf = rest;
        }
        Ok(())
    }

    fn finish(self) -> Result<()> {
        let mut out = self.out;
        let ct = self
            .enc
            .encrypt_last(self.buf.as_slice())
            .map_err(|_| anyhow::anyhow!("encrypt"))?;
        write_segment(&mut out, &ct)?;
        out.flush()?;
        out.into_inner()?.sync_all()?;
        Ok(())
    }
}

fn write_segment(out: &mut impl Write, ct: &[u8]) -> Result<()> {
    out.write_all(&(ct.len() as u32).to_le_bytes())?;
    Ok(out.write_all(ct)?)
}

/// Writes a backup of `spaces` — (name, data folder, store password) — to `out`.
/// `media`: also the decrypted media files this device has.
pub fn write(
    out: &Path,
    password: &str,
    spaces: &[(&str, &Path, &str)],
    media: bool,
) -> Result<()> {
    let res = write_inner(out, password, spaces, media);
    if res.is_err() {
        let _ = std::fs::remove_file(out);
    }
    res
}

fn write_inner(
    out: &Path,
    password: &str,
    spaces: &[(&str, &Path, &str)],
    media: bool,
) -> Result<()> {
    let mut salt = [0u8; 16];
    let mut nonce = [0u8; 19];
    getrandom::fill(&mut salt)?;
    getrandom::fill(&mut nonce)?;
    let mut f = BufWriter::new(std::fs::File::create(out)?);
    f.write_all(MAGIC)?;
    f.write_all(&salt)?;
    f.write_all(&nonce)?;
    let mut s = Sealer {
        out: f,
        enc: EncryptorBE32::from_aead(cipher(password, &salt)?, (&nonce).into()),
        buf: Vec::new(),
    };
    for (name, dir, pw) in spaces {
        let store = Store::open(dir, pw).with_context(|| format!("space «{name}»"))?;
        s.item(SPACE, name.as_bytes(), &[])?;
        for (chat, ev) in store.all_events()? {
            s.item(EVENT, &chat, &ev)?;
        }
        drop(store);
        if !media {
            continue;
        }
        let Ok(rd) = std::fs::read_dir(dir.join("media")) else {
            continue;
        };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            // A `.part` is a half-decrypted file (Node::media_path).
            if !e.file_type()?.is_file() || name.ends_with(".part") {
                continue;
            }
            let mut r = std::fs::File::open(e.path())?;
            let mut part = vec![0u8; FILE_PART];
            loop {
                let n = r.read(&mut part)?;
                if n == 0 {
                    break;
                }
                s.item(FILE, name.as_bytes(), &part[..n])?;
            }
        }
    }
    s.finish()
}

/// Restores a backup: for each space calls `space(name)` for an empty data folder and fills it,
/// the store keyed by `store_password`. Returns the restored spaces' names.
/// A wrong password fails before `space` is called.
pub fn restore(
    file: &Path,
    password: &str,
    store_password: &str,
    mut space: impl FnMut(&str) -> Result<PathBuf>,
) -> Result<Vec<String>> {
    let mut f = BufReader::new(std::fs::File::open(file)?);
    let mut head = [0u8; 8 + 16 + 19];
    f.read_exact(&mut head).context("not a Mycel backup")?;
    ensure!(&head[..8] == MAGIC, "not a Mycel backup");
    let nonce: [u8; 19] = head[24..].try_into()?;
    let mut dec = Some(DecryptorBE32::from_aead(
        cipher(password, &head[8..24])?,
        (&nonce).into(),
    ));
    let mut buf = Vec::new();
    let mut cur: Option<(Store, PathBuf)> = None;
    let mut names = vec![];
    let mut next = read_segment(&mut f)?.context(BAD_PASSWORD)?;
    loop {
        let after = read_segment(&mut f)?;
        let pt = match (&after, dec.take()) {
            (None, Some(d)) => d.decrypt_last(next.as_slice()),
            (Some(_), Some(mut d)) => {
                let r = d.decrypt_next(next.as_slice());
                dec = Some(d);
                r
            }
            (_, None) => unreachable!(),
        }
        .map_err(|_| anyhow::anyhow!(BAD_PASSWORD))?;
        buf.extend_from_slice(&pt);
        let mut at = 0;
        while buf.len() >= at + 4 {
            let len = u32::from_le_bytes(buf[at..at + 4].try_into()?) as usize;
            if buf.len() < at + 4 + len {
                break;
            }
            let (tag, a, b): (u8, Vec<u8>, Vec<u8>) =
                postcard::from_bytes(&buf[at + 4..at + 4 + len])?;
            at += 4 + len;
            match tag {
                SPACE => {
                    let name = String::from_utf8(a)?;
                    let dir = space(&name)?;
                    cur = Some((Store::open(&dir, store_password)?, dir));
                    names.push(name);
                }
                EVENT => {
                    let (store, _) = cur.as_ref().context("damaged backup")?;
                    let ev: Event = postcard::from_bytes(&b)?;
                    store.insert_event(&ev.id, &a, ev.lamport, ev.timestamp_ms, &b)?;
                }
                FILE => {
                    let (_, dir) = cur.as_ref().context("damaged backup")?;
                    let name = String::from_utf8(a)?;
                    // Only a bare file name: the backup is untrusted input.
                    let name = Path::new(&name).file_name().context("bad file name")?;
                    std::fs::create_dir_all(dir.join("media"))?;
                    std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(dir.join("media").join(name))?
                        .write_all(&b)?;
                }
                _ => bail!("damaged backup"),
            }
        }
        buf.drain(..at);
        match after {
            Some(s) => next = s,
            None => break,
        }
    }
    ensure!(buf.is_empty(), "damaged backup");
    Ok(names)
}

fn read_segment(f: &mut impl Read) -> Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    match f.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_le_bytes(len) as usize;
    // An encrypted segment is SEGMENT plus the 16-byte tag at most.
    ensure!(len <= SEGMENT + 16, "damaged backup");
    let mut ct = vec![0u8; len];
    f.read_exact(&mut ct).context("damaged backup")?;
    Ok(Some(ct))
}
