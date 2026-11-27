//! The main server's mailbox (DESIGN.md §4.6): the `mailbox/worker.js` HTTP API on the relay
//! port (envelopes, FCM push wake-ups) plus encrypted media blobs for authors who are offline,
//! rate limits, quotas and an optional allowlist (`allow.json`).
//! Files in the data dir: `box/<topic>` lines "<seq> <ts> <id> <base64>"; `blob/<hash>_<key>`;
//! `push.json`; `fcm.json` (Firebase service account; without it pushes are off). Kept 30 days.

use anyhow::{Context, Result, ensure};
use data_encoding::{BASE64, HEXLOWER};
use iroh_blobs::Hash;
use mycel_crypto::{SecretKey, sign_bytes, verify_bytes};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const DAYS: u64 = 30;
const DAY_MS: u64 = 86_400_000;
const MAX_BODY: usize = 1 << 20;
/// Largest blob the server keeps; bigger media still comes only from members online.
pub(crate) const MAX_BLOB: usize = 100 << 20;
/// Blob bytes one key may keep at a time.
const KEY_QUOTA: u64 = 2 << 30;
/// Envelope bytes one topic may keep at a time.
const TOPIC_MAX: u64 = 64 << 20;
const PAGE: usize = 200;
/// Error text of a full quota (HTTP 413).
const FULL: &str = "quota exceeded";
/// How far a signed request's time may be from the server's.
const AUTH_WINDOW_MS: u64 = 600_000;

/// FCM token → (device tag, registered at).
type Tokens = BTreeMap<String, (Option<String>, u64)>;

pub(crate) struct ServerBox {
    dir: PathBuf,
    data: PathBuf,
    st: Mutex<Inner>,
    /// topic → FCM token → (device tag, registered at).
    push: Mutex<BTreeMap<String, Tokens>>,
    /// hash → (uploader key hex, size, ts).
    blobs: Mutex<HashMap<String, (String, u64, u64)>>,
    access: Access,
    rate: Rate,
    #[cfg(not(target_os = "android"))]
    fcm: Option<crate::fcm::Fcm>,
}

struct Inner {
    seq: u64,
    ids: HashSet<(String, String)>,
    pruned: u64,
}

fn hex64(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// (seq, ts, id, base64) of each line.
fn lines(file: &Path) -> Vec<(u64, u64, String, String)> {
    let text = std::fs::read_to_string(file).unwrap_or_default();
    text.lines()
        .filter_map(|l| {
            let mut p = l.splitn(4, ' ');
            Some((
                p.next()?.parse().ok()?,
                p.next()?.parse().ok()?,
                p.next()?.to_string(),
                p.next()?.to_string(),
            ))
        })
        .collect()
}

/// Fixed one-minute windows per key ("what who").
#[derive(Default)]
pub(crate) struct Rate(Mutex<HashMap<String, (u64, u32)>>);

impl Rate {
    /// Counts a request; false once `max` per minute is passed.
    pub(crate) fn hit(&self, key: &str, max: u32) -> bool {
        let min = crate::now_ms().unwrap_or(0) / 60_000;
        let mut m = self.0.lock().unwrap();
        if m.len() > 100_000 {
            m.retain(|_, v| v.0 == min);
        }
        let e = m.entry(key.to_string()).or_insert((min, 0));
        if e.0 != min {
            *e = (min, 0);
        }
        e.1 += 1;
        e.1 <= max
    }
}

/// `allow.json`: {"<device key hex>" | "@username": ["dir", "box", "files"]}. Empty or missing:
/// the server is open to everyone.
pub type AllowList = BTreeMap<String, Vec<String>>;
pub const ROLES: [&str; 3] = ["dir", "box", "files"];

pub fn allowlist(data: &Path) -> Result<AllowList> {
    match std::fs::read(data.join("allow.json")) {
        Ok(b) => Ok(serde_json::from_slice(&b).context("allow.json")?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(AllowList::new()),
        Err(e) => Err(e.into()),
    }
}

fn save_allow(data: &Path, l: &AllowList) -> Result<()> {
    std::fs::create_dir_all(data)?;
    let tmp = data.join("allow.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(l)?)?;
    std::fs::rename(&tmp, data.join("allow.json"))?;
    Ok(())
}

/// "@name" or a device key (hex or endpoint id) → the allowlist entry.
fn entry(who: &str) -> Result<String> {
    if who.starts_with('@') {
        let u = crate::dir::username(who).context("username: 5–32 of a-z, 0-9, _")?;
        return Ok(format!("@{u}"));
    }
    let id: mycel_net::EndpointId = who.trim().parse().context("not a key or @username")?;
    Ok(HEXLOWER.encode(id.as_bytes()))
}

/// Adds or replaces `who` with `roles` (all when empty). Returns the entry.
pub fn allow(data: &Path, who: &str, roles: &[String]) -> Result<String> {
    let e = entry(who)?;
    for r in roles {
        ensure!(ROLES.contains(&r.as_str()), "roles: dir, box, files");
    }
    let roles = match roles {
        [] => ROLES.map(String::from).to_vec(),
        r => r.to_vec(),
    };
    let mut l = allowlist(data)?;
    l.insert(e.clone(), roles);
    save_allow(data, &l)?;
    Ok(e)
}

/// Removes `who`; false if it was not listed.
pub fn deny(data: &Path, who: &str) -> Result<bool> {
    let mut l = allowlist(data)?;
    let had = l.remove(&entry(who)?).is_some();
    save_allow(data, &l)?;
    Ok(had)
}

/// Files a backup keeps.
const BACKUP: [&str; 3] = ["directory.json", "dir_key", "allow.json"];

/// Writes the directory, its key and the allowlist into `file` (JSON: name → base64).
pub fn backup(data: &Path, file: &Path) -> Result<usize> {
    let mut m = BTreeMap::new();
    for n in BACKUP {
        match std::fs::read(data.join(n)) {
            Ok(b) => {
                m.insert(n, BASE64.encode(&b));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    std::fs::write(file, serde_json::to_vec_pretty(&m)?)?;
    Ok(m.len())
}

/// Puts the files of a [`backup`] back into `data` (the server must be stopped).
pub fn restore(data: &Path, file: &Path) -> Result<usize> {
    let m: BTreeMap<String, String> = serde_json::from_slice(&std::fs::read(file)?)?;
    std::fs::create_dir_all(data)?;
    for (n, b) in &m {
        ensure!(BACKUP.contains(&n.as_str()), "unknown file {n}");
        std::fs::write(data.join(n), BASE64.decode(b.as_bytes())?)?;
    }
    Ok(m.len())
}

/// Who may do what, from `allow.json` in the data dir.
pub(crate) struct Access(PathBuf);

impl Access {
    pub(crate) fn new(data: &Path) -> Self {
        Self(data.to_path_buf())
    }

    /// `key` (or its username, `name` or the directory's) holds `role`, or the list is empty.
    // shortcut: reads allow.json (and directory.json for @names) per check, so CLI edits apply at once; cache by mtime if it shows in profiles.
    pub(crate) fn allows(&self, key: Option<&[u8; 32]>, name: Option<&str>, role: &str) -> bool {
        let Ok(l) = allowlist(&self.0) else {
            return false; // a broken list closes the server rather than opening it
        };
        if l.is_empty() {
            return true;
        }
        let Some(key) = key else {
            return false;
        };
        let has = |e: &str| l.get(e).is_some_and(|r| r.iter().any(|r| r == role));
        if has(&HEXLOWER.encode(key)) {
            return true;
        }
        let name = match name {
            Some(n) => Some(n.to_string()),
            None => crate::dir::name_in(&self.0, key),
        };
        name.is_some_and(|n| has(&format!("@{n}")))
    }
}

fn auth_msg(method: &str, path: &str, ts: u64, body: &[u8]) -> Vec<u8> {
    let h = Hash::new(body);
    postcard::to_allocvec(&(b"mycel-http", method, path, ts, h.as_bytes())).expect("serializes")
}

/// Headers that sign a request (`path` with its query) with the device key, so a closed
/// server knows who asks and counts quotas per key.
pub(crate) fn sign(
    key: &SecretKey,
    method: &str,
    path: &str,
    body: &[u8],
) -> Vec<(&'static str, String)> {
    let ts = crate::now_ms().unwrap_or(0);
    let sig = sign_bytes(key, &auth_msg(method, path, ts, body));
    vec![
        ("x-mycel-key", HEXLOWER.encode(key.public().as_bytes())),
        ("x-mycel-ts", ts.to_string()),
        ("x-mycel-sig", HEXLOWER.encode(&sig)),
    ]
}

/// The signing key of a request: None when unsigned, an error when the signature is bad.
#[cfg(not(target_os = "android"))]
fn signer(h: &http::HeaderMap, method: &str, path: &str, body: &[u8]) -> Result<Option<[u8; 32]>> {
    let get = |n: &str| h.get(n).and_then(|v| v.to_str().ok());
    let Some(key) = get("x-mycel-key") else {
        return Ok(None);
    };
    let key: [u8; 32] = HEXLOWER
        .decode(key.as_bytes())
        .ok()
        .and_then(|k| k.try_into().ok())
        .context("bad key")?;
    let ts: u64 = get("x-mycel-ts")
        .and_then(|t| t.parse().ok())
        .context("bad time")?;
    ensure!(
        crate::now_ms()?.abs_diff(ts) < AUTH_WINDOW_MS,
        "clock is off by over 10 minutes"
    );
    let sig = HEXLOWER
        .decode(get("x-mycel-sig").unwrap_or("").as_bytes())
        .context("bad signature")?;
    ensure!(
        verify_bytes(&key, &auth_msg(method, path, ts, body), &sig),
        "bad signature"
    );
    Ok(Some(key))
}

impl ServerBox {
    pub(crate) fn open(data: &Path) -> Result<Self> {
        let dir = data.join("box");
        std::fs::create_dir_all(&dir)?;
        std::fs::create_dir_all(data.join("blob"))?;
        let push = match std::fs::read(data.join("push.json")) {
            Ok(b) => serde_json::from_slice(&b).unwrap_or_default(),
            Err(_) => BTreeMap::new(),
        };
        let mut blobs = HashMap::new();
        for e in std::fs::read_dir(data.join("blob"))?.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            let Some((hash, key)) = name.split_once('_') else {
                continue; // a ".tmp" left by a crash
            };
            let m = e.metadata()?;
            let ts = m
                .modified()?
                .duration_since(std::time::UNIX_EPOCH)?
                .as_millis() as u64;
            blobs.insert(hash.to_string(), (key.to_string(), m.len(), ts));
        }
        let b = Self {
            dir,
            data: data.to_path_buf(),
            st: Mutex::new(Inner {
                seq: 0,
                ids: HashSet::new(),
                pruned: 0,
            }),
            push: Mutex::new(push),
            blobs: Mutex::new(blobs),
            access: Access::new(data),
            rate: Rate::default(),
            #[cfg(not(target_os = "android"))]
            fcm: crate::fcm::Fcm::load(&data.join("fcm.json"))?,
        };
        b.prune(crate::now_ms()?)?;
        Ok(b)
    }

    fn maybe_prune(&self) -> Result<()> {
        let now = crate::now_ms()?;
        if now - self.st.lock().unwrap().pruned > 3_600_000 {
            self.prune(now)?;
        }
        Ok(())
    }

    /// Drops envelopes and blobs older than 30 days and push tokens not renewed in 60, and
    /// rebuilds the id set and the next seq.
    fn prune(&self, now: u64) -> Result<()> {
        let cut = now.saturating_sub(DAYS * DAY_MS);
        self.blobs.lock().unwrap().retain(|h, (k, _, ts)| {
            *ts >= cut
                || std::fs::remove_file(self.data.join("blob").join(format!("{h}_{k}"))).is_err()
        });
        {
            let mut p = self.push.lock().unwrap();
            p.values_mut()
                .for_each(|t| t.retain(|_, v| v.1 >= now.saturating_sub(60 * DAY_MS)));
            p.retain(|_, t| !t.is_empty());
        }
        self.save_push()?;
        let mut st = self.st.lock().unwrap();
        st.ids.clear();
        for e in std::fs::read_dir(&self.dir)?.flatten() {
            let topic = e.file_name().to_string_lossy().to_string();
            let all = lines(&e.path());
            let keep: Vec<_> = all.iter().filter(|l| l.1 >= cut).collect();
            for l in &all {
                st.seq = st.seq.max(l.0);
            }
            for l in &keep {
                st.ids.insert((topic.clone(), l.2.clone()));
            }
            if keep.len() < all.len() {
                let text: String = keep
                    .iter()
                    .map(|(s, t, i, b)| format!("{s} {t} {i} {b}\n"))
                    .collect();
                std::fs::write(e.path(), text)?;
            }
        }
        st.pruned = now;
        Ok(())
    }

    fn save_push(&self) -> Result<()> {
        let tmp = self.data.join("push.tmp");
        std::fs::write(&tmp, serde_json::to_vec(&*self.push.lock().unwrap())?)?;
        std::fs::rename(&tmp, self.data.join("push.json"))?;
        Ok(())
    }

    /// `POST /push/<topic>`: an FCM token of a phone in that space, `device` its tag.
    pub(crate) fn add_push(&self, topic: &str, token: &str, device: &str) -> Result<()> {
        let token = token.trim();
        ensure!(hex64(topic), "bad topic");
        ensure!(!token.is_empty() && token.len() <= 4096, "bad token");
        let device = hex64(device).then(|| device.to_string());
        self.push
            .lock()
            .unwrap()
            .entry(topic.into())
            .or_default()
            .insert(token.into(), (device, crate::now_ms()?));
        self.save_push()
    }

    /// Tokens to wake for a new envelope: all of the topic but the sender's, or only the
    /// devices tagged in `to` (1:1 chat; untagged old phones get every push).
    fn targets(&self, topic: &str, from: &str, to: &str) -> Vec<String> {
        let tags: Vec<&str> = to.split(',').filter(|t| hex64(t)).take(16).collect();
        let p = self.push.lock().unwrap();
        p.get(topic)
            .into_iter()
            .flatten()
            .filter(|(t, (d, _))| {
                *t != from && (tags.is_empty() || d.as_deref().is_none_or(|d| tags.contains(&d)))
            })
            .map(|(t, _)| t.clone())
            .collect()
    }

    fn forget_push(&self, token: &str) {
        for t in self.push.lock().unwrap().values_mut() {
            t.remove(token);
        }
        let _ = self.save_push();
    }

    /// `POST /box/<topic>/<id>`: keeps the envelope once; true if it is new.
    pub(crate) fn post(&self, topic: &str, id: &str, body: &[u8]) -> Result<bool> {
        ensure!(hex64(topic) && hex64(id), "bad topic or id");
        ensure!(body.len() <= MAX_BODY, "too big");
        self.maybe_prune()?;
        let now = crate::now_ms()?;
        let mut st = self.st.lock().unwrap();
        if st.ids.contains(&(topic.into(), id.into())) {
            return Ok(false);
        }
        let file = self.dir.join(topic);
        let used = std::fs::metadata(&file).map_or(0, |m| m.len());
        ensure!(used + body.len() as u64 <= TOPIC_MAX, FULL);
        st.ids.insert((topic.into(), id.into()));
        st.seq += 1;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(file)?;
        f.write_all(format!("{} {now} {id} {}\n", st.seq, BASE64.encode(body)).as_bytes())?;
        Ok(true)
    }

    /// `GET /box/<topic>?after=N`: "<seq> <base64>" lines, oldest first, at most 200.
    pub(crate) fn get(&self, topic: &str, after: u64) -> Result<String> {
        ensure!(hex64(topic), "bad topic");
        let _st = self.st.lock().unwrap();
        Ok(lines(&self.dir.join(topic))
            .into_iter()
            .filter(|l| l.0 > after)
            .take(PAGE)
            .map(|(s, _, _, b)| format!("{s} {b}\n"))
            .collect())
    }

    /// `POST /blob/<hash>`: keeps an encrypted blob whose BLAKE3 hash is `hash`.
    pub(crate) fn put_blob(&self, hash: &str, key: &[u8; 32], body: &[u8]) -> Result<()> {
        ensure!(hex64(hash), "bad hash");
        ensure!(body.len() <= MAX_BLOB, "too big");
        ensure!(Hash::new(body).to_hex().as_str() == hash, "hash mismatch");
        self.maybe_prune()?;
        let key = HEXLOWER.encode(key);
        let mut m = self.blobs.lock().unwrap();
        if m.contains_key(hash) {
            return Ok(());
        }
        let used: u64 = m.values().filter(|v| v.0 == key).map(|v| v.1).sum();
        ensure!(used + body.len() as u64 <= KEY_QUOTA, FULL);
        let dir = self.data.join("blob");
        let tmp = dir.join(format!("{hash}.tmp"));
        std::fs::write(&tmp, body)?;
        std::fs::rename(&tmp, dir.join(format!("{hash}_{key}")))?;
        m.insert(hash.into(), (key, body.len() as u64, crate::now_ms()?));
        Ok(())
    }

    /// `GET /blob/<hash>`.
    pub(crate) fn blob(&self, hash: &str) -> Result<Option<Vec<u8>>> {
        ensure!(hex64(hash), "bad hash");
        let Some((key, ..)) = self.blobs.lock().unwrap().get(hash).cloned() else {
            return Ok(None);
        };
        Ok(Some(std::fs::read(
            self.data.join("blob").join(format!("{hash}_{key}")),
        )?))
    }

    /// Sends the wake-ups for a new envelope.
    #[cfg(not(target_os = "android"))]
    async fn wake(&self, topic: &str, from: &str, to: &str) {
        let Some(f) = &self.fcm else {
            return;
        };
        for t in self.targets(topic, from, to) {
            match f.send(&t).await {
                Ok(true) => {}
                Ok(false) => self.forget_push(&t),
                Err(e) => eprintln!("mycel: push failed: {e}"),
            }
        }
    }
}

/// Answers a mailbox request, or gives it back when it is not one (the relay serves it).
#[cfg(not(target_os = "android"))]
#[allow(clippy::result_large_err)] // the request goes back to the relay
pub(crate) async fn serve(
    b: std::sync::Arc<ServerBox>,
    ip: std::net::IpAddr,
    req: hyper::Request<hyper::body::Incoming>,
) -> std::result::Result<
    hyper::Response<iroh_relay::server::http_server::BytesBody>,
    hyper::Request<hyper::body::Incoming>,
> {
    use http_body_util::{BodyExt, Full, Limited};
    use hyper::Method;
    let path = req.uri().path().to_string();
    let pq = req
        .uri()
        .path_and_query()
        .map_or(path.clone(), |p| p.as_str().to_string());
    let reply = |code: u16, body: hyper::body::Bytes| {
        let body: iroh_relay::server::http_server::BytesBody = Box::new(Full::new(body));
        hyper::Response::builder()
            .status(code)
            .body(body)
            .expect("response builds")
    };
    let text = |code: u16, t: &str| reply(code, t.to_string().into());
    let parts: Vec<&str> = path.split('/').collect();
    let m = req.method().clone();
    // (kind, requests per minute per IP and per key, role).
    let (kind, max, role) = match (&m, &parts[..]) {
        (&Method::GET, ["", ""]) => return Ok(text(200, "Mycel mailbox works (main server)")),
        (&Method::POST, ["", "push", _]) => ("push", 20, "box"),
        (&Method::GET, ["", "box", _]) | (&Method::POST, ["", "box", _, _]) => ("box", 600, "box"),
        (&Method::GET | &Method::POST, ["", "blob", _]) => ("blob", 120, "files"),
        _ => return Err(req),
    };
    if !b.rate.hit(&format!("{kind} {ip}"), max) {
        return Ok(text(429, "too many requests, try later"));
    }
    let h = req.headers().clone();
    let limit = if kind == "blob" { MAX_BLOB } else { MAX_BODY };
    let body = match Limited::new(req.into_body(), limit).collect().await {
        Ok(c) => c.to_bytes(),
        Err(_) => return Ok(text(413, "too big")),
    };
    let key = match signer(&h, m.as_str(), &pq, &body) {
        Ok(k) => k,
        Err(e) => return Ok(text(401, &e.to_string())),
    };
    if let Some(k) = &key
        && !b.rate.hit(&format!("{kind} {}", HEXLOWER.encode(k)), max)
    {
        return Ok(text(429, "too many requests, try later"));
    }
    if !b.access.allows(key.as_ref(), None, role) {
        return Ok(text(403, "not allowed on this server"));
    }
    let hv = |n: &str| {
        h.get(n)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    };
    let res: Result<hyper::body::Bytes> = match (&m, &parts[..]) {
        (&Method::POST, ["", "push", topic]) => b
            .add_push(topic, &String::from_utf8_lossy(&body), &hv("x-device"))
            .map(|()| "ok".into()),
        (&Method::GET, ["", "box", topic]) => {
            let after = pq
                .split_once("after=")
                .and_then(|(_, n)| n.split('&').next()?.parse().ok())
                .unwrap_or(0);
            b.get(topic, after).map(Into::into)
        }
        (&Method::POST, ["", "box", topic, id]) => b.post(topic, id, &body).map(|new| {
            if new && hv("x-push") == "1" && b.fcm.is_some() {
                let (b, t, from, to) = (b.clone(), topic.to_string(), hv("x-from"), hv("x-to"));
                tokio::spawn(async move { b.wake(&t, &from, &to).await });
            }
            "ok".into()
        }),
        (&Method::POST, ["", "blob", hash]) => match &key {
            Some(k) => b.put_blob(hash, k, &body).map(|()| "ok".into()),
            None => return Ok(text(401, "unsigned")),
        },
        (&Method::GET, ["", "blob", hash]) => match b.blob(hash) {
            Ok(Some(v)) => Ok(v.into()),
            Ok(None) => return Ok(text(404, "no such blob")),
            Err(e) => Err(e),
        },
        _ => unreachable!("matched above"),
    };
    Ok(match res {
        Ok(v) => reply(200, v),
        Err(e) if e.to_string() == FULL => text(413, FULL),
        Err(e) => text(400, &e.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_once_pages_and_survives_reopen() {
        let d = tempfile::tempdir().unwrap();
        let b = ServerBox::open(d.path()).unwrap();
        let (t, i, j) = ("a".repeat(64), "b".repeat(64), "c".repeat(64));
        assert!(b.post(&t, &i, b"one").unwrap());
        assert!(!b.post(&t, &i, b"one").unwrap());
        b.post(&t, &j, b"two").unwrap();
        assert!(b.post("x", &i, b"").is_err());
        let got = b.get(&t, 0).unwrap();
        assert_eq!(got.lines().count(), 2);
        assert_eq!(
            b.get(&t, 1).unwrap(),
            format!("2 {}\n", BASE64.encode(b"two"))
        );
        let b = ServerBox::open(d.path()).unwrap();
        b.post(&t, &j, b"two").unwrap();
        b.post(&t, &"d".repeat(64), b"three").unwrap();
        assert!(b.get(&t, 2).unwrap().starts_with("3 "));
    }

    #[test]
    fn push_targets_skip_sender_and_follow_tags() {
        let d = tempfile::tempdir().unwrap();
        let b = ServerBox::open(d.path()).unwrap();
        assert!(b.fcm.is_none(), "no fcm.json: pushes off");
        let (t, x, y) = ("a".repeat(64), "1".repeat(64), "2".repeat(64));
        b.add_push(&t, "tx", &x).unwrap();
        b.add_push(&t, "ty", &y).unwrap();
        b.add_push(&t, "old", "").unwrap();
        assert_eq!(b.targets(&t, "tx", ""), ["old", "ty"]);
        assert_eq!(b.targets(&t, "", &x), ["old", "tx"]);
        b.forget_push("old");
        let b = ServerBox::open(d.path()).unwrap();
        assert_eq!(b.targets(&t, "", &y), ["ty"]);
    }

    #[test]
    fn blobs_check_hash_and_quota() {
        let d = tempfile::tempdir().unwrap();
        let b = ServerBox::open(d.path()).unwrap();
        let h = Hash::new(b"data").to_hex().to_string();
        assert!(b.put_blob(&h, &[1; 32], b"other").is_err(), "hash mismatch");
        b.put_blob(&h, &[1; 32], b"data").unwrap();
        assert_eq!(b.blob(&h).unwrap().unwrap(), b"data");
        assert!(b.blob(&"0".repeat(64)).unwrap().is_none());
        // Over the key's quota.
        b.blobs.lock().unwrap().get_mut(&h).unwrap().1 = KEY_QUOTA;
        let h2 = Hash::new(b"more").to_hex().to_string();
        let e = b.put_blob(&h2, &[1; 32], b"more").unwrap_err();
        assert_eq!(e.to_string(), FULL);
        b.put_blob(&h2, &[2; 32], b"more").unwrap();
        let b = ServerBox::open(d.path()).unwrap();
        assert_eq!(b.blob(&h2).unwrap().unwrap(), b"more");
    }
}
