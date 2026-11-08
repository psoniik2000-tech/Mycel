//! Thin node API: space create/invite/join, 1:1 chats, send_text, incoming event stream, history, sync.

use anyhow::{Context, Result, bail, ensure};
use data_encoding::BASE32_NOPAD;
use iroh_blobs::Hash;
use iroh_blobs::api::remote::GetProgressItem;
use iroh_blobs::provider::events::EventSender;
use iroh_blobs::store::GcConfig;
use iroh_blobs::store::fs::{FsStore, options::Options};
use mycel_crypto::mls::{Incoming, Mls};
use mycel_crypto::{SecretKey, envelope_id, sign_addr, sign_event, verify_addr, verify_event};
use mycel_net::{
    BLOBS_ALPN, CALL_ALPN, ConnInbox, EndpointAddr, EndpointId, EndpointTicket, JoinInbox, Net,
};
use mycel_proto::{
    BlobRef, Envelope, Event, EventBody, EventId, JoinReq, JoinResp, MediaKind, ReceiptKind,
    SignedAddr, SyncMsg,
};
use mycel_store::Store;
use n0_future::StreamExt;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;

mod call;
pub use call::{AUDIO, Frame, LinkStats, VIDEO_DELTA, VIDEO_KEY};
pub use mycel_net::RelayUrl;
pub use mycel_proto;

const LINK: &str = "mycel://join/";
/// Join error prefixes (the UI shows a matching text).
pub const JOIN_BAD_LINK: &str = "not a mycel invite link";
pub const JOIN_UNREACHABLE: &str = "inviter unreachable";
pub const JOIN_USED: &str = "invite already used or unknown";
const JOIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(40);

/// Network options of a node.
#[derive(Debug, Default, Clone)]
pub struct Opts {
    /// iroh UDP port, 0 = any free one.
    pub port: u16,
    /// URL of the iroh-relay this node serves (anchor), advertised to the members.
    pub own_relay: Option<String>,
    /// Also use the public n0 relays and address lookup (off by default, last resort).
    pub n0: bool,
    /// Download every media blob as soon as its event arrives (anchors, §8.4).
    pub keep_blobs: bool,
    /// Largest media file to send or download, bytes; 0 = 4 GiB (§8.1).
    pub max_blob: u64,
}

/// A chat: `None` is the space chat, `Some(device)` the 1:1 chat with that member (§17).
pub type Chat = Option<[u8; 32]>;

/// A member profile: (name, avatar letter, color index, last-resort KeyPackage).
pub type Profile = (String, String, u8, Vec<u8>);

fn chat_key(c: &Chat) -> &[u8] {
    c.as_ref().map_or(&[], |p| &p[..])
}

fn chat_from(k: &[u8]) -> Chat {
    k.try_into().ok()
}

// shortcut: a node is in exactly one space = one MLS group, plus one 2-member group per 1:1 chat.
struct State {
    store: Store,
    mls: Mls,
}

impl State {
    fn save_mls(&self) -> Result<()> {
        self.store.set_meta("mls", &self.mls.state())
    }

    fn in_space(&self) -> Result<bool> {
        Ok(self.store.meta("group")?.is_some())
    }

    /// (group id, partner) of every 1:1 group; both sides may have created one, so a partner
    /// can have two, and the smaller id is the one we send to.
    fn dms(&self) -> Result<Vec<([u8; 32], [u8; 32])>> {
        let r = self.store.meta("dms")?;
        Ok(r.map(|b| postcard::from_bytes(&b))
            .transpose()?
            .unwrap_or_default())
    }

    fn add_dm(&self, gid: [u8; 32], peer: [u8; 32]) -> Result<()> {
        let mut d = self.dms()?;
        d.push((gid, peer));
        self.store.set_meta("dms", &postcard::to_allocvec(&d)?)
    }

    /// The MLS group a chat sends to.
    fn gid(&self, chat: &Chat) -> Result<[u8; 32]> {
        match chat {
            None => self.group(),
            Some(p) => self
                .dms()?
                .into_iter()
                .filter(|(_, q)| q == p)
                .map(|(g, _)| g)
                .min()
                .context("no chat with this member"),
        }
    }

    /// The chat of a known group.
    fn chat_of(&self, gid: &[u8; 32]) -> Result<Option<Chat>> {
        if *gid == self.group()? {
            return Ok(Some(None));
        }
        Ok(self
            .dms()?
            .into_iter()
            .find(|(g, _)| g == gid)
            .map(|(_, p)| Some(p)))
    }

    fn group(&self) -> Result<[u8; 32]> {
        let g = self.store.meta("group")?.context("not in a space")?;
        g.try_into().ok().context("bad group id")
    }

    fn admin(&self) -> Result<[u8; 32]> {
        let a = self.store.meta("admin")?.context("not in a space")?;
        a.try_into().ok().context("bad admin id")
    }

    /// Relay URLs learned from invites and peer exchange.
    fn relays(&self) -> Result<Vec<String>> {
        let r = self.store.meta("relays")?;
        Ok(r.map(|b| postcard::from_bytes(&b))
            .transpose()?
            .unwrap_or_default())
    }

    fn envelope(&self, group_id: [u8; 32], ciphertext: Vec<u8>, epoch: u64) -> Result<Envelope> {
        Ok(Envelope {
            version: 1,
            space_id: self.group()?[..16].try_into()?,
            group_id,
            epoch,
            ciphertext,
        })
    }

    fn store_env(&self, env: &Envelope) -> Result<()> {
        self.store
            .insert_envelope(&envelope_id(env), &postcard::to_allocvec(env)?)
    }

    fn store_event(&self, ev: &Event, chat: &Chat) -> Result<bool> {
        self.store.insert_event(
            &ev.id,
            chat_key(chat),
            ev.lamport,
            ev.timestamp_ms,
            &postcard::to_allocvec(ev)?,
        )
    }

    /// Decrypts and stores one envelope; returns the new events (several when it is the
    /// Welcome of a 1:1 chat whose messages came first).
    fn receive(&mut self, env: &Envelope, me: [u8; 32]) -> Result<Vec<(Chat, Event)>> {
        ensure!(env.space_id == self.group()?[..16], "foreign space");
        if self.store.has_envelope(&envelope_id(env))? {
            return Ok(vec![]);
        }
        let out = match self.chat_of(&env.group_id)? {
            Some(chat) => self.open(env, chat)?.into_iter().collect(),
            // Someone else's 1:1 chat (kept to forward it), or the Welcome to a new one with us.
            None => self.join_dm(env, me).unwrap_or_default(),
        };
        self.save_mls()?;
        self.store_env(env)?;
        Ok(out)
    }

    fn open(&mut self, env: &Envelope, chat: Chat) -> Result<Option<(Chat, Event)>> {
        // No commits in 1:1 groups: the Welcome already holds both members.
        let admin = if chat.is_none() {
            self.admin()?
        } else {
            [0; 32]
        };
        Ok(
            match self.mls.process(&env.group_id, &env.ciphertext, &admin) {
                Ok(Incoming::App { sender, data }) => {
                    let ev: Event = postcard::from_bytes(&data)?;
                    ensure!(verify_event(&ev), "bad signature");
                    ensure!(ev.author_device == sender, "author is not the MLS sender");
                    self.store_event(&ev, &chat)?.then_some((chat, ev))
                }
                Ok(Incoming::Commit) => None,
                // Before our join (or past retention): unreadable for us, kept so we can forward it.
                Err(_) if env.epoch < self.mls.epoch(&env.group_id) => None,
                Err(e) => return Err(e),
            },
        )
    }

    /// Joins a 1:1 group from its Welcome (fails for anyone but the invited member), then
    /// reads the messages of that group that are already here.
    fn join_dm(&mut self, env: &Envelope, me: [u8; 32]) -> Result<Vec<(Chat, Event)>> {
        let gid = self.mls.join(&env.ciphertext)?;
        ensure!(gid == env.group_id, "welcome for another group");
        let m = self.mls.members(&gid);
        let peer = *m.iter().find(|p| **p != me).context("no partner")?;
        let space = self.mls.members(&self.group()?);
        ensure!(m.len() == 2 && m.contains(&me), "not a 1:1 group");
        ensure!(space.contains(&peer), "partner is not a space member");
        self.add_dm(env.group_id, peer)?;
        let mut out = vec![];
        for (_, data) in self.store.envelopes()? {
            let e: Envelope = postcard::from_bytes(&data)?;
            if e.group_id == env.group_id
                && let Ok(Some(x)) = self.open(&e, Some(peer))
            {
                out.push(x);
            }
        }
        Ok(out)
    }
}

pub struct Node {
    st: Mutex<State>,
    net: Net,
    own_relay: Option<String>,
    n0: bool,
    /// Encrypted media blobs (ciphertext, by BLAKE3 hash); a tag per hash keeps it from GC.
    blobs: FsStore,
    dir: PathBuf,
    keep_blobs: bool,
    max_blob: u64,
    call: Mutex<Option<call::Call>>,
    frames: Mutex<Option<mpsc::Sender<call::Frame>>>,
}

impl Node {
    /// Opens the encrypted store (creating the device key on first run), binds the network and
    /// syncs with known peers. Returns the node and a stream of new verified events from peers.
    pub async fn open(
        dir: &Path,
        password: &str,
        opts: &Opts,
    ) -> Result<(Arc<Self>, mpsc::Receiver<Event>)> {
        let store = Store::open(dir, password)?;
        let key = match store.meta("device_key")? {
            Some(b) => SecretKey::from_bytes(&b.try_into().ok().context("bad device key")?),
            None => {
                let k = SecretKey::generate();
                store.set_meta("device_key", &k.to_bytes())?;
                k
            }
        };
        let st = State {
            mls: Mls::new(&key, None, &[])?,
            store,
        };
        let mut gids: Vec<Vec<u8>> = st.store.meta("group")?.into_iter().collect();
        gids.extend(st.dms()?.iter().map(|(g, _)| g.to_vec()));
        let mls = Mls::new(&key, st.store.meta("mls")?.as_deref(), &gids)?;
        let st = State { mls, ..st };
        let mut relays = st.relays()?;
        relays.extend(opts.own_relay.clone());
        let (net, mut inbox, joins, conns) = Net::bind(key, opts.port, &relays, opts.n0).await?;
        let mut bopts = Options::new(&dir.join("blobs"));
        bopts.gc = Some(GcConfig {
            interval: std::time::Duration::from_secs(3600),
            add_protected: None,
        });
        let blobs = FsStore::load_with_opts(dir.join("blobs").join("blobs.db"), bopts).await?;
        let node = Arc::new(Self {
            st: Mutex::new(st),
            net,
            own_relay: opts.own_relay.clone(),
            n0: opts.n0,
            blobs,
            dir: std::path::absolute(dir)?,
            keep_blobs: opts.keep_blobs,
            max_blob: if opts.max_blob == 0 {
                4 << 30
            } else {
                opts.max_blob
            },
            call: Mutex::new(None),
            frames: Mutex::new(None),
        });
        let (tx, rx) = mpsc::channel(256);
        let n = node.clone();
        tokio::spawn(async move {
            while let Some((from, msg)) = inbox.recv().await {
                let before = n.members().len();
                let res = n.handle(from, msg);
                // Someone joined: they cannot read our older profile, so send it again.
                if n.members().len() > before
                    && let Err(e) = n.refresh_profile(true)
                {
                    eprintln!("mycel: profile resend failed: {e}");
                }
                match res {
                    Ok(mut evs) => {
                        if !evs.is_empty() {
                            match n.apply_deletes() {
                                Ok(gone) => evs.retain(|(_, e)| !gone.contains(&e.id)),
                                Err(e) => eprintln!("mycel: delete failed: {e}"),
                            }
                        }
                        n.send_receipt(&evs);
                        let evs: Vec<Event> = evs.into_iter().map(|(_, e)| e).collect();
                        if n.keep_blobs {
                            n.clone().fetch_all(evs.clone());
                        }
                        for ev in evs {
                            n.on_call_event(&ev);
                            let _ = tx.send(ev).await;
                        }
                    }
                    Err(e) => eprintln!("mycel: dropped message from {}: {e}", from.fmt_short()),
                }
            }
        });
        tokio::spawn(node.clone().serve_joins(joins));
        tokio::spawn(node.clone().serve_conns(conns));
        if node.keep_blobs {
            node.clone().fetch_missing();
        }
        node.sync()?;
        if let Err(e) = node.refresh_profile(false) {
            eprintln!("mycel: profile refresh failed: {e}");
        }
        if !relays.is_empty() {
            // Again once a relay is up, so peers learn our relay address.
            let n = node.clone();
            tokio::spawn(async move {
                n.net.online().await;
                let _ = n.sync();
            });
        }
        Ok((node, rx))
    }

    /// Ticket with this node's id and current addresses (waits briefly for a relay if we know any).
    pub async fn ticket(&self) -> String {
        self.wait_relay().await;
        EndpointTicket::new(self.net.addr()).to_string()
    }

    /// Without a relay in our address, a peer in another network cannot reach us.
    async fn wait_relay(&self) {
        if self.n0 || !self.relays().is_empty() {
            self.net.online().await;
        }
    }

    /// Relay URLs this node uses (own and learned).
    pub fn relays(&self) -> Vec<String> {
        let mut r = self.st.lock().unwrap().relays().unwrap_or_default();
        r.extend(self.own_relay.clone());
        r
    }

    pub fn device_id(&self) -> [u8; 32] {
        *self.net.id().as_bytes()
    }

    /// Creates a new space with this device as the only admin.
    pub fn create_space(&self) -> Result<()> {
        let mut st = self.st.lock().unwrap();
        ensure!(!st.in_space()?, "already in a space");
        let mut gid = [0u8; 32];
        getrandom::fill(&mut gid)?;
        st.mls.create_group(&gid)?;
        st.store.set_meta("group", &gid)?;
        st.store.set_meta("admin", &self.device_id())?;
        st.save_mls()
    }

    /// One-time invite link: base32(space id, one-time secret, admin ticket, relay URLs).
    /// The ticket's id is the admin key.
    pub async fn invite(&self) -> Result<String> {
        let ticket = self.ticket().await;
        let relays = self.relays();
        let mut secret = [0u8; 16];
        getrandom::fill(&mut secret)?;
        let st = self.st.lock().unwrap();
        ensure!(st.admin()? == self.device_id(), "only the admin can invite");
        st.store
            .set_meta(&format!("invite:{}", hex(&secret)), &[])?;
        let bytes = postcard::to_allocvec(&(st.group()?, secret, ticket, relays))?;
        Ok(format!("{LINK}{}", BASE32_NOPAD.encode(&bytes)))
    }

    /// Joins a space by invite link: proves the secret, sends a KeyPackage, applies the Welcome.
    /// Errors start with [`JOIN_BAD_LINK`], [`JOIN_UNREACHABLE`] or [`JOIN_USED`] when they are those.
    pub async fn join(&self, link: &str) -> Result<()> {
        // Messengers and notes wrap long links: drop every whitespace, not only the ends.
        let link: String = link.split_whitespace().collect();
        let parsed = || -> Result<_> {
            let b32 = link.strip_prefix(LINK).context("no mycel://join/ prefix")?;
            let (gid, secret, ticket, relays): ([u8; 32], [u8; 16], String, Vec<String>) =
                postcard::from_bytes(&BASE32_NOPAD.decode(b32.as_bytes())?)?;
            let admin: EndpointAddr = ticket.parse::<EndpointTicket>()?.into();
            Ok((gid, secret, ticket, relays, admin))
        };
        let (gid, secret, ticket, relays, admin) = parsed().context(JOIN_BAD_LINK)?;
        {
            let st = self.st.lock().unwrap();
            ensure!(!st.in_space()?, "already in a space");
            self.learn_relays(&st, &relays)?;
        }
        self.wait_relay().await;
        let req = JoinReq {
            secret,
            key_package: self.st.lock().unwrap().mls.key_package(false)?,
            addr: self.my_addr()?,
        };
        let admin_id = admin.id;
        let resp = tokio::time::timeout(
            JOIN_TIMEOUT,
            self.net.join(admin, &postcard::to_allocvec(&req)?),
        )
        .await
        .map_err(anyhow::Error::from)
        .and_then(|r| r)
        .context(JOIN_UNREACHABLE)?;
        // The admin answers an empty message when it refuses (unknown or used secret).
        ensure!(!resp.is_empty(), JOIN_USED);
        let resp: JoinResp = postcard::from_bytes(&resp).context("bad join response")?;
        let mut st = self.st.lock().unwrap();
        ensure!(
            st.mls.join(&resp.welcome)? == gid,
            "welcome for another group"
        );
        st.store.set_meta("group", &gid)?;
        st.store.set_meta("admin", admin_id.as_bytes())?;
        st.save_mls()?;
        st.store
            .upsert_peer(admin_id.as_bytes(), ticket.as_bytes())?;
        self.add_peers(&st, &resp.peers)?;
        drop(st);
        // Fetch what was sent since our join (members re-send their profiles for us).
        self.sync()
    }

    /// Device ids of the space members.
    pub fn members(&self) -> Vec<[u8; 32]> {
        let st = self.st.lock().unwrap();
        st.group().map_or(vec![], |g| st.mls.members(&g))
    }

    /// Devices in a chat's MLS group.
    pub fn chat_members(&self, chat: &Chat) -> Result<Vec<[u8; 32]>> {
        let st = self.st.lock().unwrap();
        Ok(st.mls.members(&st.gid(chat)?))
    }

    pub fn admin(&self) -> Result<[u8; 32]> {
        self.st.lock().unwrap().admin()
    }

    /// Events of a chat ordered by (lamport, ts, id).
    pub fn history(&self, chat: &Chat) -> Result<Vec<Event>> {
        let rows = self.st.lock().unwrap().store.events(chat_key(chat))?;
        rows.iter().map(|b| Ok(postcard::from_bytes(b)?)).collect()
    }

    /// Events of every chat, with their chat, ordered by (lamport, ts, id).
    pub fn all_events(&self) -> Result<Vec<(Chat, Event)>> {
        let rows = self.st.lock().unwrap().store.all_events()?;
        rows.iter()
            .map(|(c, b)| Ok((chat_from(c), postcard::from_bytes(b)?)))
            .collect()
    }

    /// Partners of our 1:1 chats.
    pub fn dms(&self) -> Result<Vec<[u8; 32]>> {
        let mut v: Vec<_> = self.st.lock().unwrap().dms()?;
        v.sort_by_key(|(_, p)| *p);
        let mut p: Vec<_> = v.into_iter().map(|(_, p)| p).collect();
        p.dedup();
        Ok(p)
    }

    /// Latest profile of each member: (name, avatar, color, key package).
    pub fn profiles(&self) -> Result<HashMap<[u8; 32], Profile>> {
        Ok(self
            .history(&None)?
            .into_iter()
            .filter_map(|e| match e.body {
                EventBody::Profile {
                    name,
                    avatar,
                    color,
                    key_package,
                } => Some((e.author_device, (name, avatar, color, key_package))),
                _ => None,
            })
            .collect())
    }

    /// Sends our name and avatar to the space chat, with a fresh last-resort KeyPackage.
    pub fn set_profile(&self, name: &str, avatar: &str, color: u8) -> Result<Event> {
        let name = name.trim();
        ensure!(!name.is_empty(), "empty name");
        let key_package = self.st.lock().unwrap().mls.key_package(true)?;
        self.send_event(
            &None,
            EventBody::Profile {
                name: name.chars().take(64).collect(),
                avatar: avatar.trim().chars().take(2).collect(),
                color,
                key_package,
            },
        )
    }

    /// Re-sends our profile when its KeyPackage is a month old (they expire after ~3 months),
    /// or always with `force` (a newcomer cannot read what was sent before its join).
    fn refresh_profile(&self, force: bool) -> Result<()> {
        let me = self.device_id();
        let last = self
            .history(&None)?
            .into_iter()
            .rev()
            .find(|e| e.author_device == me && matches!(e.body, EventBody::Profile { .. }));
        if let Some(Event {
            timestamp_ms,
            body:
                EventBody::Profile {
                    name,
                    avatar,
                    color,
                    ..
                },
            ..
        }) = last
            && (force || now_ms()?.saturating_sub(timestamp_ms) > 30 * 86_400_000)
        {
            self.set_profile(&name, &avatar, color)?;
        }
        Ok(())
    }

    /// Opens a 1:1 chat with a member (no-op if there is one): a new 2-member MLS group from
    /// the KeyPackage in their profile; the Welcome goes out as an envelope of that group.
    pub fn open_dm(&self, peer: [u8; 32]) -> Result<()> {
        let me = self.device_id();
        ensure!(peer != me, "a chat with yourself");
        ensure!(self.members().contains(&peer), "not a member");
        let kp = self
            .profiles()?
            .remove(&peer)
            .context("this member has no profile yet (an older app version)")?
            .3;
        let (env, peers) = {
            let mut st = self.st.lock().unwrap();
            if st.gid(&Some(peer)).is_ok() {
                return Ok(());
            }
            let mut gid = [0u8; 32];
            getrandom::fill(&mut gid)?;
            st.mls.create_group(&gid)?;
            let (id, _commit, welcome) = st.mls.add_member(&gid, &kp)?;
            ensure!(id == peer, "KeyPackage is not the member's");
            st.add_dm(gid, peer)?;
            st.save_mls()?;
            let env = st.envelope(gid, welcome, 0)?;
            st.store_env(&env)?;
            (env, st.store.peers()?)
        };
        self.broadcast(peers, SyncMsg::Envs(vec![env]));
        Ok(())
    }

    /// Signs, encrypts (MLS), stores and sends a text event to every known peer.
    pub fn send_text(&self, chat: &Chat, text: &str, reply_to: Option<EventId>) -> Result<Event> {
        self.send_event(
            chat,
            EventBody::Text {
                text: text.into(),
                reply_to,
                mentions: vec![],
            },
        )
    }

    /// An event of any chat, with its chat.
    pub fn event(&self, id: &EventId) -> Result<(Chat, Event)> {
        let (c, b) = self
            .st
            .lock()
            .unwrap()
            .store
            .event(id)?
            .context("no such message")?;
        Ok((chat_from(&c), postcard::from_bytes(&b)?))
    }

    /// Replaces the text of an own message (or the caption of own media).
    pub fn edit(&self, target: EventId, text: &str) -> Result<Event> {
        let (chat, t) = self.event(&target)?;
        ensure!(
            t.author_device == self.device_id() && is_msg(&t.body),
            "only own messages can be edited"
        );
        self.send_event(
            &chat,
            EventBody::Edit {
                target,
                new_text: text.into(),
            },
        )
    }

    /// Deletes a message here; `for_all` also sends a Delete (own messages, or any one by the admin).
    pub fn delete(&self, target: EventId, for_all: bool) -> Result<()> {
        if for_all {
            let (chat, t) = self.event(&target)?;
            let me = self.device_id();
            ensure!(
                t.author_device == me || self.admin()? == me,
                "only the author or the admin can delete for everyone"
            );
            self.send_event(&chat, EventBody::Delete { target })?;
        }
        self.wipe(&[target])
    }

    pub fn pin(&self, target: EventId, pinned: bool) -> Result<Event> {
        let chat = self.event(&target)?.0;
        self.send_event(&chat, EventBody::Pin { target, pinned })
    }

    /// Sends a copy of a text or media message as a new own message to chat `to`.
    pub fn forward(&self, target: EventId, to: &Chat) -> Result<Event> {
        let body = match self.event(&target)?.1.body {
            EventBody::Text { text, .. } => EventBody::Text {
                text,
                reply_to: None,
                mentions: vec![],
            },
            EventBody::Media {
                kind,
                blob,
                caption,
                ..
            } => EventBody::Media {
                kind,
                blob,
                caption,
                reply_to: None,
            },
            _ => bail!("not a message"),
        };
        self.send_event(to, body)
    }

    /// Erases events with their decrypted media and blobs from this device.
    fn wipe(&self, ids: &[EventId]) -> Result<()> {
        let mut gone = vec![];
        {
            let st = self.st.lock().unwrap();
            for id in ids {
                if let Some(b) = st.store.take_event(id)? {
                    gone.push(postcard::from_bytes::<Event>(&b)?);
                }
            }
        }
        for ev in &gone {
            if let Some((_, b)) = media(ev) {
                let _ = std::fs::remove_file(self.media_file(b));
                let (blobs, tag) = (self.blobs.clone(), hex(&b.hash));
                tokio::spawn(async move { blobs.tags().delete(tag).await });
            }
        }
        Ok(())
    }

    /// Wipes the targets of valid Delete events still here (a Delete may arrive before its
    /// target); returns their ids.
    fn apply_deletes(&self) -> Result<HashSet<EventId>> {
        let hist: Vec<_> = self.all_events()?.into_iter().map(|(_, e)| e).collect();
        let admin = self.admin()?;
        let author: HashMap<_, _> = hist.iter().map(|e| (e.id, e.author_device)).collect();
        let ids: HashSet<_> = hist
            .iter()
            .filter_map(|e| match e.body {
                EventBody::Delete { target }
                    if author
                        .get(&target)
                        .is_some_and(|a| *a == e.author_device || e.author_device == admin) =>
                {
                    Some(target)
                }
                _ => None,
            })
            .collect();
        self.wipe(&ids.iter().copied().collect::<Vec<_>>())?;
        Ok(ids)
    }

    /// Delivery of own text events (§7.4): 0 ⏱ nobody confirmed yet, 1 ✓ some member device has it,
    /// 2 ✓✓ every other member device has it (by their Delivered receipts).
    // shortcut: "has it" = a receipt up to an event with lamport >= ours; use per-author clocks if concurrent sends mislead it.
    pub fn statuses(&self, chat: &Chat) -> Result<Vec<(EventId, u8)>> {
        let me = self.device_id();
        let others: Vec<_> = self
            .chat_members(chat)?
            .into_iter()
            .filter(|m| *m != me)
            .collect();
        let hist = self.history(chat)?;
        let lamport: HashMap<_, _> = hist.iter().map(|e| (e.id, e.lamport)).collect();
        let mut seen: HashMap<[u8; 32], u64> = HashMap::new();
        for e in &hist {
            if let EventBody::Receipt {
                up_to,
                kind: ReceiptKind::Delivered,
            } = &e.body
                && let Some(&l) = lamport.get(up_to)
            {
                let s = seen.entry(e.author_device).or_default();
                *s = (*s).max(l);
            }
        }
        Ok(hist
            .iter()
            .filter(|e| e.author_device == me && is_msg(&e.body))
            .map(|e| {
                let n = others
                    .iter()
                    .filter(|m| seen.get(*m).is_some_and(|&l| l >= e.lamport))
                    .count();
                let s = match n {
                    0 => 0,
                    n if n == others.len() => 2,
                    _ => 1,
                };
                (e.id, s)
            })
            .collect())
    }

    /// Store-and-forward retention (§7.3): drops envelopes and events older than `days`,
    /// and untags blobs no remaining event refers to (the store GC deletes them within an hour).
    // shortcut: time-based only; also drop once every member has receipted, if anchors run out of disk.
    pub async fn prune(&self, days: u64) -> Result<usize> {
        let cutoff = now_ms()?.saturating_sub(days * 86_400_000);
        let n = self.st.lock().unwrap().store.prune(cutoff as i64)?;
        let keep: HashSet<String> = self
            .all_events()?
            .iter()
            .filter_map(|(_, e)| media(e).map(|(_, b)| hex(&b.hash)))
            .collect();
        let tags: Vec<_> = self.blobs.tags().list().await?.collect::<Vec<_>>().await;
        for t in tags {
            let name = t?.name;
            if !keep.contains(String::from_utf8_lossy(name.as_ref()).as_ref()) {
                self.blobs.tags().delete(name).await?;
            }
        }
        Ok(n)
    }

    /// Encrypts `src`, stores the ciphertext as a blob and sends a Media event (§8.1).
    /// `meta` carries mime, name, preview, duration and dims; hash, size and key are filled here.
    pub async fn send_media(
        &self,
        chat: &Chat,
        kind: MediaKind,
        src: &Path,
        mut meta: BlobRef,
        caption: Option<String>,
    ) -> Result<Event> {
        let size = std::fs::metadata(src)?.len();
        ensure!(
            size <= self.max_blob,
            "file is over {} bytes",
            self.max_blob
        );
        let tmp = self.temp()?;
        let (s, t) = (src.to_owned(), tmp.clone());
        let key = tokio::task::spawn_blocking(move || mycel_media::encrypt(&s, &t)).await??;
        let added = self.blobs.add_path(&tmp).temp_tag().await;
        let _ = std::fs::remove_file(&tmp);
        let hash = added?.hash();
        self.blobs.tags().set(hex(hash.as_bytes()), hash).await?;
        meta.hash = *hash.as_bytes();
        meta.size = size;
        meta.key = key;
        // The sender sees its own media without decrypting it back.
        let out = self.media_file(&meta);
        std::fs::create_dir_all(out.parent().context("media dir")?)?;
        std::fs::copy(src, &out)?;
        self.send_event(
            chat,
            EventBody::Media {
                kind,
                blob: meta,
                caption,
                reply_to: None,
            },
        )
    }

    /// True once the whole blob is here.
    pub async fn has_blob(&self, blob: &BlobRef) -> bool {
        self.blobs
            .has(Hash::from_bytes(blob.hash))
            .await
            .unwrap_or(false)
    }

    /// Downloads a blob from its author, else from any other member that has it (e.g. an anchor),
    /// resuming what is already here. `progress` gets bytes present so far. Returns bytes read now.
    pub async fn fetch(
        &self,
        blob: &BlobRef,
        author: [u8; 32],
        mut progress: impl FnMut(u64),
    ) -> Result<u64> {
        ensure!(
            blob.size <= self.max_blob,
            "blob is over {} bytes",
            self.max_blob
        );
        let hash = Hash::from_bytes(blob.hash);
        // The tag also keeps a partial download from GC, so it resumes later.
        self.blobs.tags().set(hex(&blob.hash), hash).await?;
        if self.blobs.has(hash).await? {
            return Ok(0);
        }
        let mut peers: Vec<_> = self.st.lock().unwrap().store.peers()?;
        peers.retain(|p| peer_addr(p).is_some_and(|a| a.id != self.net.id()));
        peers.sort_by_key(|p| peer_addr(p).is_none_or(|a| *a.id.as_bytes() != author));
        let mut last = anyhow::anyhow!("no member is reachable");
        for p in peers {
            let Some(addr) = peer_addr(&p) else { continue };
            match self.fetch_from(addr, hash, &mut progress).await {
                Ok(n) => return Ok(n),
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    async fn fetch_from(
        &self,
        addr: EndpointAddr,
        hash: Hash,
        progress: &mut impl FnMut(u64),
    ) -> Result<u64> {
        let base = self.blobs.remote().local(hash).await?.local_bytes();
        let conn = self.net.connect(addr, BLOBS_ALPN).await?;
        let mut s = self.blobs.remote().fetch(conn, hash).stream();
        while let Some(item) = s.next().await {
            match item {
                GetProgressItem::Progress(n) => progress(base + n),
                GetProgressItem::Done(st) => {
                    ensure!(self.blobs.has(hash).await?, "peer has only part of it");
                    return Ok(st.payload_bytes_read);
                }
                GetProgressItem::Error(e) => bail!("{e}"),
            }
        }
        bail!("download stopped")
    }

    /// The decrypted media file, decrypting the blob on first use (the blob must be here).
    // shortcut: plaintext cache in `dir/media` for <img>/<video> and "open with"; decrypt on the fly via a custom protocol if at-rest secrecy of media matters.
    pub async fn media_path(&self, blob: &BlobRef) -> Result<PathBuf> {
        let out = self.media_file(blob);
        if out.exists() {
            return Ok(out);
        }
        std::fs::create_dir_all(out.parent().context("media dir")?)?;
        let tmp = self.temp()?;
        let hash = Hash::from_bytes(blob.hash);
        self.blobs.export(hash, &tmp).finish().await?;
        let part = out.with_extension("part");
        let (t, p, key) = (tmp.clone(), part.clone(), blob.key);
        let res = tokio::task::spawn_blocking(move || mycel_media::decrypt(&t, &p, &key)).await?;
        let _ = std::fs::remove_file(&tmp);
        res?;
        std::fs::rename(&part, &out)?;
        Ok(out)
    }

    /// `dir/media/<hash>.<ext>`, the extension from the name or the mime subtype; exists once decrypted.
    pub fn media_file(&self, b: &BlobRef) -> PathBuf {
        let ext = b
            .name
            .as_deref()
            .and_then(|n| Path::new(n).extension()?.to_str())
            .or_else(|| b.mime.split(['/', ';']).nth(1))
            .unwrap_or("bin");
        let ext: String = ext
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .take(8)
            .collect();
        self.dir
            .join("media")
            .join(format!("{}.{ext}", hex(&b.hash)))
    }

    fn temp(&self) -> Result<PathBuf> {
        let mut r = [0u8; 8];
        getrandom::fill(&mut r)?;
        let d = self.dir.join("tmp");
        std::fs::create_dir_all(&d)?;
        Ok(d.join(hex(&r)))
    }

    /// Downloads every blob of the history that is not here yet (anchors).
    pub fn fetch_missing(self: Arc<Self>) {
        if let Ok(h) = self.history(&None) {
            self.fetch_all(h);
        }
    }

    /// Anchors: download the blob of every Media event among `evs` that is not here yet.
    fn fetch_all(self: Arc<Self>, evs: Vec<Event>) {
        tokio::spawn(async move {
            for ev in &evs {
                if let Some((_, b)) = media(ev)
                    && let Err(e) = self.fetch(b, ev.author_device, |_| {}).await
                {
                    eprintln!("mycel: blob fetch failed: {e}");
                }
            }
        });
    }

    async fn serve_conns(self: Arc<Self>, mut conns: ConnInbox) {
        while let Some(conn) = conns.recv().await {
            if conn.alpn() == CALL_ALPN {
                if let Err(e) = self.add_call_conn(conn, None) {
                    eprintln!("mycel: call connection refused: {e}");
                }
                continue;
            }
            if !self.members().contains(conn.remote_id().as_bytes()) {
                continue;
            }
            let store = (*self.blobs).clone();
            tokio::spawn(iroh_blobs::provider::handle_connection(
                conn,
                store,
                EventSender::DEFAULT,
            ));
        }
    }

    pub async fn close(&self) {
        self.net.close().await;
        // Releases the blob database lock, so the same folder can be opened again.
        let _ = self.blobs.shutdown().await;
    }

    /// Peers we currently hold a connection to (the UI's connection indicator).
    pub fn peers_online(&self) -> usize {
        self.net.connected()
    }

    /// Signs, encrypts (MLS), stores and sends an event to every known peer; 1:1 chats too,
    /// so any member (an anchor) can keep and forward the ciphertext while the partner is away.
    fn send_event(&self, chat: &Chat, body: EventBody) -> Result<Event> {
        let (ev, env, peers) = {
            let mut st = self.st.lock().unwrap();
            let gid = st.gid(chat)?;
            let head = st.store.head(chat_key(chat))?;
            let ev = Event {
                id: [0; 32],
                author_device: [0; 32],
                parents: head
                    .iter()
                    .filter_map(|(id, _)| id.clone().try_into().ok())
                    .collect(),
                lamport: head.map_or(0, |(_, l)| l) + 1,
                timestamp_ms: now_ms()?,
                body,
                signature: vec![],
            };
            let ev = sign_event(self.net.secret_key(), ev);
            let epoch = st.mls.epoch(&gid);
            let ct = st.mls.encrypt(&gid, &postcard::to_allocvec(&ev)?)?;
            st.save_mls()?;
            let env = st.envelope(gid, ct, epoch)?;
            st.store_event(&ev, chat)?;
            st.store_env(&env)?;
            (ev, env, st.store.peers()?)
        };
        self.broadcast(peers, SyncMsg::Envs(vec![env]));
        Ok(ev)
    }

    /// One Delivered receipt per chat in a received batch, up to its newest text from someone else (§7.4).
    // shortcut: receipts are stored events (~members per message); prune or aggregate if history grows too big.
    fn send_receipt(&self, evs: &[(Chat, Event)]) {
        let me = self.device_id();
        let mut last: HashMap<Chat, &Event> = HashMap::new();
        for (c, e) in evs {
            if e.author_device != me && is_msg(&e.body) {
                let l = last.entry(*c).or_insert(e);
                if e.lamport > l.lamport {
                    *l = e;
                }
            }
        }
        for (c, e) in last {
            let body = EventBody::Receipt {
                up_to: e.id,
                kind: ReceiptKind::Delivered,
            };
            if let Err(e) = self.send_event(&c, body) {
                eprintln!("mycel: receipt failed: {e}");
            }
        }
    }

    fn my_addr(&self) -> Result<SignedAddr> {
        let ticket = EndpointTicket::new(self.net.addr()).to_string();
        Ok(sign_addr(
            self.net.secret_key(),
            ticket,
            self.own_relay.clone(),
            now_ms()?,
        ))
    }

    /// Remembers new relay URLs and adds them to the endpoint.
    fn learn_relays(&self, st: &State, urls: &[String]) -> Result<()> {
        let mut known = st.relays()?;
        let mut new = false;
        for u in urls {
            if known.contains(u)
                || Some(u) == self.own_relay.as_ref()
                || u.parse::<RelayUrl>().is_err()
            {
                continue;
            }
            known.push(u.clone());
            new = true;
            let (net, u) = (self.net.clone(), u.clone());
            tokio::spawn(async move { net.add_relay(&u).await });
        }
        if new {
            st.store
                .set_meta("relays", &postcard::to_allocvec(&known)?)?;
        }
        Ok(())
    }

    /// Stores verified, newer addresses of members and learns their relays (§6.2).
    fn add_peers(&self, st: &State, list: &[SignedAddr]) -> Result<()> {
        let members = st.mls.members(&st.group()?);
        let mut relays = vec![];
        for a in list {
            let Ok(t) = a.ticket.parse::<EndpointTicket>() else {
                continue;
            };
            let id = t.endpoint_addr().id;
            if id == self.net.id()
                || !members.contains(id.as_bytes())
                || !verify_addr(a, id.as_bytes())
            {
                continue;
            }
            let old = st.store.peer(id.as_bytes()).ok();
            if old
                .and_then(|b| postcard::from_bytes::<SignedAddr>(&b).ok())
                .is_some_and(|o| o.ts_ms >= a.ts_ms)
            {
                continue;
            }
            st.store
                .upsert_peer(id.as_bytes(), &postcard::to_allocvec(a)?)?;
            relays.extend(a.relay.clone());
        }
        self.learn_relays(st, &relays)
    }

    /// Our address plus every signed member address we know.
    fn peer_list(&self, st: &State) -> Result<Vec<SignedAddr>> {
        let mut list: Vec<SignedAddr> = st
            .store
            .peers()?
            .iter()
            .filter_map(|b| postcard::from_bytes(b).ok())
            .collect();
        list.push(self.my_addr()?);
        Ok(list)
    }

    /// Exchanges addresses with every known peer and asks for what we miss (offering what they miss).
    fn sync(&self) -> Result<()> {
        let st = self.st.lock().unwrap();
        if !st.in_space()? {
            return Ok(());
        }
        let peers = st.store.peers()?;
        self.broadcast(peers.clone(), SyncMsg::Peers(self.peer_list(&st)?));
        let ids = env_ids(&st.store)?;
        self.broadcast(peers, SyncMsg::Have { ids, reply: true });
        Ok(())
    }

    /// Send to each peer in the background, retrying a few times: sync runs only at start,
    /// so a message dropped by a flaky link (VPN, relay, network switch) would stay lost.
    fn broadcast(&self, peers: Vec<Vec<u8>>, msg: SyncMsg) {
        for p in peers {
            let Some(addr) = peer_addr(&p) else {
                continue;
            };
            if addr.id == self.net.id() {
                continue;
            }
            let (net, msg) = (self.net.clone(), msg.clone());
            tokio::spawn(async move {
                for wait in [1, 3, 9, 0] {
                    match net.send(addr.clone(), &msg).await {
                        Ok(()) => break,
                        Err(e) if wait == 0 => {
                            eprintln!("mycel: send to {} failed: {e}", addr.id.fmt_short())
                        }
                        Err(_) => tokio::time::sleep(std::time::Duration::from_secs(wait)).await,
                    }
                }
            });
        }
    }

    fn handle(&self, from: EndpointId, msg: SyncMsg) -> Result<Vec<(Chat, Event)>> {
        let mut st = self.st.lock().unwrap();
        let space = st.group()?;
        ensure!(
            st.mls.members(&space).contains(from.as_bytes()),
            "sender is not a member"
        );
        let me = self.device_id();
        if !st.store.has_peer(from.as_bytes())? {
            let t = EndpointTicket::new(EndpointAddr::new(from));
            st.store
                .upsert_peer(from.as_bytes(), t.to_string().as_bytes())?;
        }
        let mut out = vec![];
        match msg {
            SyncMsg::Envs(mut envs) => {
                // Commits (and 1:1 Welcomes, epoch 0) must apply before later-epoch messages.
                envs.sort_by_key(|e| e.epoch);
                for env in envs {
                    match st.receive(&env, me) {
                        Ok(evs) => out.extend(evs),
                        Err(e) => eprintln!("mycel: bad envelope: {e}"),
                    }
                }
            }
            SyncMsg::Have { ids, reply } => {
                let theirs: HashSet<_> = ids.into_iter().collect();
                let mut missing = vec![];
                for (id, data) in st.store.envelopes()? {
                    if !theirs.contains(id.as_slice()) {
                        missing.push(postcard::from_bytes(&data)?);
                    }
                }
                let to = vec![st.store.peer(from.as_bytes())?];
                if !missing.is_empty() {
                    self.broadcast(to.clone(), SyncMsg::Envs(missing));
                }
                if reply {
                    self.broadcast(to.clone(), SyncMsg::Peers(self.peer_list(&st)?));
                    let ids = env_ids(&st.store)?;
                    self.broadcast(to, SyncMsg::Have { ids, reply: false });
                }
            }
            SyncMsg::Peers(list) => self.add_peers(&st, &list)?,
        }
        Ok(out)
    }

    async fn serve_joins(self: Arc<Self>, mut joins: JoinInbox) {
        while let Some((from, req, reply)) = joins.recv().await {
            match self.accept_join(from, &req) {
                Ok(resp) => {
                    let _ = reply.send(resp);
                    if let Err(e) = self.refresh_profile(true) {
                        eprintln!("mycel: profile resend failed: {e}");
                    }
                }
                Err(e) => {
                    let _ = reply.send(vec![]);
                    eprintln!("mycel: join from {} refused: {e}", from.fmt_short());
                }
            }
        }
    }

    /// Admin side of §4.3: burns the secret, adds the KeyPackage, sends the commit to old members.
    fn accept_join(&self, from: EndpointId, req: &[u8]) -> Result<Vec<u8>> {
        let req: JoinReq = postcard::from_bytes(req)?;
        let ticket: EndpointTicket = req.addr.ticket.parse()?;
        ensure!(
            ticket.endpoint_addr().id == from && verify_addr(&req.addr, from.as_bytes()),
            "address is not the sender's"
        );
        let mut st = self.st.lock().unwrap();
        ensure!(st.admin()? == self.device_id(), "not the admin");
        if !st
            .store
            .take_meta(&format!("invite:{}", hex(&req.secret)))?
        {
            bail!("unknown or used invite");
        }
        let gid = st.group()?;
        let epoch = st.mls.epoch(&gid);
        let (id, commit, welcome) = st.mls.add_member(&gid, &req.key_package)?;
        ensure!(id == *from.as_bytes(), "KeyPackage is not the sender's");
        st.save_mls()?;
        let env = st.envelope(gid, commit, epoch)?;
        st.store_env(&env)?;
        let peers = st.store.peers()?;
        let list = self.peer_list(&st)?;
        st.store
            .upsert_peer(from.as_bytes(), &postcard::to_allocvec(&req.addr)?)?;
        if let Some(r) = &req.addr.relay {
            self.learn_relays(&st, std::slice::from_ref(r))?;
        }
        self.broadcast(peers.clone(), SyncMsg::Envs(vec![env]));
        self.broadcast(peers, SyncMsg::Peers(vec![req.addr]));
        Ok(postcard::to_allocvec(&JoinResp {
            welcome,
            peers: list,
        })?)
    }
}

fn env_ids(store: &Store) -> Result<Vec<[u8; 32]>> {
    Ok(store
        .envelopes()?
        .into_iter()
        .filter_map(|(id, _)| id.try_into().ok())
        .collect())
}

/// A stored peer is a SignedAddr, or a bare ticket string (admin from the invite, data before 2b).
fn peer_addr(p: &[u8]) -> Option<EndpointAddr> {
    let t: EndpointTicket = std::str::from_utf8(p)
        .ok()
        .and_then(|s| s.parse().ok())
        .or_else(|| {
            postcard::from_bytes::<SignedAddr>(p)
                .ok()?
                .ticket
                .parse()
                .ok()
        })?;
    Some(t.into())
}

/// A chat message shown to the user (has delivery ticks and receipts).
/// What edits and pins say over a history: the latest text of each edited message (its
/// author's edits only) and the pinned messages, oldest pin first.
pub fn edits_and_pins(hist: &[Event]) -> (HashMap<EventId, String>, Vec<EventId>) {
    let author: HashMap<_, _> = hist.iter().map(|e| (e.id, e.author_device)).collect();
    let (mut edits, mut pins) = (HashMap::new(), vec![]);
    for e in hist {
        match &e.body {
            EventBody::Edit { target, new_text }
                if author.get(target) == Some(&e.author_device) =>
            {
                edits.insert(*target, new_text.clone());
            }
            EventBody::Pin { target, pinned } if author.contains_key(target) => {
                pins.retain(|p| p != target);
                if *pinned {
                    pins.push(*target);
                }
            }
            _ => {}
        }
    }
    (edits, pins)
}

fn is_msg(b: &EventBody) -> bool {
    matches!(b, EventBody::Text { .. } | EventBody::Media { .. })
}

/// The media kind and blob of a Media event.
pub fn media(ev: &Event) -> Option<(MediaKind, &BlobRef)> {
    match &ev.body {
        EventBody::Media { kind, blob, .. } => Some((*kind, blob)),
        _ => None,
    }
}

fn now_ms() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64)
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|b| format!("{b:02x}")).collect()
}
