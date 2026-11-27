//! Username directory on a main server (DESIGN.md §4.6): @username → a signed public contact link.
//! The server only maps names to links; chats stay end-to-end encrypted and P2P.

use crate::serverbox::{Access, Rate};
use crate::{CONTACT_LINK, Mailbox, Node, hex};
use anyhow::{Context, Result, bail, ensure};
use data_encoding::BASE32_NOPAD;
use mycel_crypto::{SecretKey, sign_bytes, verify_bytes};
use mycel_net::{Connection, DIR_ALPN, EndpointAddr, EndpointId, EndpointTicket, Net, RelayUrl};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Path on the main server's relay port that answers its directory's endpoint id.
pub const ID_PATH: &str = "/mycel-id";
/// Most names a search returns.
const SEARCH_MAX: usize = 20;
const DIR_TIMEOUT: Duration = Duration::from_secs(15);
/// Invisible last char of a record's name: its owner does not want to come up in search (no
/// wire change; an older server or app still lists it).
const UNLISTED: char = '\u{2063}';

/// A username's entry: the account's public contact link, signed by the key in it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct DirRecord {
    pub username: String,
    /// Display name.
    pub name: String,
    /// Reusable `mycel://contact/` link (§4.2).
    pub link: String,
    /// Device key that signed it, the endpoint id in `link`.
    pub key: [u8; 32],
    /// Unix ms; an update must be newer.
    pub ts: u64,
    pub sig: Vec<u8>,
    /// Account that owns the name (§4.4: its first device); zero = `key` (older records).
    #[serde(default)]
    pub account: [u8; 32],
    /// The account's signature over [`device_cert_msg`] of `key`; empty when `key` is the account.
    #[serde(default)]
    pub cert: Vec<u8>,
}

/// What an account root signs to let a linked device update the account's username.
// shortcut: an unlinked device keeps a valid cert; add a revocation list if that matters.
pub(crate) fn device_cert_msg(dev: &[u8; 32]) -> Vec<u8> {
    postcard::to_allocvec(&(b"mycel-device", dev)).expect("cert serializes")
}

impl DirRecord {
    fn msg(&self) -> Vec<u8> {
        postcard::to_allocvec(&(
            b"mycel-dir",
            &self.username,
            &self.name,
            &self.link,
            &self.key,
            self.ts,
        ))
        .expect("record serializes")
    }

    /// The account that owns this name.
    pub fn owner(&self) -> [u8; 32] {
        if self.account == [0; 32] {
            self.key
        } else {
            self.account
        }
    }

    pub fn sign(mut self, key: &SecretKey) -> Self {
        self.key = *key.public().as_bytes();
        self.sig = sign_bytes(key, &self.msg());
        self
    }

    /// Signature, name rules and that the link really is a contact link of `key`.
    fn check(&self) -> Result<()> {
        ensure!(
            username(&self.username).as_deref() == Some(self.username.as_str()),
            "bad username"
        );
        ensure!(self.name.chars().count() <= 64, "name too long");
        ensure!(
            verify_bytes(&self.key, &self.msg(), &self.sig),
            "bad signature"
        );
        ensure!(link_owner(&self.link)? == self.key, "link of another key");
        ensure!(
            self.owner() == self.key
                || verify_bytes(&self.account, &device_cert_msg(&self.key), &self.cert),
            "device of another account"
        );
        Ok(())
    }
}

/// Endpoint id named in a contact link.
fn link_owner(link: &str) -> Result<[u8; 32]> {
    let b32 = link
        .strip_prefix(CONTACT_LINK)
        .context("not a contact link")?;
    let (_, ticket, _): ([u8; 16], String, Vec<String>) =
        postcard::from_bytes(&BASE32_NOPAD.decode(b32.as_bytes())?)?;
    let to: EndpointAddr = ticket.parse::<EndpointTicket>()?.into();
    Ok(*to.id.as_bytes())
}

/// Normalized username (lowercase, 5–32 of `a-z 0-9 _`, a leading @ dropped) or None.
pub fn username(s: &str) -> Option<String> {
    let u = s.trim().trim_start_matches('@').to_lowercase();
    let ok = (5..=32).contains(&u.len())
        && u.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    ok.then_some(u)
}

#[derive(Serialize, Deserialize, Debug)]
enum DirReq {
    Register(DirRecord),
    Lookup(String),
    /// Usernames starting with this prefix.
    Search(String),
    /// Username of a key (reverse lookup).
    NameOf([u8; 32]),
}

#[derive(Serialize, Deserialize, Debug)]
enum DirResp {
    Ok,
    Err(String),
    Found(Option<DirRecord>),
    /// (username, display name).
    List(Vec<(String, String)>),
}

/// Error text when a username belongs to another key.
pub const DIR_TAKEN: &str = "username taken";

/// The names and their records, kept in a JSON file; one mutex makes check-and-set atomic.
struct Db {
    file: PathBuf,
    names: BTreeMap<String, DirRecord>,
    /// Account → its username.
    by_acct: HashMap<[u8; 32], String>,
}

impl Db {
    fn load(file: PathBuf) -> Result<Self> {
        let names: BTreeMap<String, DirRecord> = match std::fs::read(&file) {
            Ok(b) => serde_json::from_slice(&b)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(e.into()),
        };
        let by_acct = names.iter().map(|(n, r)| (r.owner(), n.clone())).collect();
        Ok(Self {
            file,
            names,
            by_acct,
        })
    }

    fn save(&self) -> Result<()> {
        let tmp = self.file.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec(&self.names)?)?;
        std::fs::rename(&tmp, &self.file)?;
        Ok(())
    }

    fn handle(&mut self, req: DirReq) -> DirResp {
        match req {
            DirReq::Register(r) => match self.register(r) {
                Ok(()) => DirResp::Ok,
                Err(e) => DirResp::Err(e.to_string()),
            },
            DirReq::Lookup(u) => {
                DirResp::Found(username(&u).and_then(|u| self.names.get(&u).cloned()))
            }
            DirReq::Search(p) => {
                let p = p.trim().trim_start_matches('@').to_lowercase();
                if p.len() < 2 {
                    return DirResp::List(vec![]);
                }
                DirResp::List(
                    self.names
                        .range(p.clone()..)
                        .take_while(|(n, _)| n.starts_with(&p))
                        .filter(|(_, r)| !r.name.ends_with(UNLISTED))
                        .take(SEARCH_MAX)
                        .map(|(n, r)| (n.clone(), r.name.clone()))
                        .collect(),
                )
            }
            // An account, or the device that wrote the record.
            DirReq::NameOf(k) => DirResp::Found(
                self.by_acct
                    .get(&k)
                    .and_then(|n| self.names.get(n))
                    .or_else(|| self.names.values().find(|r| r.key == k))
                    .cloned(),
            ),
        }
    }

    /// First come first served; any device of the owner account may update its record or move
    /// it to a free name.
    fn register(&mut self, r: DirRecord) -> Result<()> {
        r.check()?;
        let acct = r.owner();
        if let Some(old) = self.names.get(&r.username) {
            ensure!(old.owner() == acct, DIR_TAKEN);
            ensure!(r.ts > old.ts, "stale record");
        }
        if let Some(prev) = self.by_acct.get(&acct)
            && *prev != r.username
        {
            ensure!(
                self.names.get(prev).is_none_or(|o| r.ts > o.ts),
                "stale record"
            );
            self.names.remove(prev);
        }
        self.by_acct.insert(acct, r.username.clone());
        self.names.insert(r.username.clone(), r);
        self.save()
    }
}

/// Username of `key` in the directory kept in `data` (main server side).
pub(crate) fn name_in(data: &Path, key: &[u8; 32]) -> Option<String> {
    Db::load(data.join("directory.json"))
        .ok()?
        .by_key
        .remove(key)
}

/// Directory service of a main server: its own iroh endpoint (stable key in `dir_key`), reached
/// through the server's relay; records in `directory.json`. Stops when dropped.
pub struct DirServer {
    net: Net,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for DirServer {
    fn drop(&mut self) {
        self.task.abort();
        // A restarted server binds the same key: the old endpoint must leave the relay.
        let net = self.net.clone();
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move { net.close().await });
        }
    }
}

impl DirServer {
    /// `relay`: the server's own relay; the endpoint joins it over loopback (no NAT loopback needed).
    pub async fn spawn(data: &Path, relay: &str) -> Result<Self> {
        std::fs::create_dir_all(data)?;
        let kf = data.join("dir_key");
        let key = match std::fs::read(&kf) {
            Ok(b) => SecretKey::from_bytes(&b.try_into().ok().context("bad dir_key")?),
            Err(_) => {
                let k = SecretKey::generate();
                std::fs::write(&kf, k.to_bytes())?;
                k
            }
        };
        let u: RelayUrl = relay.parse()?;
        let port = u
            .port_or_known_default()
            .context("relay url without port")?;
        let local = format!("http://127.0.0.1:{port}");
        let db = Arc::new(Mutex::new(Db::load(data.join("directory.json"))?));
        let (net, _, _, mut conns) = Net::bind(key, 0, &[local], false).await?;
        let guard = Arc::new((Access::new(data), Rate::default()));
        let task = tokio::spawn(async move {
            while let Some(conn) = conns.recv().await {
                if conn.alpn() == DIR_ALPN {
                    tokio::spawn(serve(conn, db.clone(), guard.clone()));
                }
            }
        });
        Ok(Self { net, task })
    }

    pub fn id(&self) -> EndpointId {
        self.net.id()
    }
}

/// Error text when the allowlist (`allow.json`) does not let a key register.
pub const DIR_DENIED: &str = "not allowed on this server";
/// Error text of a rate limit.
pub const DIR_BUSY: &str = "too many requests, try later";

async fn serve(conn: Connection, db: Arc<Mutex<Db>>, guard: Arc<(Access, Rate)>) -> Result<()> {
    let (mut s, mut r) = conn.accept_bi().await?;
    let req = r.read_to_end(64 << 10).await?;
    let (access, rate) = &*guard;
    // Endpoint ids are free to make: the global cap keeps a flood from one host in check too.
    let who = conn.remote_id();
    let resp = match postcard::from_bytes::<DirReq>(&req) {
        _ if !rate.hit(&format!("dir {who}"), 120) || !rate.hit("dir *", 3000) => {
            DirResp::Err(DIR_BUSY.into())
        }
        Ok(DirReq::Register(r)) if !rate.hit(&format!("reg {}", hex(&r.key)), 10) => {
            DirResp::Err(DIR_BUSY.into())
        }
        Ok(DirReq::Register(r)) if !access.allows(Some(&r.key), Some(&r.username), "dir") => {
            DirResp::Err(DIR_DENIED.into())
        }
        Ok(req) => db.lock().unwrap().handle(req),
        Err(e) => DirResp::Err(e.to_string()),
    };
    s.write_all(&postcard::to_allocvec(&resp)?).await?;
    s.finish()?;
    let _ = tokio::time::timeout(DIR_TIMEOUT, conn.closed()).await;
    Ok(())
}

/// A main server setting `http://host:port` or `http://host:port#<endpoint id>` → (relay URL,
/// directory id). Without the id it asks the relay port's [`ID_PATH`]; the caller keeps the
/// answer (`url#id`) to pin it.
pub async fn resolve(server: &str) -> Result<(RelayUrl, EndpointId)> {
    let (url, id) = match server.trim().split_once('#') {
        Some((u, id)) => (u.to_string(), id.to_string()),
        None => {
            let u = server.trim().trim_end_matches('/').to_string();
            let b = Mailbox::new(&u)?;
            let id = b
                .http
                .get(format!("{u}{ID_PATH}"))
                .timeout(Duration::from_secs(8))
                .send()
                .await
                .context("main server unreachable")?
                .error_for_status()
                .context("not a Mycel main server")?
                .text()
                .await?;
            (u, id.trim().to_string())
        }
    };
    Ok((
        url.parse().context("bad server address")?,
        id.parse().context("bad server id")?,
    ))
}

/// The pinned form of a resolved server.
pub fn pinned(url: &RelayUrl, id: &EndpointId) -> String {
    format!("{}#{id}", url.as_str().trim_end_matches('/'))
}

impl Node {
    async fn dir_call(&self, server: &str, req: &DirReq) -> Result<(String, DirResp)> {
        let (url, id) = resolve(server).await?;
        let addr = EndpointAddr::new(id).with_relay_url(url.clone());
        let call = async {
            let conn = self.net.connect(addr, DIR_ALPN).await?;
            let (mut s, mut r) = conn.open_bi().await?;
            s.write_all(&postcard::to_allocvec(req)?).await?;
            s.finish()?;
            let resp = r.read_to_end(1 << 20).await?;
            conn.close(0u32.into(), b"");
            anyhow::Ok(postcard::from_bytes::<DirResp>(&resp)?)
        };
        let resp = tokio::time::timeout(DIR_TIMEOUT, call)
            .await
            .map_err(anyhow::Error::from)
            .and_then(|r| r)
            .context("main server unreachable")?;
        if let DirResp::Err(e) = resp {
            bail!(e);
        }
        Ok((pinned(&url, &id), resp))
    }

    /// Reusable contact link for the directory: like [`Node::contact_invite`], but the secret
    /// (meta `pubcontact`) is not burned by use.
    pub async fn public_contact_link(&self) -> Result<String> {
        let ticket = self.ticket().await;
        let relays = self.relays();
        let st = self.st.lock().unwrap();
        let secret: [u8; 16] = match st.store.meta("pubcontact")? {
            Some(s) => s.try_into().ok().context("bad pubcontact")?,
            None => {
                let mut s = [0u8; 16];
                getrandom::fill(&mut s)?;
                st.store.set_meta("pubcontact", &s)?;
                s
            }
        };
        let bytes = postcard::to_allocvec(&(secret, ticket, relays))?;
        Ok(format!("{CONTACT_LINK}{}", BASE32_NOPAD.encode(&bytes)))
    }

    /// Whether `secret` is our public contact secret.
    pub(crate) fn is_public_secret(&self, secret: &[u8; 16]) -> bool {
        let st = self.st.lock().unwrap();
        st.store
            .meta("pubcontact")
            .ok()
            .flatten()
            .is_some_and(|s| s == secret)
    }

    /// Registers (or refreshes, or renames to) `username` with our name and a fresh public
    /// contact link. Returns the pinned server address.
    pub async fn dir_register(&self, server: &str, name: &str) -> Result<String> {
        let username = username(name).context("username: 5–32 of a-z, 0-9, _")?;
        let (me, account) = (self.device_id(), self.account());
        let mut display = self
            .profiles()?
            .get(&me)
            .map(|p| p.0.clone())
            .unwrap_or_default();
        if !self.privacy().findable {
            display = display.chars().take(63).chain([UNLISTED]).collect();
        }
        let cert = if account == me {
            vec![]
        } else {
            let c = self.st.lock().unwrap().store.meta("devcert")?;
            c.context("this device was linked by an older app: link it again to use the username")?
        };
        let rec = DirRecord {
            username,
            name: display,
            link: self.public_contact_link().await?,
            key: me,
            ts: crate::now_ms()?,
            sig: vec![],
            account,
            cert,
        }
        .sign(self.net.secret_key());
        Ok(self.dir_call(server, &DirReq::Register(rec)).await?.0)
    }

    pub async fn dir_lookup(&self, server: &str, name: &str) -> Result<Option<DirRecord>> {
        match self.dir_call(server, &DirReq::Lookup(name.into())).await?.1 {
            DirResp::Found(r) => Ok(r),
            _ => bail!("bad answer"),
        }
    }

    /// Record of a device key, if it has a username.
    pub async fn dir_name_of(&self, server: &str, key: [u8; 32]) -> Result<Option<DirRecord>> {
        match self.dir_call(server, &DirReq::NameOf(key)).await?.1 {
            DirResp::Found(r) => Ok(r),
            _ => bail!("bad answer"),
        }
    }

    /// Up to 20 (username, display name) starting with `prefix` (at least 2 characters).
    pub async fn dir_search(&self, server: &str, prefix: &str) -> Result<Vec<(String, String)>> {
        match self
            .dir_call(server, &DirReq::Search(prefix.into()))
            .await?
            .1
        {
            DirResp::List(mut l) => {
                // An older server lists the unlisted ones too.
                l.retain(|(_, n)| !n.ends_with(UNLISTED));
                Ok(l)
            }
            _ => bail!("bad answer"),
        }
    }

    /// Finds `name` and adds it as a contact unless we already chat; returns the chat partner.
    pub async fn dir_add(&self, server: &str, name: &str) -> Result<[u8; 32]> {
        let rec = self
            .dir_lookup(server, name)
            .await?
            .context("no such username")?;
        rec.check()?;
        let (peer, owner) = (self.account_of(&rec.key), rec.owner());
        ensure!(
            peer != self.account() && owner != self.account() && rec.key != self.device_id(),
            "this is you"
        );
        let known = {
            let st = self.st.lock().unwrap();
            st.gid(&Some(peer)).is_ok() || st.gid(&Some(owner)).is_ok()
        };
        if !known {
            self.join(&rec.link).await?;
        }
        Ok(self.account_of(&rec.key))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(key: &SecretKey, name: &str, ts: u64) -> DirRecord {
        let ticket = EndpointTicket::new(EndpointAddr::new(key.public())).to_string();
        let link = format!(
            "{CONTACT_LINK}{}",
            BASE32_NOPAD.encode(
                &postcard::to_allocvec(&([0u8; 16], ticket, Vec::<String>::new())).unwrap()
            )
        );
        DirRecord {
            username: name.into(),
            name: "N".into(),
            link,
            key: [0; 32],
            ts,
            sig: vec![],
            account: [0; 32],
            cert: vec![],
        }
        .sign(key)
    }

    #[test]
    fn update_needs_the_same_key_and_a_newer_record() {
        let d = tempfile::tempdir().unwrap();
        let mut db = Db::load(d.path().join("d.json")).unwrap();
        let (a, b) = (SecretKey::generate(), SecretKey::generate());
        db.register(rec(&a, "alice", 2)).unwrap();
        assert!(db.register(rec(&b, "alice", 3)).is_err(), "taken");
        assert!(db.register(rec(&a, "alice", 1)).is_err(), "stale replay");
        let mut forged = rec(&a, "alice", 5);
        forged.link = rec(&b, "alice", 5).link;
        assert!(db.register(forged).is_err(), "link of another key");
        let mut forged = rec(&a, "alice", 5);
        forged.name = "X".into();
        assert!(db.register(forged).is_err(), "bad signature");
        db.register(rec(&a, "alice", 6)).unwrap();
        // A reload keeps it.
        let db = Db::load(d.path().join("d.json")).unwrap();
        assert_eq!(db.names["alice"].ts, 6);
        assert!(username("Ab_cd").is_some() && username("a-bcd").is_none());
    }
}
