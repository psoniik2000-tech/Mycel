//! The main server's mailbox (DESIGN.md §4.6): the `mailbox/worker.js` HTTP API on the relay
//! port, so the main server keeps envelopes for everyone who is offline. Push wake-ups (FCM)
//! stay with the worker; `/push/` is accepted and ignored.
//! Files `box/<topic>`: lines "<seq> <ts> <id> <base64>"; kept 30 days.

use anyhow::{Result, ensure};
use data_encoding::BASE64;
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const DAYS: u64 = 30;
const MAX_BODY: usize = 1 << 20;
const PAGE: usize = 200;

pub(crate) struct ServerBox {
    dir: PathBuf,
    st: Mutex<Inner>,
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

impl ServerBox {
    pub(crate) fn open(data: &Path) -> Result<Self> {
        let dir = data.join("box");
        std::fs::create_dir_all(&dir)?;
        let b = Self {
            dir,
            st: Mutex::new(Inner {
                seq: 0,
                ids: HashSet::new(),
                pruned: 0,
            }),
        };
        b.prune(crate::now_ms()?)?;
        Ok(b)
    }

    /// Drops lines older than 30 days and rebuilds the id set and the next seq.
    fn prune(&self, now: u64) -> Result<()> {
        let mut st = self.st.lock().unwrap();
        let cut = now.saturating_sub(DAYS * 86_400_000);
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

    /// `POST /box/<topic>/<id>`: keeps the envelope once.
    pub(crate) fn post(&self, topic: &str, id: &str, body: &[u8]) -> Result<()> {
        ensure!(hex64(topic) && hex64(id), "bad topic or id");
        ensure!(body.len() <= MAX_BODY, "too big");
        let now = crate::now_ms()?;
        if now - self.st.lock().unwrap().pruned > 3_600_000 {
            self.prune(now)?;
        }
        let mut st = self.st.lock().unwrap();
        if !st.ids.insert((topic.into(), id.into())) {
            return Ok(());
        }
        st.seq += 1;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join(topic))?;
        f.write_all(format!("{} {now} {id} {}\n", st.seq, BASE64.encode(body)).as_bytes())?;
        Ok(())
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
}

/// Answers a mailbox request, or gives it back when it is not one (the relay serves it).
#[cfg(not(target_os = "android"))]
#[allow(clippy::result_large_err)] // the request goes back to the relay
pub(crate) async fn serve(
    b: &ServerBox,
    req: hyper::Request<hyper::body::Incoming>,
) -> std::result::Result<
    hyper::Response<iroh_relay::server::http_server::BytesBody>,
    hyper::Request<hyper::body::Incoming>,
> {
    use http_body_util::{BodyExt, Full, Limited};
    let path = req.uri().path().to_string();
    let parts: Vec<&str> = path.split('/').collect();
    let reply = |code: u16, text: String| {
        let body: iroh_relay::server::http_server::BytesBody =
            Box::new(Full::new(hyper::body::Bytes::from(text)));
        hyper::Response::builder()
            .status(code)
            .body(body)
            .expect("response builds")
    };
    let get = req.method() == hyper::Method::GET;
    match parts[..] {
        ["", ""] if get => Ok(reply(200, "Mycel mailbox works (main server)".into())),
        ["", "push", _] => Ok(reply(200, "ok".into())),
        ["", "box", topic] if get => {
            let after = req
                .uri()
                .query()
                .and_then(|q| q.strip_prefix("after="))
                .and_then(|n| n.parse().ok())
                .unwrap_or(0);
            Ok(match b.get(topic, after) {
                Ok(t) => reply(200, t),
                Err(e) => reply(400, e.to_string()),
            })
        }
        ["", "box", topic, id] if req.method() == hyper::Method::POST => {
            let (topic, id) = (topic.to_string(), id.to_string());
            let body = match Limited::new(req.into_body(), MAX_BODY).collect().await {
                Ok(c) => c.to_bytes(),
                Err(_) => return Ok(reply(413, "too big".into())),
            };
            Ok(match b.post(&topic, &id, &body) {
                Ok(()) => reply(200, "ok".into()),
                Err(e) => reply(400, e.to_string()),
            })
        }
        _ => Err(req),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_once_pages_and_survives_reopen() {
        let d = tempfile::tempdir().unwrap();
        let b = ServerBox::open(d.path()).unwrap();
        let (t, i, j) = ("a".repeat(64), "b".repeat(64), "c".repeat(64));
        b.post(&t, &i, b"one").unwrap();
        b.post(&t, &i, b"one").unwrap();
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
}
