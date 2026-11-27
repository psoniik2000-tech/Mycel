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
    BlobRef, DeviceId, Envelope, Event, EventBody, EventId, Forwarded, JoinReq, JoinResp,
    MediaKind, ReceiptKind, SignedAddr, SyncMsg,
};
use mycel_store::Store;
use n0_future::StreamExt;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;

pub mod backup;
mod call;
pub use call::{AUDIO, Frame, LinkStats, VIDEO_DELTA, VIDEO_KEY};
#[cfg(not(target_os = "android"))]
pub use iroh_relay::server::Server as RelayServer;
pub use mycel_crypto::safety_code;
pub use mycel_net::RelayUrl;
pub use mycel_proto;

const LINK: &str = "mycel://join/";
const CONTACT_LINK: &str = "mycel://contact/";
const DEVICE_LINK: &str = "mycel://device/";
/// Join error prefixes (the UI shows a matching text).
pub const JOIN_BAD_LINK: &str = "not a mycel invite link";
pub const JOIN_UNREACHABLE: &str = "inviter unreachable";
pub const JOIN_USED: &str = "invite already used or unknown";
const JOIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(40);
/// How long a used invite answers its own device again (a joiner that lost the answer), ms.
const INVITE_RETRY: u64 = 24 * 3_600_000;
/// Periodic address exchange and catch-up with every known peer.
const RESYNC: std::time::Duration = std::time::Duration::from_secs(60);
/// The mailbox is pulled this often while an own message awaits its first receipt (receipts
/// come without a push), for at most `FAST_FOR` after the send.
const FAST_POLL: std::time::Duration = std::time::Duration::from_secs(5);
const FAST_FOR: std::time::Duration = std::time::Duration::from_secs(120);

/// Network options of a node.
#[derive(Debug, Default, Clone)]
pub struct Opts {
    /// iroh UDP port, 0 = any free one.
    pub port: u16,
    /// URL of the iroh-relay this node serves (anchor), advertised to the members.
    pub own_relay: Option<String>,
    /// Also use the public n0 relays and address lookup (off by default, last resort).
    pub n0: bool,
    /// More relays to use and advertise in invites (backup public ones the user added).
    pub extra_relays: Vec<String>,
    /// Download every media blob as soon as its event arrives (anchors, §8.4).
    pub keep_blobs: bool,
    /// Largest media file to send or download, bytes; 0 = 4 GiB (§8.1).
    pub max_blob: u64,
    /// HTTP mailbox (`mailbox/worker.js`) that keeps envelopes for members who are offline.
    pub mailbox: Option<String>,
}

/// A chat: `None` is the space chat, `Some(device)` the 1:1 chat with that member (§17).
pub type Chat = Option<[u8; 32]>;

/// A member profile: (name, avatar letter, color index, last-resort KeyPackage).
pub type Profile = (String, String, u8, Vec<u8>);

/// Latest Account event of each author: (timestamp, account, devices) (§4.4).
type Accounts = HashMap<[u8; 32], (u64, [u8; 32], Vec<[u8; 32]>)>;

/// What a linked device gets from the account root in its join answer: the space Welcome,
/// (group, partner, is a contact group, Welcome) of every 1:1 group, and the history.
type LinkPack = (
    Vec<u8>,
    Vec<([u8; 32], [u8; 32], bool, Vec<u8>)>,
    Vec<(Vec<u8>, Event)>,
);
/// Largest history a linked device gets at once (the join answer is at most 16 MiB).
const LINK_HISTORY: usize = 8 << 20;

fn chat_key(c: &Chat) -> &[u8] {
    c.as_ref().map_or(&[], |p| &p[..])
}

fn chat_from(k: &[u8]) -> Chat {
    k.try_into().ok()
}

// shortcut: a node is in exactly one space = one MLS group, plus one 2-member group per 1:1 chat
// (a space member's, or a contact's: a pair group with its own space id, §4.2).
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

    /// Pair groups made by a contact link (§4.2): their envelopes carry their own space id and
    /// go to the partner only. The partner is in `dms`.
    fn contacts(&self) -> Result<Vec<[u8; 32]>> {
        let r = self.store.meta("contacts")?;
        Ok(r.map(|b| postcard::from_bytes(&b))
            .transpose()?
            .unwrap_or_default())
    }

    fn add_contact(&self, gid: [u8; 32], peer: [u8; 32]) -> Result<()> {
        let mut c = self.contacts()?;
        c.push(gid);
        self.store
            .set_meta("contacts", &postcard::to_allocvec(&c)?)?;
        self.add_dm(gid, peer)
    }

    /// Who may get a group's envelopes: the partner of a contact group, the space members for
    /// the space and its 1:1 groups. A closure, so a sync looks the rest up once.
    fn audience(&self) -> Result<impl Fn(&[u8; 32], &[u8; 32]) -> bool + use<>> {
        let members: HashSet<_> = self.mls.members(&self.group()?).into_iter().collect();
        let contacts: HashSet<_> = self.contacts()?.into_iter().collect();
        // Members of each 1:1 group: the partner's devices and ours (§4.4).
        let groups: HashMap<_, _> = self
            .dms()?
            .into_iter()
            .map(|(g, _)| (g, self.mls.members(&g)))
            .collect();
        Ok(move |gid: &[u8; 32], dev: &[u8; 32]| {
            let inside = groups.get(gid).is_some_and(|m| m.contains(dev));
            inside || (!contacts.contains(gid) && members.contains(dev))
        })
    }

    /// A space member or a member of one of our 1:1 groups: may sync with us and fetch blobs.
    fn knows(&self, dev: &[u8; 32]) -> Result<bool> {
        Ok(self.mls.members(&self.group()?).contains(dev)
            || self
                .dms()?
                .iter()
                .any(|(g, _)| self.mls.members(g).contains(dev)))
    }

    fn accounts(&self) -> Result<Accounts> {
        let r = self.store.meta("accounts")?;
        Ok(r.map(|b| postcard::from_bytes(&b))
            .transpose()?
            .unwrap_or_default())
    }

    /// The account of someone else's device: its root when the root lists it and it names the
    /// root, else the device itself (an older app = an account of one device).
    fn account_of(&self, d: &[u8; 32]) -> Result<[u8; 32]> {
        let a = self.accounts()?;
        Ok(match a.get(d) {
            Some((_, acc, _)) if acc != d && a.get(acc).is_some_and(|(_, _, l)| l.contains(d)) => {
                *acc
            }
            _ => *d,
        })
    }

    fn my_account(&self, me: [u8; 32]) -> Result<[u8; 32]> {
        let a = self.store.meta("account")?;
        Ok(a.and_then(|a| a.try_into().ok()).unwrap_or(me))
    }

    /// Our account's devices as its root lists them (confirmed or not), us and the root.
    fn mine(&self, me: [u8; 32]) -> Result<HashSet<[u8; 32]>> {
        let acc = self.my_account(me)?;
        let mut s: HashSet<_> = self
            .accounts()?
            .remove(&acc)
            .map(|(_, _, l)| l)
            .unwrap_or_default()
            .into_iter()
            .collect();
        s.extend([me, acc]);
        Ok(s)
    }

    /// Re-keys 1:1 chats whose partner device turned out to be one device of an account.
    fn merge_chats(&self) -> Result<()> {
        let mut d = self.dms()?;
        let mut changed = false;
        for (_, p) in d.iter_mut() {
            let a = self.account_of(p)?;
            if a != *p {
                self.store.move_chat(p, &a)?;
                *p = a;
                changed = true;
            }
        }
        if changed {
            self.store.set_meta("dms", &postcard::to_allocvec(&d)?)?;
        }
        Ok(())
    }

    /// Stored peers that may get group `gid`'s envelopes.
    fn peers_of(&self, gid: &[u8; 32]) -> Result<Vec<Vec<u8>>> {
        let sees = self.audience()?;
        Ok(self
            .store
            .peers()?
            .into_iter()
            .filter(|p| peer_addr(p).is_some_and(|a| sees(gid, a.id.as_bytes())))
            .collect())
    }

    /// (id, group) of every stored envelope.
    fn env_groups(&self) -> Result<Vec<([u8; 32], [u8; 32])>> {
        let mut out = vec![];
        for (id, data) in self.store.envelopes()? {
            let e: Envelope = postcard::from_bytes(&data)?;
            out.extend(id.try_into().ok().map(|id| (id, e.group_id)));
        }
        Ok(out)
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
            space_id: if self.contacts()?.contains(&group_id) {
                group_id[..16].try_into()?
            } else {
                self.group()?[..16].try_into()?
            },
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
        let new = self.store.insert_event(
            &ev.id,
            chat_key(chat),
            ev.lamport,
            ev.timestamp_ms,
            &postcard::to_allocvec(ev)?,
        )?;
        if new && let EventBody::Account { account, devices } = &ev.body {
            let mut a = self.accounts()?;
            if a.get(&ev.author_device)
                .is_none_or(|o| o.0 <= ev.timestamp_ms)
            {
                a.insert(
                    ev.author_device,
                    (ev.timestamp_ms, *account, devices.clone()),
                );
                self.store
                    .set_meta("accounts", &postcard::to_allocvec(&a)?)?;
                self.merge_chats()?;
            }
        }
        Ok(new)
    }

    /// Decrypts and stores envelopes (commits and 1:1 Welcomes, epoch 0, first); bad ones are skipped.
    fn take(&mut self, mut envs: Vec<Envelope>, me: [u8; 32]) -> Vec<(Chat, Event)> {
        envs.sort_by_key(|e| e.epoch);
        let mut out = vec![];
        for env in envs {
            match self.receive(&env, me) {
                Ok(evs) => out.extend(evs),
                Err(e) => eprintln!("mycel: bad envelope: {e}"),
            }
        }
        out
    }

    /// Decrypts and stores one envelope; returns the new events (several when it is the
    /// Welcome of a 1:1 chat whose messages came first).
    fn receive(&mut self, env: &Envelope, me: [u8; 32]) -> Result<Vec<(Chat, Event)>> {
        // A contact group has its own space id; an unknown one may be a Welcome from our device.
        let own = self.contacts()?.contains(&env.group_id)
            || (self.chat_of(&env.group_id)?.is_none() && env.space_id == env.group_id[..16]);
        let space = if own { env.group_id } else { self.group()? };
        ensure!(env.space_id == space[..16], "foreign space");
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
        // shortcut: any member of a 1:1 group may commit (it adds its own devices, §4.4); two
        // commits at once fork the group, order them if that happens.
        let admin = if chat.is_none() {
            Some(self.admin()?)
        } else {
            None
        };
        Ok(
            match self
                .mls
                .process(&env.group_id, &env.ciphertext, admin.as_ref())
            {
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

    /// Joins a 1:1 group from its Welcome (fails for anyone but the invited members), then
    /// reads the messages of that group that are already here. Its members are the partner's
    /// devices and ours (§4.4); a contact's group comes only through one of our own devices.
    // shortcut: a space member could pass a group off as a 1:1 with someone else; require one
    // partner account once Account events are sure to arrive before the Welcome.
    fn join_dm(&mut self, env: &Envelope, me: [u8; 32]) -> Result<Vec<(Chat, Event)>> {
        let contact = env.space_id == env.group_id[..16];
        let gid = self.mls.join(&env.ciphertext)?;
        ensure!(gid == env.group_id, "welcome for another group");
        let m = self.mls.members(&gid);
        let mine = self.mine(me)?;
        ensure!(m.contains(&me), "not a member");
        let others: Vec<_> = m.iter().filter(|d| !mine.contains(*d)).collect();
        if contact {
            ensure!(
                m.iter().any(|d| *d != me && mine.contains(d)),
                "a contact group not from our own device"
            );
        } else {
            let space = self.mls.members(&self.group()?);
            ensure!(
                others.iter().all(|d| space.contains(*d)),
                "partner is not a space member"
            );
        }
        // A device of the partner; `merge_chats` re-keys the chat once its account is known.
        let mut peer = [0; 32];
        for d in &others {
            peer = peer.max(self.account_of(d)?);
        }
        ensure!(peer != [0; 32], "no partner");
        if contact {
            self.add_contact(env.group_id, peer)?;
        } else {
            self.add_dm(env.group_id, peer)?;
        }
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
    extra_relays: Vec<String>,
    /// Encrypted media blobs (ciphertext, by BLAKE3 hash); a tag per hash keeps it from GC.
    blobs: FsStore,
    dir: PathBuf,
    keep_blobs: bool,
    max_blob: u64,
    call: Mutex<Option<call::Call>>,
    frames: Mutex<Option<mpsc::Sender<call::Frame>>>,
    mailbox: Option<Mailbox>,
    /// Feeds envelopes pulled from the mailbox to the inbox loop.
    box_tx: mpsc::Sender<Vec<Envelope>>,
    /// This phone's FCM token: the mailbox wakes it when something new is posted.
    push_token: Mutex<Option<String>>,
    push_registered: std::sync::atomic::AtomicBool,
    /// Per chat: (lamport, send time) of the newest own message nobody has confirmed yet.
    unacked: Mutex<HashMap<Chat, (u64, Instant)>>,
    /// Where incoming typing signals go (`typing_events`).
    typing_tx: Mutex<Option<mpsc::Sender<Typing>>>,
    /// Per chat: when we last sent a typing signal and whether it was voice.
    typing_sent: Mutex<HashMap<Chat, (Instant, bool)>>,
}

/// A member is typing (or recording a voice message, `voice`) in a chat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Typing {
    pub chat: Chat,
    pub from: [u8; 32],
    pub voice: bool,
}

/// Store-and-forward when no member is online: every new envelope is also posted to an HTTP
/// mailbox (a Cloudflare Worker); members pull what is new at start and every minute
/// (every `FAST_POLL` while waiting for a receipt).
/// It sees only ciphertext; the topic is derived from the space id, which only members know.
// shortcut: envelopes only; media blobs still come from a member who is online.
struct Mailbox {
    http: reqwest::Client,
    url: String,
}

/// Envelopes per mailbox page (the worker returns at most this many).
const BOX_PAGE: usize = 200;

/// HTTPS client with the bundled web roots (no system store on Android); `timeout` for the whole call.
fn tls_client(timeout: std::time::Duration) -> Result<reqwest::Client> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(reqwest::Client::builder()
        .tls_backend_preconfigured(tls)
        .timeout(timeout)
        .build()?)
}

impl Mailbox {
    fn new(url: &str) -> Result<Self> {
        Ok(Self {
            http: tls_client(std::time::Duration::from_secs(30))?,
            url: url.trim_end_matches('/').to_string(),
        })
    }

    fn topic(space_id: &[u8]) -> String {
        Hash::new([b"mycel-mailbox".as_slice(), space_id].concat())
            .to_hex()
            .to_string()
    }

    /// A device's tag in the push list: opaque to the mailbox, other spaces' tags differ.
    fn device_tag(space_id: &[u8], device: &[u8; 32]) -> String {
        Hash::new([b"mycel-push".as_slice(), space_id, device].concat())
            .to_hex()
            .to_string()
    }
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
        relays.extend(opts.extra_relays.iter().cloned());
        let mailbox = opts.mailbox.as_deref().map(Mailbox::new).transpose()?;
        let (net, mut inbox, joins, conns) = Net::bind(key, opts.port, &relays, opts.n0).await?;
        let mut bopts = Options::new(&dir.join("blobs"));
        bopts.gc = Some(GcConfig {
            interval: std::time::Duration::from_secs(3600),
            add_protected: None,
        });
        let blobs = FsStore::load_with_opts(dir.join("blobs").join("blobs.db"), bopts).await?;
        let (box_tx, mut box_rx) = mpsc::channel::<Vec<Envelope>>(16);
        let node = Arc::new(Self {
            st: Mutex::new(st),
            net,
            own_relay: opts.own_relay.clone(),
            n0: opts.n0,
            extra_relays: opts.extra_relays.clone(),
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
            mailbox,
            box_tx,
            push_token: Mutex::new(None),
            push_registered: Default::default(),
            unacked: Default::default(),
            typing_tx: Default::default(),
            typing_sent: Default::default(),
        });
        let (tx, rx) = mpsc::channel(256);
        let n = node.clone();
        tokio::spawn(async move {
            loop {
                let before = n.group_size();
                let (from, res) = tokio::select! {
                    Some((from, msg)) = inbox.recv() => (Some(from), n.handle(from, msg)),
                    Some(envs) = box_rx.recv() => (None, n.take_envs(envs)),
                    else => break,
                };
                // Someone joined: they cannot read our older profile, so send it again.
                if n.group_size() > before
                    && let Err(e) = n.refresh_profile(true)
                {
                    eprintln!("mycel: profile resend failed: {e}");
                }
                match res {
                    Ok(mut evs) => {
                        n.note_receipts(&evs);
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
                    Err(e) => eprintln!("mycel: dropped message from {from:?}: {e}"),
                }
            }
        });
        // A peer's link dropped (relay hiccup, phone switched network): dial it again and catch up
        // in a moment instead of after the next periodic sync.
        let (mut lost, weak) = (node.net.on_lost(), Arc::downgrade(&node));
        tokio::spawn(async move {
            while let Some(peer) = lost.recv().await {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                let Some(n) = weak.upgrade() else { break };
                if n.net.is_closed() {
                    break;
                }
                if n.net.link(peer).0.is_none()
                    && let Err(e) = n.sync_with(Some(peer))
                {
                    eprintln!("mycel: reconnect failed: {e}");
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
        // Again once a relay is up (so peers learn our relay address), then every minute:
        // a sync that failed at start (peer offline, relay not up yet) must not wait for a restart.
        // shortcut: full Have list each time; Negentropy or "since" ids when histories grow.
        let weak = Arc::downgrade(&node);
        let wait_relay = opts.n0 || !relays.is_empty();
        tokio::spawn(async move {
            if let Some(n) = weak.upgrade() {
                if wait_relay {
                    n.net.online().await;
                    let _ = n.sync();
                }
                n.poll_box().await;
            }
            let mut synced = Instant::now();
            let mut home = weak.upgrade().and_then(|n| n.relay_now());
            loop {
                tokio::time::sleep(FAST_POLL).await;
                let Some(n) = weak.upgrade() else { break };
                if n.net.is_closed() {
                    break;
                }
                // Back on a relay (or another one: network switch, wake from sleep): catch up now,
                // not on the next minute tick, so the anchor hands over what came meanwhile.
                let now = n.relay_now();
                let back = now.is_some() && now != home;
                home = now;
                let due = back || synced.elapsed() >= RESYNC;
                if due {
                    synced = Instant::now();
                    if let Err(e) = n.sync() {
                        eprintln!("mycel: resync failed: {e}");
                    }
                    if let Err(e) = n.expire() {
                        eprintln!("mycel: expire failed: {e}");
                    }
                }
                if due || n.awaiting_receipt() {
                    n.poll_box().await;
                }
            }
        });
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

    /// Whether a relay is connected (waits up to 5 s): without one, peers in other networks cannot reach us.
    pub async fn relay_connected(&self) -> bool {
        self.net.online().await;
        self.net.addr().relay_urls().next().is_some()
    }

    /// Relay URLs this node uses (own, learned and added; not the n0 ones).
    pub fn relays(&self) -> Vec<String> {
        let mut r = self.st.lock().unwrap().relays().unwrap_or_default();
        r.extend(self.own_relay.clone());
        r.extend(self.extra_relays.iter().cloned());
        r
    }

    /// The home relay if it is connected right now (no waiting).
    fn relay_now(&self) -> Option<String> {
        self.net.relay_up().then(|| self.home_relay()).flatten()
    }

    /// The relay peers in other networks reach us through right now, if any.
    pub fn home_relay(&self) -> Option<String> {
        self.net.addr().relay_urls().next().map(|u| u.to_string())
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

    /// One-time contact link (§4.2): base32(one-time secret, our ticket, relay URLs). Anyone may
    /// make one; whoever accepts it gets a pair group with us and nobody else.
    pub async fn contact_invite(&self) -> Result<String> {
        let ticket = self.ticket().await;
        let relays = self.relays();
        let mut secret = [0u8; 16];
        getrandom::fill(&mut secret)?;
        let st = self.st.lock().unwrap();
        st.store
            .set_meta(&format!("contact:{}", hex(&secret)), &[])?;
        let bytes = postcard::to_allocvec(&(secret, ticket, relays))?;
        Ok(format!("{CONTACT_LINK}{}", BASE32_NOPAD.encode(&bytes)))
    }

    /// Partners added by contact links.
    pub fn contacts(&self) -> Result<Vec<[u8; 32]>> {
        let st = self.st.lock().unwrap();
        let dms: HashMap<_, _> = st.dms()?.into_iter().collect();
        let mut c: Vec<_> = st
            .contacts()?
            .iter()
            .filter_map(|g| dms.get(g).copied())
            .collect();
        c.sort();
        c.dedup();
        Ok(c)
    }

    /// Our account id: the first device's id (§4.4).
    pub fn account(&self) -> [u8; 32] {
        let me = self.device_id();
        self.st.lock().unwrap().my_account(me).unwrap_or(me)
    }

    /// The account a device belongs to (itself for an account of one device).
    pub fn account_of(&self, d: &[u8; 32]) -> [u8; 32] {
        let me = self.device_id();
        let st = self.st.lock().unwrap();
        if st.mine(me).is_ok_and(|m| m.contains(d)) {
            return st.my_account(me).unwrap_or(me);
        }
        st.account_of(d).unwrap_or(*d)
    }

    /// Devices of our account, the first (root) one first.
    pub fn devices(&self) -> Vec<[u8; 32]> {
        let me = self.device_id();
        let st = self.st.lock().unwrap();
        let acc = st.my_account(me).unwrap_or(me);
        let mut v = vec![acc];
        let listed = st.accounts().unwrap_or_default().remove(&acc);
        v.extend(
            listed
                .map_or(vec![], |l| l.2)
                .into_iter()
                .filter(|d| *d != acc),
        );
        if !v.contains(&me) {
            v.push(me);
        }
        v
    }

    /// Our devices' last-resort KeyPackages from their profiles, except ours (to add them to a
    /// new 1:1 group).
    fn sibling_kps(&self) -> Result<Vec<([u8; 32], Vec<u8>)>> {
        let me = self.device_id();
        let mut prof = self.profiles()?;
        Ok(self
            .devices()
            .into_iter()
            .filter(|d| *d != me)
            .filter_map(|d| Some((d, prof.remove(&d)?.3)))
            .collect())
    }

    /// Our Account event (§4.4): the root lists the devices, a linked device names the root;
    /// `None` for an account of one device (older apps cannot read it).
    fn account_body(&self) -> Option<EventBody> {
        let (me, account, devices) = (self.device_id(), self.account(), self.devices());
        if account != me {
            Some(EventBody::Account {
                account,
                devices: vec![],
            })
        } else {
            (devices.len() > 1).then_some(EventBody::Account { account, devices })
        }
    }

    /// Sends an event where profiles go: the space chat and every contact.
    fn send_everywhere(&self, body: EventBody) -> Result<Event> {
        for p in self.contacts()? {
            self.send_event(&Some(p), body.clone())?;
        }
        self.send_event(&None, body)
    }

    /// One-time link that adds a new device to our account (§4.4): base32(secret, our ticket,
    /// relay URLs). Only the account's first device makes one, and only as its space's admin.
    // shortcut: no linking in a space someone else runs; the admin would have to add the device.
    pub async fn device_invite(&self) -> Result<String> {
        let ticket = self.ticket().await;
        let relays = self.relays();
        let mut secret = [0u8; 16];
        getrandom::fill(&mut secret)?;
        let me = self.device_id();
        let st = self.st.lock().unwrap();
        ensure!(
            st.my_account(me)? == me,
            "only the first device of the account can link others"
        );
        ensure!(
            st.admin()? == me,
            "only the admin of the space can link a device"
        );
        st.store.set_meta(&format!("link:{}", hex(&secret)), &[])?;
        let bytes = postcard::to_allocvec(&(secret, ticket, relays))?;
        Ok(format!("{DEVICE_LINK}{}", BASE32_NOPAD.encode(&bytes)))
    }

    /// Removes a linked device from our account and from every group (root only).
    pub fn unlink_device(&self, dev: [u8; 32]) -> Result<()> {
        let me = self.device_id();
        ensure!(dev != me, "this is this device");
        let devices: Vec<_> = self.devices().into_iter().filter(|d| *d != dev).collect();
        let sends = {
            let mut st = self.st.lock().unwrap();
            ensure!(
                st.my_account(me)? == me,
                "only the first device of the account can unlink others"
            );
            let mut out = vec![];
            let groups: Vec<_> = st.dms()?.into_iter().map(|(g, _)| g).collect();
            for gid in [st.group()?].into_iter().chain(groups) {
                let epoch = st.mls.epoch(&gid);
                if let Some(c) = st.mls.remove_member(&gid, &dev)? {
                    let env = st.envelope(gid, c, epoch)?;
                    st.store_env(&env)?;
                    out.push((env, st.peers_of(&gid)?));
                }
            }
            st.save_mls()?;
            out
        };
        for (env, peers) in sends {
            self.post_box(&env, false, None);
            self.broadcast(peers, SyncMsg::Envs(vec![env]));
        }
        self.send_everywhere(EventBody::Account {
            account: me,
            devices,
        })?;
        Ok(())
    }

    /// Root side of a device link: adds the new device to the space and every 1:1 group, tells
    /// everyone it is ours, and answers with the Welcomes and the history (§4.4).
    fn accept_link(&self, from: EndpointId, req: &JoinReq) -> Result<Vec<u8>> {
        let dev = *from.as_bytes();
        let (space_w, groups, commits) = {
            let mut st = self.st.lock().unwrap();
            let space = st.group()?;
            let contacts = st.contacts()?;
            let mut welcomes = vec![];
            let mut commits = vec![];
            for (gid, partner) in [(space, [0; 32])].into_iter().chain(st.dms()?) {
                let epoch = st.mls.epoch(&gid);
                let (ids, commit, welcome) = st
                    .mls
                    .add_members(&gid, std::slice::from_ref(&req.key_package))?;
                ensure!(ids == [dev], "KeyPackage is not the sender's");
                let env = st.envelope(gid, commit, epoch)?;
                st.store_env(&env)?;
                commits.push((env, st.peers_of(&gid)?));
                welcomes.push((gid, partner, contacts.contains(&gid), welcome));
            }
            st.save_mls()?;
            st.store
                .upsert_peer(&dev, &postcard::to_allocvec(&req.addr)?)?;
            if let Some(r) = &req.addr.relay {
                self.learn_relays(&st, std::slice::from_ref(r))?;
            }
            let space_w = welcomes.remove(0).3;
            (space_w, welcomes, commits)
        };
        for (env, peers) in commits {
            self.post_box(&env, false, None);
            self.broadcast(peers, SyncMsg::Envs(vec![env]));
        }
        let mut devices = self.devices();
        if !devices.contains(&dev) {
            devices.push(dev);
        }
        self.send_everywhere(EventBody::Account {
            account: self.device_id(),
            devices,
        })?;
        // Newest history first up to the size limit, then back in order.
        let mut history = vec![];
        let mut size = 0;
        for (c, e) in self.all_events()?.into_iter().rev() {
            size += postcard::to_allocvec(&e)?.len();
            if size > LINK_HISTORY {
                break;
            }
            history.push((chat_key(&c).to_vec(), e));
        }
        history.reverse();
        let peers = {
            let st = self.st.lock().unwrap();
            let mut l: Vec<SignedAddr> = st
                .store
                .peers()?
                .iter()
                .filter_map(|b| postcard::from_bytes(b).ok())
                .collect();
            l.push(self.my_addr()?);
            l
        };
        let pack: LinkPack = (space_w, groups, history);
        Ok(postcard::to_allocvec(&JoinResp {
            welcome: postcard::to_allocvec(&pack)?,
            peers,
        })?)
    }

    /// New-device side of a link: joins the root's space and 1:1 groups, takes its history,
    /// copies its profile and names it as our account.
    async fn link_to(&self, link: &str) -> Result<()> {
        let parsed = || -> Result<_> {
            let b32 = link.strip_prefix(DEVICE_LINK).context("no prefix")?;
            let (secret, ticket, relays): ([u8; 16], String, Vec<String>) =
                postcard::from_bytes(&BASE32_NOPAD.decode(b32.as_bytes())?)?;
            let to: EndpointAddr = ticket.parse::<EndpointTicket>()?.into();
            Ok((secret, ticket, relays, to))
        };
        let (secret, ticket, relays, to) = parsed().context(JOIN_BAD_LINK)?;
        let root = *to.id.as_bytes();
        ensure!(root != self.device_id(), "this is your own link");
        {
            let st = self.st.lock().unwrap();
            ensure!(!st.in_space()?, "this device is already set up");
            self.learn_relays(&st, &relays)?;
        }
        // Last resort: one KeyPackage joins every group.
        let resp = self.ask_join(to, secret, true).await?;
        let (space_w, groups, history): LinkPack = postcard::from_bytes(&resp.welcome)?;
        {
            let mut st = self.st.lock().unwrap();
            let gid = st.mls.join(&space_w)?;
            st.store.set_meta("group", &gid)?;
            st.store.set_meta("admin", &root)?;
            st.store.set_meta("account", &root)?;
            for (g, partner, contact, w) in groups {
                ensure!(st.mls.join(&w)? == g, "welcome for another group");
                if contact {
                    st.add_contact(g, partner)?;
                } else {
                    st.add_dm(g, partner)?;
                }
            }
            st.save_mls()?;
            for (c, e) in history {
                if verify_event(&e) {
                    st.store_event(&e, &chat_from(&c))?;
                }
            }
            st.store.upsert_peer(&root, ticket.as_bytes())?;
            self.add_peers(&st, &resp.peers)?;
        }
        self.push_registered
            .store(false, std::sync::atomic::Ordering::Relaxed);
        match self.profiles()?.remove(&root) {
            Some((name, avatar, color, _)) => {
                self.set_profile(&name, &avatar, color).map(|_| ())?
            }
            None => {
                if let Some(b) = self.account_body() {
                    self.send_everywhere(b)?;
                }
            }
        }
        self.sync()
    }

    /// Proves an invite secret to `to` with a KeyPackage and our address; returns its answer.
    async fn ask_join(
        &self,
        to: EndpointAddr,
        secret: [u8; 16],
        last_resort: bool,
    ) -> Result<JoinResp> {
        self.wait_relay().await;
        let req = JoinReq {
            secret,
            key_package: self.st.lock().unwrap().mls.key_package(last_resort)?,
            addr: self.my_addr()?,
        };
        let resp = tokio::time::timeout(
            JOIN_TIMEOUT,
            self.net.join(to, &postcard::to_allocvec(&req)?),
        )
        .await
        .map_err(anyhow::Error::from)
        .and_then(|r| r)
        .context(JOIN_UNREACHABLE)?;
        // The inviter answers an empty message when it refuses (unknown or used secret).
        ensure!(!resp.is_empty(), JOIN_USED);
        postcard::from_bytes(&resp).context("bad join response")
    }

    /// Accepts a contact link: a pair group with its author. A node without a space gets its own
    /// first (the app treats "in a space" as "set up").
    async fn add_contact(&self, link: &str) -> Result<()> {
        let parsed = || -> Result<_> {
            let b32 = link.strip_prefix(CONTACT_LINK).context("no prefix")?;
            let (secret, ticket, relays): ([u8; 16], String, Vec<String>) =
                postcard::from_bytes(&BASE32_NOPAD.decode(b32.as_bytes())?)?;
            let to: EndpointAddr = ticket.parse::<EndpointTicket>()?.into();
            Ok((secret, ticket, relays, to))
        };
        let (secret, ticket, relays, to) = parsed().context(JOIN_BAD_LINK)?;
        let (me, peer) = (self.device_id(), *to.id.as_bytes());
        ensure!(peer != me, "this is your own link");
        if !self.st.lock().unwrap().in_space()? {
            self.create_space()?;
        }
        self.learn_relays(&self.st.lock().unwrap(), &relays)?;
        let resp = self.ask_join(to, secret, false).await?;
        let sib = self.sibling_kps()?;
        let sends = {
            let mut st = self.st.lock().unwrap();
            let gid: [u8; 32] = st
                .mls
                .join(&resp.welcome)?
                .try_into()
                .ok()
                .context("bad group id")?;
            let m = st.mls.members(&gid);
            // The author's own devices may be in it too (§4.4).
            ensure!(
                m.contains(&me) && m.contains(&peer),
                "not a pair group with the link's author"
            );
            let account = st.account_of(&peer)?;
            st.add_contact(gid, account)?;
            st.store.upsert_peer(&peer, ticket.as_bytes())?;
            self.add_peers(&st, &resp.peers)?;
            // Our other devices join too; the Welcome also goes to our space's mailbox topic,
            // which they pull.
            let mut sends = vec![];
            if !sib.is_empty() {
                let epoch = st.mls.epoch(&gid);
                let kps: Vec<_> = sib.iter().map(|s| s.1.clone()).collect();
                let (_, commit, welcome) = st.mls.add_members(&gid, &kps)?;
                let c = st.envelope(gid, commit, epoch)?;
                // Epoch 0: members already in skip it, the new ones read it first.
                let w = st.envelope(gid, welcome, 0)?;
                st.store_env(&c)?;
                st.store_env(&w)?;
                sends = vec![(c, st.peers_of(&gid)?), (w, st.peers_of(&gid)?)];
            }
            st.save_mls()?;
            (sends, st.group()?)
        };
        let (sends, space) = sends;
        for (env, peers) in sends {
            self.post_box(&env, false, None);
            self.post_box_at(&env, &space[..16], false, None);
            self.broadcast(peers, SyncMsg::Envs(vec![env]));
        }
        self.push_registered
            .store(false, std::sync::atomic::Ordering::Relaxed);
        self.refresh_profile(true)?;
        self.sync()
    }

    /// Joins a space by invite link: proves the secret, sends a KeyPackage, applies the Welcome;
    /// a contact link adds a contact instead.
    /// Errors start with [`JOIN_BAD_LINK`], [`JOIN_UNREACHABLE`] or [`JOIN_USED`] when they are those.
    pub async fn join(&self, link: &str) -> Result<()> {
        // Messengers and notes wrap long links: drop every whitespace, not only the ends.
        let link: String = link.split_whitespace().collect();
        if link.starts_with(CONTACT_LINK) {
            return self.add_contact(&link).await;
        }
        if link.starts_with(DEVICE_LINK) {
            return self.link_to(&link).await;
        }
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
        let admin_id = admin.id;
        let resp = self.ask_join(admin, secret, false).await?;
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

    /// Members of the space and of every 1:1 group, counted with repeats (grows when someone
    /// joins one of them).
    fn group_size(&self) -> usize {
        let st = self.st.lock().unwrap();
        let groups = st
            .group()
            .into_iter()
            .chain(st.dms().unwrap_or_default().into_iter().map(|(g, _)| g));
        groups.map(|g| st.mls.members(&g).len()).sum()
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
    /// Expired disappearing messages are left out (`expire` erases them).
    pub fn history(&self, chat: &Chat) -> Result<Vec<Event>> {
        let rows = self.st.lock().unwrap().store.events(chat_key(chat))?;
        let now = now_ms()?;
        let mut out = vec![];
        for b in rows {
            let e: Event = postcard::from_bytes(&b)?;
            if e.expires_ms().is_none_or(|t| t > now) {
                out.push(e);
            }
        }
        Ok(out)
    }

    /// The chat's disappearing-message timer in seconds (0 = off): its latest setting.
    // shortcut: the setting is an event, so `prune` past retention turns the timer off; keep it in chat state if that bites.
    pub fn timer(&self, chat: &Chat) -> Result<u32> {
        Ok(self
            .history(chat)?
            .iter()
            .rev()
            .find_map(|e| match e.body {
                EventBody::ChatMeta {
                    disappearing: Some(d),
                    ..
                } => Some(d.as_secs() as u32),
                _ => None,
            })
            .unwrap_or(0))
    }

    /// Sets the chat's timer for everyone in it (0 = off); later messages carry it.
    pub fn set_timer(&self, chat: &Chat, secs: u32) -> Result<Event> {
        self.send_event(
            chat,
            EventBody::ChatMeta {
                name: None,
                avatar: None,
                disappearing: Some(std::time::Duration::from_secs(secs.into())),
            },
        )
    }

    /// Erases expired disappearing messages from this device; returns how many.
    pub fn expire(&self) -> Result<usize> {
        let now = now_ms()?;
        let ids: Vec<_> = self
            .all_events()?
            .iter()
            .filter(|(_, e)| e.expires_ms().is_some_and(|t| t <= now))
            .map(|(_, e)| e.id)
            .collect();
        self.wipe(&ids)?;
        Ok(ids.len())
    }

    /// Sends a message with the chat's timer, marked as forwarded from `fwd`.
    fn send_msg(&self, chat: &Chat, body: EventBody, fwd: Option<Forwarded>) -> Result<Event> {
        let ttl_secs = self.timer(chat)?;
        let body = if ttl_secs == 0 && fwd.is_none() {
            body
        } else {
            EventBody::Extra {
                body: Box::new(body),
                ttl_secs,
                forwarded: fwd,
            }
        };
        self.send_event(chat, body)
    }

    /// Events of every chat, with their chat, ordered by (lamport, ts, id).
    pub fn all_events(&self) -> Result<Vec<(Chat, Event)>> {
        let rows = self.st.lock().unwrap().store.all_events()?;
        rows.iter()
            .map(|(c, b)| Ok((chat_from(c), postcard::from_bytes(b)?)))
            .collect()
    }

    /// Partners of our 1:1 chats; also chats only in the history (restored from a backup,
    /// read-only until the pair group is made again).
    pub fn dms(&self) -> Result<Vec<[u8; 32]>> {
        let st = self.st.lock().unwrap();
        let mut p: Vec<_> = st.dms()?.into_iter().map(|(_, p)| p).collect();
        p.extend(st.store.chats()?.iter().filter_map(|c| chat_from(c)));
        p.sort();
        p.dedup();
        Ok(p)
    }

    /// Newest Profile event of each author, from every chat (contacts send theirs in the pair chat).
    fn profile_events(&self) -> Result<HashMap<[u8; 32], Event>> {
        let mut out: HashMap<[u8; 32], Event> = HashMap::new();
        for (_, e) in self.all_events()? {
            if matches!(e.body, EventBody::Profile { .. })
                && out
                    .get(&e.author_device)
                    .is_none_or(|o| o.timestamp_ms <= e.timestamp_ms)
            {
                out.insert(e.author_device, e);
            }
        }
        Ok(out)
    }

    /// Latest profile of each member and contact: (name, avatar, color, key package). A device
    /// of an account shows the account's name and avatar (§4.4), with its own KeyPackage.
    pub fn profiles(&self) -> Result<HashMap<[u8; 32], Profile>> {
        let raw: HashMap<_, _> = self
            .profile_events()?
            .into_iter()
            .filter_map(|(a, e)| match e.body {
                EventBody::Profile {
                    name,
                    avatar,
                    color,
                    key_package,
                } => Some((a, (name, avatar, color, key_package))),
                _ => None,
            })
            .collect();
        Ok(raw
            .iter()
            .map(|(d, p)| {
                let (n, a, c, _) = raw.get(&self.account_of(d)).unwrap_or(p);
                (*d, (n.clone(), a.clone(), *c, p.3.clone()))
            })
            .collect())
    }

    /// Sends our name and avatar to the space chat and every contact, with a fresh last-resort
    /// KeyPackage; then our Account event, if we have more than one device.
    pub fn set_profile(&self, name: &str, avatar: &str, color: u8) -> Result<Event> {
        let name = name.trim();
        ensure!(!name.is_empty(), "empty name");
        let key_package = self.st.lock().unwrap().mls.key_package(true)?;
        let body = EventBody::Profile {
            name: name.chars().take(64).collect(),
            avatar: avatar.trim().chars().take(2).collect(),
            color,
            key_package,
        };
        let ev = self.send_everywhere(body)?;
        if let Some(b) = self.account_body() {
            self.send_everywhere(b)?;
        }
        Ok(ev)
    }

    /// Re-sends our profile when its KeyPackage is a month old (they expire after ~3 months),
    /// or always with `force` (a newcomer cannot read what was sent before its join).
    fn refresh_profile(&self, force: bool) -> Result<()> {
        let me = self.device_id();
        let last = self.profile_events()?.remove(&me);
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
    /// `peer` is an account: all its devices and our other ones go in (§4.4).
    pub fn open_dm(&self, peer: [u8; 32]) -> Result<()> {
        let me = self.device_id();
        if self.st.lock().unwrap().gid(&Some(peer)).is_ok() {
            return Ok(());
        }
        ensure!(
            self.account_of(&peer) != self.account(),
            "a chat with yourself"
        );
        ensure!(self.members().contains(&peer), "not a member");
        let mut prof = self.profiles()?;
        let kp = prof
            .remove(&peer)
            .context("this member has no profile yet (an older app version)")?
            .3;
        let mut add = vec![(peer, kp)];
        for d in self.members() {
            if d != peer && d != me && self.account_of(&d) == peer {
                add.extend(prof.remove(&d).map(|p| (d, p.3)));
            }
        }
        add.extend(self.sibling_kps()?);
        let (env, peers) = {
            let mut st = self.st.lock().unwrap();
            if st.gid(&Some(peer)).is_ok() {
                return Ok(());
            }
            let mut gid = [0u8; 32];
            getrandom::fill(&mut gid)?;
            st.mls.create_group(&gid)?;
            let kps: Vec<_> = add.iter().map(|a| a.1.clone()).collect();
            let (ids, _commit, welcome) = st.mls.add_members(&gid, &kps)?;
            ensure!(
                ids.iter().eq(add.iter().map(|a| &a.0)),
                "KeyPackage is not the member's"
            );
            st.add_dm(gid, peer)?;
            st.save_mls()?;
            let env = st.envelope(gid, welcome, 0)?;
            st.store_env(&env)?;
            (env, st.peers_of(&gid)?)
        };
        self.post_box(&env, false, None);
        self.broadcast(peers, SyncMsg::Envs(vec![env]));
        Ok(())
    }

    /// Signs, encrypts (MLS), stores and sends a text event to every known peer.
    pub fn send_text(&self, chat: &Chat, text: &str, reply_to: Option<EventId>) -> Result<Event> {
        self.send_msg(
            chat,
            EventBody::Text {
                text: text.into(),
                reply_to,
                mentions: vec![],
            },
            None,
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

    /// Toggles own reaction `emoji` on a message: the same one again takes it back.
    pub fn react(&self, target: EventId, emoji: &str) -> Result<Event> {
        let chat = self.event(&target)?.0;
        let me = self.device_id();
        let remove = reactions(&self.history(&chat)?)
            .get(&target)
            .is_some_and(|r| r.iter().any(|(e, who)| e == emoji && who.contains(&me)));
        let emoji = emoji.into();
        self.send_event(
            &chat,
            EventBody::Reaction {
                target,
                emoji,
                remove,
            },
        )
    }

    /// Sends copies of text or media messages as new own messages to chat `to`. Albums among them
    /// get new ids, so a copy never joins the original. Each names its original author.
    pub fn forward(&self, targets: &[EventId], to: &Chat) -> Result<()> {
        let mut albums = HashMap::new();
        let names = self.profiles()?;
        for target in targets {
            let ev = self.event(target)?.1;
            let fwd = ev.body.forwarded().cloned().unwrap_or_else(|| Forwarded {
                name: names
                    .get(&ev.author_device)
                    .map(|p| p.0.clone())
                    .unwrap_or_default(),
                device: ev.author_device,
            });
            let body = match ev.body.msg().clone() {
                EventBody::Text { text, .. } => EventBody::Text {
                    text,
                    reply_to: None,
                    mentions: vec![],
                },
                EventBody::Media {
                    kind,
                    mut blob,
                    caption,
                    ..
                } => {
                    if let Some(old) = blob.album().map(str::to_owned) {
                        let id = match albums.entry(old) {
                            Entry::Occupied(e) => e.into_mut(),
                            Entry::Vacant(e) => e.insert(new_album()?),
                        };
                        blob.set_album(id);
                    }
                    EventBody::Media {
                        kind,
                        blob,
                        caption,
                        reply_to: None,
                    }
                }
                _ => bail!("not a message"),
            };
            self.send_msg(to, body, Some(fwd))?;
        }
        Ok(())
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
        if !gone.iter().any(|e| media(e).is_some()) {
            return Ok(());
        }
        // A forwarded copy shares the blob: keep it while another message still shows it.
        let kept: HashSet<_> = self
            .all_events()?
            .iter()
            .filter_map(|(_, e)| media(e).map(|(_, b)| b.hash))
            .collect();
        for ev in &gone {
            if let Some((_, b)) = media(ev)
                && !kept.contains(&b.hash)
            {
                let _ = std::fs::remove_file(self.media_file(b));
                let (blobs, tag) = (self.blobs.clone(), hex(&b.hash));
                tokio::spawn(async move { blobs.tags().delete(tag).await });
            }
        }
        Ok(())
    }

    /// Wipes the targets of valid Delete events still here (a Delete may arrive before its
    /// target) and expired disappearing messages; returns their ids.
    fn apply_deletes(&self) -> Result<HashSet<EventId>> {
        let hist: Vec<_> = self.all_events()?.into_iter().map(|(_, e)| e).collect();
        let now = now_ms()?;
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
                _ if e.expires_ms().is_some_and(|t| t <= now) => Some(e.id),
                _ => None,
            })
            .collect();
        self.wipe(&ids.iter().copied().collect::<Vec<_>>())?;
        Ok(ids)
    }

    /// Status of own messages (§7.4): 0 ⏱ nobody confirmed yet, 1 ✓ some other member device has it
    /// (Delivered receipt), 2 ✓✓ some other member device has read it (Read receipt). In a group chat ✓✓
    /// means "read by at least one", like 1:1.
    // shortcut: "has it" = a receipt up to an event with lamport >= ours; use per-author clocks if concurrent sends mislead it.
    pub fn statuses(&self, chat: &Chat) -> Result<Vec<(EventId, u8)>> {
        let mine = self.mine();
        let hist = self.history(chat)?;
        let lamport: HashMap<_, _> = hist.iter().map(|e| (e.id, e.lamport)).collect();
        // Highest lamport delivered / read by anyone else.
        let (mut delivered, mut read) = (None, None);
        for e in &hist {
            if let EventBody::Receipt { up_to, kind } = &e.body
                && !mine.contains(&e.author_device)
                && let Some(&l) = lamport.get(up_to)
            {
                let s = if *kind == ReceiptKind::Read {
                    &mut read
                } else {
                    &mut delivered
                };
                *s = (*s).max(Some(l));
            }
        }
        Ok(hist
            .iter()
            .filter(|e| mine.contains(&e.author_device) && is_msg(&e.body))
            .map(|e| {
                let has = |s: Option<u64>| s.is_some_and(|l| l >= e.lamport);
                let s = if has(read) {
                    2
                } else if has(delivered) {
                    1
                } else {
                    0
                };
                (e.id, s)
            })
            .collect())
    }

    /// Others' messages and calls in the chat newer than my latest Read receipt.
    // shortcut: "newer" by lamport, as in `statuses`: in a group, one written offline that arrives after
    // I read later ones counts as read; keep per-author marks if that misleads.
    pub fn unread(&self, chat: &Chat) -> Result<Vec<EventId>> {
        let mine = self.mine();
        let hist = self.history(chat)?;
        let lamport: HashMap<_, _> = hist.iter().map(|e| (e.id, e.lamport)).collect();
        let read = hist
            .iter()
            .filter_map(|e| match &e.body {
                // A receipt whose target was deleted covers everything before itself.
                EventBody::Receipt {
                    up_to,
                    kind: ReceiptKind::Read,
                } if mine.contains(&e.author_device) => {
                    Some(lamport.get(up_to).copied().unwrap_or(e.lamport))
                }
                _ => None,
            })
            .max();
        Ok(hist
            .iter()
            .filter(|e| !mine.contains(&e.author_device) && is_incoming(&e.body))
            .filter(|e| read.is_none_or(|l| e.lamport > l))
            .map(|e| e.id)
            .collect())
    }

    /// Sends one Read receipt up to the chat's newest message or call from someone else, unless one already
    /// covers it (§7.4). The UI calls it while the chat is open and the window visible.
    pub fn mark_read(&self, chat: &Chat) -> Result<()> {
        let mine = self.mine();
        let hist = self.history(chat)?;
        let Some(last) = hist
            .iter()
            .filter(|e| !mine.contains(&e.author_device) && is_incoming(&e.body))
            .max_by_key(|e| e.lamport)
        else {
            return Ok(());
        };
        let lamport: HashMap<_, _> = hist.iter().map(|e| (e.id, e.lamport)).collect();
        // A Read receipt from any of our devices counts (§4.4).
        let done = hist.iter().any(|e| {
            mine.contains(&e.author_device)
                && matches!(&e.body, EventBody::Receipt { up_to, kind: ReceiptKind::Read }
                    if lamport.get(up_to).is_some_and(|&l| l >= last.lamport))
        });
        if !done {
            self.send_event(
                chat,
                EventBody::Receipt {
                    up_to: last.id,
                    kind: ReceiptKind::Read,
                },
            )?;
        }
        Ok(())
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

    /// Size of a file that can be sent.
    pub fn check_size(&self, src: &Path) -> Result<u64> {
        let size = std::fs::metadata(src)?.len();
        ensure!(
            size <= self.max_blob,
            "file is over {} bytes",
            self.max_blob
        );
        Ok(size)
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
        let size = self.check_size(src)?;
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
        self.send_msg(
            chat,
            EventBody::Media {
                kind,
                blob: meta,
                caption,
                reply_to: None,
            },
            None,
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
        let peers: Vec<_> = self.st.lock().unwrap().store.peers()?;
        // Dial the author, the anchors (they keep every blob) and everyone else at once and
        // download from whoever answers first: one after another, each offline member cost a
        // full dial timeout. Anchors and the author are dialed first.
        let mut addrs: Vec<(bool, EndpointAddr)> = peers
            .iter()
            .filter_map(|p| {
                let a = peer_addr(p)?;
                let anchor = postcard::from_bytes::<SignedAddr>(p).is_ok_and(|s| s.relay.is_some());
                (a.id != self.net.id()).then(|| (!(anchor || *a.id.as_bytes() == author), a))
            })
            .collect();
        addrs.sort_by_key(|(later, _)| *later);
        let mut dials: n0_future::FuturesUnordered<_> = addrs
            .into_iter()
            .map(|(_, a)| {
                let net = self.net.clone();
                async move {
                    tokio::time::timeout(
                        std::time::Duration::from_secs(8),
                        net.connect(a, BLOBS_ALPN),
                    )
                    .await
                    .context("peer did not answer")?
                }
            })
            .collect();
        let mut last = anyhow::anyhow!("no member is reachable");
        while let Some(conn) = dials.next().await {
            match conn {
                Ok(conn) => match self.fetch_from(conn, hash, &mut progress).await {
                    Ok(n) => return Ok(n),
                    Err(e) => last = e,
                },
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    async fn fetch_from(
        &self,
        conn: mycel_net::Connection,
        hash: Hash,
        progress: &mut impl FnMut(u64),
    ) -> Result<u64> {
        let base = self.blobs.remote().local(hash).await?.local_bytes();
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
            let known = self.st.lock().unwrap().knows(conn.remote_id().as_bytes());
            if !known.unwrap_or(false) {
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

    /// Re-encrypts the local database under a new password.
    pub fn change_password(&self, password: &str) -> Result<()> {
        self.st.lock().unwrap().store.rekey(&self.dir, password)
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

    /// Link to device `dev`: `Some(true)` relay, `Some(false)` direct, `None` none; and last-seen unix ms.
    pub fn link(&self, dev: &[u8; 32]) -> (Option<bool>, Option<u64>) {
        EndpointId::from_bytes(dev).map_or((None, None), |id| self.net.link(id))
    }

    /// Signs, encrypts (MLS), stores and sends an event to every known peer; 1:1 chats too,
    /// so any member (an anchor) can keep and forward the ciphertext while the partner is away.
    fn send_event(&self, chat: &Chat, body: EventBody) -> Result<Event> {
        // Messages and incoming calls wake the other phones; receipts and edits wait for the next pull.
        let push = is_msg(&body)
            || matches!(
                body,
                EventBody::Call {
                    action: mycel_proto::CallAction::Offer { .. },
                    ..
                }
            );
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
            (ev, env, st.peers_of(&gid)?)
        };
        if is_msg(&ev.body) {
            self.unacked
                .lock()
                .unwrap()
                .insert(*chat, (ev.lamport, Instant::now()));
        }
        self.post_box(&env, push, *chat);
        self.broadcast(peers, SyncMsg::Envs(vec![env]));
        Ok(ev)
    }

    /// One Delivered receipt per chat in a received batch, up to its newest text from someone else (§7.4).
    // shortcut: receipts are stored events (~members per message); prune or aggregate if history grows too big.
    fn send_receipt(&self, evs: &[(Chat, Event)]) {
        let mine = self.mine();
        let mut last: HashMap<Chat, &Event> = HashMap::new();
        for (c, e) in evs {
            if !mine.contains(&e.author_device) && is_msg(&e.body) {
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

    /// Forgets the awaited messages that a received receipt covers.
    fn note_receipts(&self, evs: &[(Chat, Event)]) {
        let mine = self.mine();
        for (c, e) in evs {
            if let EventBody::Receipt { up_to, .. } = e.body
                && !mine.contains(&e.author_device)
                && let Ok((_, seen)) = self.event(&up_to)
            {
                let mut u = self.unacked.lock().unwrap();
                if u.get(c).is_some_and(|&(l, _)| l <= seen.lamport) {
                    u.remove(c);
                }
            }
        }
    }

    /// A message sent within `FAST_FOR` still has no receipt.
    fn awaiting_receipt(&self) -> bool {
        self.unacked
            .lock()
            .unwrap()
            .values()
            .any(|(_, at)| at.elapsed() < FAST_FOR)
    }

    /// Our account's devices (us at least).
    fn mine(&self) -> HashSet<[u8; 32]> {
        let me = self.device_id();
        let m = self.st.lock().unwrap().mine(me);
        m.unwrap_or_else(|_| [me].into())
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
        let mut relays = vec![];
        for a in list {
            let Ok(t) = a.ticket.parse::<EndpointTicket>() else {
                continue;
            };
            let id = t.endpoint_addr().id;
            if id == self.net.id() || !st.knows(id.as_bytes())? || !verify_addr(a, id.as_bytes()) {
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

    /// Our address plus, for a space member, every signed member address we know (a contact
    /// learns no one else).
    fn peer_list(&self, st: &State, to: &[u8; 32]) -> Result<Vec<SignedAddr>> {
        let space = st.group()?;
        let mut list: Vec<SignedAddr> = vec![];
        if st.mine(self.device_id())?.contains(to) {
            // Our own device: everyone we know (§4.4).
            list.extend(
                st.store
                    .peers()?
                    .iter()
                    .filter_map(|b| postcard::from_bytes(b).ok()),
            );
        } else if st.mls.members(&space).contains(to) {
            list.extend(
                st.peers_of(&space)?
                    .iter()
                    .filter_map(|b| postcard::from_bytes(b).ok()),
            );
        }
        list.push(self.my_addr()?);
        Ok(list)
    }

    /// Exchanges addresses with every known peer and asks for what we miss (offering what they miss).
    fn sync(&self) -> Result<()> {
        self.sync_with(None)
    }

    /// `sync` with one peer only (`Some`) or with all of them.
    fn sync_with(&self, only: Option<EndpointId>) -> Result<()> {
        let st = self.st.lock().unwrap();
        if !st.in_space()? {
            return Ok(());
        }
        let (sees, envs) = (st.audience()?, st.env_groups()?);
        for p in st.store.peers()? {
            let Some(id) = peer_addr(&p).map(|a| *a.id.as_bytes()) else {
                continue;
            };
            if only.is_some_and(|o| *o.as_bytes() != id) {
                continue;
            }
            let ids = envs
                .iter()
                .filter(|(_, g)| sees(g, &id))
                .map(|(i, _)| *i)
                .collect();
            let to = vec![p];
            self.broadcast(to.clone(), SyncMsg::Peers(self.peer_list(&st, &id)?));
            self.broadcast(to, SyncMsg::Have { ids, reply: true });
        }
        Ok(())
    }

    /// Envelopes pulled from the mailbox (MLS authenticates them; no peer to check).
    fn take_envs(&self, envs: Vec<Envelope>) -> Result<Vec<(Chat, Event)>> {
        let me = self.device_id();
        Ok(self.st.lock().unwrap().take(envs, me))
    }

    /// Sets this phone's FCM token; it is registered with the mailbox on the next pull.
    pub fn set_push_token(&self, token: String) {
        *self.push_token.lock().unwrap() = Some(token);
        self.push_registered
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }

    /// Syncs with peers and pulls the mailbox now (the app came to the foreground or got a push).
    pub async fn sync_now(&self) {
        // After sleep or Android background the network may differ: iroh re-checks paths first.
        self.net.network_change().await;
        let _ = self.sync();
        self.poll_box().await;
    }

    /// Puts a new envelope into the mailbox in the background; peers online get it directly anyway.
    /// `push`: the mailbox wakes the other phones of the space, only the partner's for a 1:1 chat.
    /// For a 1:1 chat the push goes to every device of the partner. Callers holding the state
    /// lock pass `chat: None`.
    fn post_box(&self, env: &Envelope, push: bool, chat: Chat) {
        self.post_box_at(env, &env.space_id, push, chat)
    }

    /// `post_box` to the mailbox topic of `space` (our other devices pull our space's topic).
    fn post_box_at(&self, env: &Envelope, space: &[u8], push: bool, chat: Chat) {
        let Some(b) = &self.mailbox else { return };
        let Ok(body) = postcard::to_allocvec(env) else {
            return;
        };
        let url = format!(
            "{}/box/{}/{}",
            b.url,
            Mailbox::topic(space),
            data_encoding::HEXLOWER.encode(&envelope_id(env))
        );
        let http = b.http.clone();
        let from = self.push_token.lock().unwrap().clone().unwrap_or_default();
        let to = if chat.is_some() {
            let mine = self.mine();
            let devs = self.st.lock().unwrap().mls.members(&env.group_id);
            let tags = devs.iter().filter(|d| !mine.contains(*d));
            let tags: Vec<_> = tags.map(|d| Mailbox::device_tag(space, d)).collect();
            tags.join(",")
        } else {
            String::new()
        };
        tokio::spawn(async move {
            for wait in [2, 10, 60, 0] {
                match http
                    .post(&url)
                    .header("x-push", if push { "1" } else { "0" })
                    .header("x-from", &from)
                    .header("x-to", &to)
                    .body(body.clone())
                    .send()
                    .await
                    .and_then(|r| r.error_for_status())
                {
                    Ok(_) => break,
                    Err(e) if wait == 0 => eprintln!("mycel: mailbox post failed: {e}"),
                    Err(_) => tokio::time::sleep(std::time::Duration::from_secs(wait)).await,
                }
            }
        });
    }

    /// Pulls the envelopes posted to the mailbox since the last pull into the inbox loop.
    async fn poll_box(&self) {
        if let Err(e) = self.try_poll_box(&self.box_tx).await {
            eprintln!("mycel: mailbox poll failed: {e}");
        }
    }

    /// Pulls the topic of the space and of every contact (each has its own space id, §4.2).
    async fn try_poll_box(&self, tx: &mpsc::Sender<Vec<Envelope>>) -> Result<()> {
        let Some(b) = &self.mailbox else {
            return Ok(());
        };
        // (space id, meta key of its last seen seq).
        let topics: Vec<(Vec<u8>, String)> = {
            let st = self.st.lock().unwrap();
            if !st.in_space()? {
                return Ok(());
            }
            let mut t = vec![(st.group()?[..16].to_vec(), "box_seq".to_string())];
            for g in st.contacts()? {
                t.push((g[..16].to_vec(), format!("box_seq:{}", hex(&g[..16]))));
            }
            t
        };
        let token = self.push_token.lock().unwrap().clone();
        if let Some(t) = token
            && !self
                .push_registered
                .load(std::sync::atomic::Ordering::Relaxed)
        {
            for (space, _) in &topics {
                b.http
                    .post(format!("{}/push/{}", b.url, Mailbox::topic(space)))
                    .header("x-device", Mailbox::device_tag(space, &self.device_id()))
                    .body(t.clone())
                    .send()
                    .await?
                    .error_for_status()?;
            }
            self.push_registered
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
        for (space, key) in topics {
            let topic = Mailbox::topic(&space);
            let mut seq = self
                .st
                .lock()
                .unwrap()
                .store
                .meta(&key)?
                .and_then(|v| v.try_into().ok())
                .map_or(0, u64::from_le_bytes);
            loop {
                let url = format!("{}/box/{topic}?after={seq}", b.url);
                let text = b
                    .http
                    .get(url)
                    .send()
                    .await?
                    .error_for_status()?
                    .text()
                    .await?;
                // One envelope per line: "<seq> <base64 postcard>".
                let mut envs = vec![];
                let mut n = 0;
                for line in text.lines() {
                    let Some((s, body)) = line.split_once(' ') else {
                        continue;
                    };
                    n += 1;
                    seq = seq.max(s.parse()?);
                    match data_encoding::BASE64.decode(body.as_bytes()) {
                        Ok(b) => envs.extend(postcard::from_bytes::<Envelope>(&b).ok()),
                        Err(e) => eprintln!("mycel: bad mailbox entry: {e}"),
                    }
                }
                if !envs.is_empty() {
                    tx.send(envs).await?;
                }
                self.st
                    .lock()
                    .unwrap()
                    .store
                    .set_meta(&key, &seq.to_le_bytes())?;
                if n < BOX_PAGE {
                    break;
                }
            }
        }
        Ok(())
    }

    /// Tells the chat's online peers we are typing (`voice`: recording); at most once per 3 s.
    /// Ephemeral (§7.4): over the peer connections only, not stored, not mailed, no push.
    pub fn typing(&self, chat: &Chat, voice: bool) -> Result<()> {
        {
            let mut sent = self.typing_sent.lock().unwrap();
            if sent
                .get(chat)
                .is_some_and(|(t, v)| *v == voice && t.elapsed().as_secs() < 3)
            {
                return Ok(());
            }
            sent.insert(*chat, (Instant::now(), voice));
        }
        let (group_id, peers) = {
            let st = self.st.lock().unwrap();
            let g = st.gid(chat)?;
            (g, st.peers_of(&g)?)
        };
        let msg = SyncMsg::Typing { group_id, voice };
        for addr in peers.iter().filter_map(|p| peer_addr(p)) {
            if addr.id == self.net.id() {
                continue;
            }
            let (net, msg) = (self.net.clone(), msg.clone());
            // One try, no retries: a late "typing" is worse than none.
            tokio::spawn(async move { net.send(addr, &msg).await });
        }
        Ok(())
    }

    /// Incoming typing signals go to the returned receiver (replacing an older one).
    pub fn typing_events(&self) -> mpsc::Receiver<Typing> {
        let (tx, rx) = mpsc::channel(64);
        *self.typing_tx.lock().unwrap() = Some(tx);
        rx
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
        let who = *from.as_bytes();
        ensure!(st.knows(&who)?, "sender is not a member or contact");
        let me = self.device_id();
        if !st.store.has_peer(from.as_bytes())? {
            let t = EndpointTicket::new(EndpointAddr::new(from));
            st.store
                .upsert_peer(from.as_bytes(), t.to_string().as_bytes())?;
        }
        let mut out = vec![];
        match msg {
            SyncMsg::Envs(envs) => out = st.take(envs, me),
            SyncMsg::Have { ids, reply } => {
                let theirs: HashSet<_> = ids.into_iter().collect();
                let sees = st.audience()?;
                let mut missing = vec![];
                for (id, data) in st.store.envelopes()? {
                    if !theirs.contains(id.as_slice()) {
                        let e: Envelope = postcard::from_bytes(&data)?;
                        if sees(&e.group_id, &who) {
                            missing.push(e);
                        }
                    }
                }
                let to = vec![st.store.peer(from.as_bytes())?];
                if !missing.is_empty() {
                    self.broadcast(to.clone(), SyncMsg::Envs(missing));
                }
                if reply {
                    self.broadcast(to.clone(), SyncMsg::Peers(self.peer_list(&st, &who)?));
                    let ids = st
                        .env_groups()?
                        .into_iter()
                        .filter(|(_, g)| sees(g, &who))
                        .map(|(i, _)| i)
                        .collect();
                    self.broadcast(to, SyncMsg::Have { ids, reply: false });
                }
            }
            SyncMsg::Peers(list) => self.add_peers(&st, &list)?,
            SyncMsg::Typing { group_id, voice } => {
                ensure!(st.audience()?(&group_id, &who), "not in this chat");
                let chat = st.chat_of(&group_id)?.context("unknown chat")?;
                if let Some(tx) = &*self.typing_tx.lock().unwrap() {
                    let _ = tx.try_send(Typing {
                        chat,
                        from: who,
                        voice,
                    });
                }
            }
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
        let link = format!("link:{}", hex(&req.secret));
        if self.st.lock().unwrap().store.take_meta(&link)? {
            return self.accept_link(from, &req);
        }
        let sib = self.sibling_kps()?;
        let mut st = self.st.lock().unwrap();
        if st
            .store
            .take_meta(&format!("contact:{}", hex(&req.secret)))?
        {
            // A contact link: a new pair group with the two of us and our other devices (§4.2);
            // the joiner adds its own other devices.
            let mut gid = [0u8; 32];
            getrandom::fill(&mut gid)?;
            st.mls.create_group(&gid)?;
            let mut kps = vec![req.key_package.clone()];
            let ours = !sib.is_empty();
            // The joiner gets our other devices' addresses too: they are in the group.
            let mut peers = vec![self.my_addr()?];
            for (d, _) in &sib {
                let a = st.store.peer(d).ok();
                peers.extend(a.and_then(|b| postcard::from_bytes(&b).ok()));
            }
            kps.extend(sib.into_iter().map(|s| s.1));
            let (ids, _commit, welcome) = st.mls.add_members(&gid, &kps)?;
            let id = ids[0];
            ensure!(id == *from.as_bytes(), "KeyPackage is not the sender's");
            st.add_contact(gid, id)?;
            st.save_mls()?;
            st.store
                .upsert_peer(&id, &postcard::to_allocvec(&req.addr)?)?;
            if let Some(r) = &req.addr.relay {
                self.learn_relays(&st, std::slice::from_ref(r))?;
            }
            if ours {
                // Our other devices get the Welcome as an epoch-0 envelope of the group, also
                // through our space's mailbox topic (§4.4).
                let w = st.envelope(gid, welcome.clone(), 0)?;
                st.store_env(&w)?;
                self.post_box_at(&w, &st.group()?[..16], false, None);
                // Not to the joiner: it joins from the answer, a second join would fail.
                let to = st.peers_of(&gid)?.into_iter();
                let to = to.filter(|p| peer_addr(p).is_some_and(|a| a.id != from));
                self.broadcast(to.collect(), SyncMsg::Envs(vec![w]));
            }
            self.push_registered
                .store(false, std::sync::atomic::Ordering::Relaxed);
            return Ok(postcard::to_allocvec(&JoinResp { welcome, peers })?);
        }
        ensure!(st.admin()? == self.device_id(), "not the admin");
        // `invite:` is empty while unused, else holds the joiner's device, its answer and the time:
        // that same device may ask again within INVITE_RETRY (a dropped last step).
        let key = format!("invite:{}", hex(&req.secret));
        match st.store.meta(&key)? {
            None => bail!("unknown or used invite"),
            Some(v) if !v.is_empty() => {
                let (at, dev, resp): (u64, [u8; 32], Vec<u8>) = postcard::from_bytes(&v)?;
                ensure!(
                    dev == *from.as_bytes() && now_ms()? < at + INVITE_RETRY,
                    "unknown or used invite"
                );
                return Ok(resp);
            }
            Some(_) => {}
        }
        let gid = st.group()?;
        let epoch = st.mls.epoch(&gid);
        let (ids, commit, welcome) = st
            .mls
            .add_members(&gid, std::slice::from_ref(&req.key_package))?;
        let id = ids[0];
        ensure!(id == *from.as_bytes(), "KeyPackage is not the sender's");
        st.save_mls()?;
        let env = st.envelope(gid, commit, epoch)?;
        st.store_env(&env)?;
        let peers = st.peers_of(&gid)?;
        let list = self.peer_list(&st, &id)?;
        st.store
            .upsert_peer(from.as_bytes(), &postcard::to_allocvec(&req.addr)?)?;
        if let Some(r) = &req.addr.relay {
            self.learn_relays(&st, std::slice::from_ref(r))?;
        }
        self.post_box(&env, false, None);
        self.broadcast(peers.clone(), SyncMsg::Envs(vec![env]));
        self.broadcast(peers, SyncMsg::Peers(vec![req.addr]));
        let resp = postcard::to_allocvec(&JoinResp {
            welcome,
            peers: list,
        })?;
        st.store.set_meta(
            &key,
            &postcard::to_allocvec(&(now_ms()?, *from.as_bytes(), resp.clone()))?,
        )?;
        Ok(resp)
    }
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

/// The reactions on one message: each emoji (in the order first set) with who set it.
pub type Reactions = Vec<(String, Vec<DeviceId>)>;

/// Reactions over a history, per message.
pub fn reactions(hist: &[Event]) -> HashMap<EventId, Reactions> {
    let known: HashSet<_> = hist.iter().map(|e| e.id).collect();
    let mut out: HashMap<EventId, Reactions> = HashMap::new();
    for e in hist {
        let EventBody::Reaction {
            target,
            emoji,
            remove,
        } = &e.body
        else {
            continue;
        };
        // An emoji is a few code points; longer strings from a peer are junk.
        if !known.contains(target) || emoji.len() > 32 {
            continue;
        }
        let slots = out.entry(*target).or_default();
        let i = match slots.iter().position(|(x, _)| x == emoji) {
            Some(i) => i,
            None => {
                slots.push((emoji.clone(), vec![]));
                slots.len() - 1
            }
        };
        let who = &mut slots[i].1;
        who.retain(|d| *d != e.author_device);
        if !remove {
            who.push(e.author_device);
        }
    }
    for slots in out.values_mut() {
        slots.retain(|(_, who)| !who.is_empty());
    }
    out.retain(|_, s| !s.is_empty());
    out
}

fn is_msg(b: &EventBody) -> bool {
    matches!(b.msg(), EventBody::Text { .. } | EventBody::Media { .. })
}

/// What counts as unread from someone else: a message or a call.
fn is_incoming(b: &EventBody) -> bool {
    is_msg(b)
        || matches!(
            b,
            EventBody::Call {
                action: mycel_proto::CallAction::Offer { .. },
                ..
            }
        )
}

/// A fresh album id for `BlobRef::set_album`.
pub fn new_album() -> Result<String> {
    let mut id = [0; 8];
    getrandom::fill(&mut id)?;
    Ok(hex(&id))
}

/// The media kind and blob of a Media event.
pub fn media(ev: &Event) -> Option<(MediaKind, &BlobRef)> {
    match ev.body.msg() {
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

/// In-app update check: the body of a GitHub API URL (GitHub wants a User-Agent).
pub async fn https_text(url: &str) -> Result<String> {
    let r = tls_client(std::time::Duration::from_secs(15))?
        .get(url)
        .header(reqwest::header::USER_AGENT, "Mycel")
        .send()
        .await?
        .error_for_status()?;
    Ok(r.text().await?)
}

/// In-app update: downloads `url` into `to`; `on_bytes(done, total)` after each chunk (`total` 0 if unknown).
pub async fn https_download(
    url: &str,
    to: &Path,
    mut on_bytes: impl FnMut(u64, u64),
) -> Result<()> {
    let mut r = tls_client(std::time::Duration::from_secs(900))?
        .get(url)
        .header(reqwest::header::USER_AGENT, "Mycel")
        .send()
        .await?
        .error_for_status()?;
    let total = r.content_length().unwrap_or(0);
    let mut f = std::fs::File::create(to)?;
    let mut done = 0;
    while let Some(c) = r.chunk().await? {
        std::io::Write::write_all(&mut f, &c)?;
        done += c.len() as u64;
        on_bytes(done, total);
    }
    Ok(())
}

/// Settings → «Проверить»: the server answers over HTTP(S) within 5 s → its round trip in ms.
/// A relay passes on any reply; a mailbox must also say it is one.
pub async fn check_server(url: &str, mailbox: bool) -> Result<u64> {
    let b = Mailbox::new(url)?;
    let t = Instant::now();
    let r = b
        .http
        .get(&b.url)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await?;
    let ms = t.elapsed().as_millis() as u64;
    if mailbox {
        ensure!(
            r.text().await?.contains("Mycel mailbox"),
            "this is not a Mycel mailbox"
        );
    }
    Ok(ms)
}

/// iroh-relay over plain HTTP on the port of `url` (like `http://1.2.3.4:3340`), all interfaces.
/// Pass the same URL as [`Opts::own_relay`]. Kept running until the returned server is dropped or shut down.
// shortcut: no TLS (relayed QUIC is end-to-end encrypted anyway, but not disguised as HTTPS);
// serve HTTPS (ACME) once an anchor has a domain. Needs the TCP port forwarded and NAT loopback on the router.
#[cfg(not(target_os = "android"))]
pub async fn spawn_relay(url: &str) -> Result<iroh_relay::server::Server> {
    use iroh_relay::server::{RelayConfig, Server, ServerConfig};
    let u: RelayUrl = url.parse().context("relay url like http://1.2.3.4:3340")?;
    let port = u
        .port_or_known_default()
        .context("relay url without port")?;
    let mut cfg = ServerConfig::default();
    cfg.relay = Some(RelayConfig::new(([0, 0, 0, 0], port)));
    Ok(Server::spawn(cfg).await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn awaits_receipt_until_it_arrives() {
        let dirs = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
        let opts = Opts::default();
        let (a, _rx_a) = Node::open(dirs[0].path(), "pa", &opts).await.unwrap();
        let (b, _rx_b) = Node::open(dirs[1].path(), "pb", &opts).await.unwrap();
        a.create_space().unwrap();
        b.join(&a.invite().await.unwrap()).await.unwrap();
        assert!(!a.awaiting_receipt());
        a.send_text(&None, "hi", None).unwrap();
        assert!(a.awaiting_receipt());
        for _ in 0..100 {
            if !a.awaiting_receipt() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(!a.awaiting_receipt(), "the receipt did not stop the wait");
        a.close().await;
        b.close().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn backup_restores_history_as_a_new_device() {
        let (src, dst) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (a, _rx) = Node::open(src.path(), "pa", &Opts::default())
            .await
            .unwrap();
        a.create_space().unwrap();
        let ev = a.send_text(&None, "hi", None).unwrap();
        let mut dm = ev.clone();
        dm.id = [9; 32];
        a.st.lock()
            .unwrap()
            .store_event(&dm, &Some([7; 32]))
            .unwrap();
        let media = src.path().join("media");
        std::fs::create_dir_all(&media).unwrap();
        // Over one segment: the stream has a next and a last part.
        std::fs::write(media.join("x.jpg"), vec![5u8; 1_500_000]).unwrap();
        let old = a.device_id();
        a.close().await;

        let file = dst.path().join("b.mycelbak");
        backup::write(&file, "bk", &[("Моё", src.path(), "pa")], true).unwrap();
        let to = dst.path().join("s");
        let err = backup::restore(&file, "bad", "pw", |_| Ok(to.clone())).unwrap_err();
        assert_eq!(err.to_string(), backup::BAD_PASSWORD);
        assert!(!to.exists());
        let names = backup::restore(&file, "bk", "pw", |_| Ok(to.clone())).unwrap();
        assert_eq!(names, ["Моё"]);
        assert_eq!(
            std::fs::read(to.join("media").join("x.jpg")).unwrap().len(),
            1_500_000
        );

        let (b, _rx) = Node::open(&to, "pw", &Opts::default()).await.unwrap();
        assert_ne!(b.device_id(), old, "a restored device has a new key");
        assert!(b.admin().is_err(), "and joins its space again");
        assert!(b.history(&None).unwrap().contains(&ev));
        assert_eq!(b.dms().unwrap(), vec![[7; 32]]);
        assert_eq!(b.history(&Some([7; 32])).unwrap(), vec![dm]);
        b.close().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn backup_by_recovery_phrase() {
        let (src, dst) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (a, _rx) = Node::open(src.path(), "pa", &Opts::default())
            .await
            .unwrap();
        a.create_space().unwrap();
        let ev = a.send_text(&None, "hi", None).unwrap();
        a.close().await;

        let words = backup::new_phrase().unwrap();
        assert_eq!(words.split(' ').count(), 24);
        let file = dst.path().join("b.mycelbak");
        backup::write(&file, &words, &[("Моё", src.path(), "pa")], false).unwrap();
        // Typed back with other case and spacing.
        let typed = format!(
            "  {}
",
            words.to_uppercase().replace(' ', "   ")
        );
        assert_eq!(backup::phrase(&typed).unwrap(), words);
        // A swapped word breaks the checksum (almost always) or the key.
        let mut w: Vec<_> = words.split(' ').collect();
        w.swap(0, 1);
        let other = backup::new_phrase().unwrap();
        assert!(backup::phrase("abandon ability").is_err());
        assert!(backup::phrase(&format!("{} zzzz", &words[..words.rfind(' ').unwrap()])).is_err());
        let to = dst.path().join("s");
        let err = backup::restore(&file, &other, "pw", |_| Ok(to.clone())).unwrap_err();
        assert_eq!(err.to_string(), backup::BAD_PASSWORD);
        if let Ok(p) = backup::phrase(&w.join(" ")) {
            assert!(backup::restore(&file, &p, "pw", |_| Ok(to.clone())).is_err());
        }
        assert!(!to.exists());
        let pw = backup::phrase(&typed).unwrap();
        assert_eq!(
            backup::restore(&file, &pw, "pw", |_| Ok(to.clone())).unwrap(),
            ["Моё"]
        );
        let (b, _rx) = Node::open(&to, "pw", &Opts::default()).await.unwrap();
        assert!(b.history(&None).unwrap().contains(&ev));
        b.close().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn simultaneous_dms_converge_on_one_sending_group() {
        let dirs = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
        let opts = Opts::default();
        let (a, _ra) = Node::open(dirs[0].path(), "pa", &opts).await.unwrap();
        let (b, _rb) = Node::open(dirs[1].path(), "pb", &opts).await.unwrap();
        a.create_space().unwrap();
        b.join(&a.invite().await.unwrap()).await.unwrap();
        let (ida, idb) = (a.device_id(), b.device_id());
        a.set_profile("A", "", 1).unwrap();
        b.set_profile("B", "", 2).unwrap();
        for _ in 0..200 {
            if a.profiles().unwrap().contains_key(&idb) && b.profiles().unwrap().contains_key(&ida)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        // Both sides create their 1:1 group before either Welcome arrives (what open_dm does
        // when the two open a chat at once), then each applies the other's Welcome.
        let kp = |n: &Node| n.st.lock().unwrap().mls.key_package(false).unwrap();
        let (kp_a, kp_b) = (kp(&a), kp(&b));
        let open = |n: &Node, kp: Vec<u8>, peer: [u8; 32]| {
            let mut st = n.st.lock().unwrap();
            let mut gid = [0u8; 32];
            getrandom::fill(&mut gid).unwrap();
            st.mls.create_group(&gid).unwrap();
            let (_, _, welcome) = st.mls.add_members(&gid, &[kp]).unwrap();
            st.add_dm(gid, peer).unwrap();
            st.save_mls().unwrap();
            st.envelope(gid, welcome, 0).unwrap()
        };
        let w_a = open(&a, kp_b, idb);
        let w_b = open(&b, kp_a, ida);
        b.st.lock().unwrap().receive(&w_a, idb).unwrap();
        a.st.lock().unwrap().receive(&w_b, ida).unwrap();
        let gid = |n: &Node, p: [u8; 32]| n.st.lock().unwrap().gid(&Some(p)).unwrap();
        let groups = |n: &Node, p: [u8; 32]| {
            n.st.lock()
                .unwrap()
                .dms()
                .unwrap()
                .into_iter()
                .filter(|(_, q)| *q == p)
                .count()
        };
        assert_eq!((groups(&a, idb), groups(&b, ida)), (2, 2));
        // Both send into the smaller group id; the other group is only read.
        let smaller = [w_a.group_id, w_b.group_id].into_iter().min().unwrap();
        assert_eq!(gid(&a, idb), smaller);
        assert_eq!(gid(&b, ida), smaller);
        let ev = a.send_text(&Some(idb), "hi", None).unwrap();
        let ev2 = b.send_text(&Some(ida), "там", None).unwrap();
        for _ in 0..200 {
            if b.history(&Some(ida)).unwrap().contains(&ev)
                && a.history(&Some(idb)).unwrap().contains(&ev2)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(b.history(&Some(ida)).unwrap().contains(&ev));
        assert!(a.history(&Some(idb)).unwrap().contains(&ev2));
        a.close().await;
        b.close().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn invite_answer_is_repeated_for_the_same_device_only() {
        let dirs = [
            tempfile::tempdir().unwrap(),
            tempfile::tempdir().unwrap(),
            tempfile::tempdir().unwrap(),
        ];
        let opts = Opts::default();
        let (a, _ra) = Node::open(dirs[0].path(), "pa", &opts).await.unwrap();
        let (b, _rb) = Node::open(dirs[1].path(), "pb", &opts).await.unwrap();
        let (c, _rc) = Node::open(dirs[2].path(), "pc", &opts).await.unwrap();
        a.create_space().unwrap();
        let link = a.invite().await.unwrap();
        let b32 = link.strip_prefix(LINK).unwrap();
        let (_, secret, _, _): ([u8; 32], [u8; 16], String, Vec<String>) =
            postcard::from_bytes(&BASE32_NOPAD.decode(b32.as_bytes()).unwrap()).unwrap();
        let req = |n: &Node| {
            let key_package = n.st.lock().unwrap().mls.key_package(false).unwrap();
            let addr = n.my_addr().unwrap();
            postcard::to_allocvec(&JoinReq {
                secret,
                key_package,
                addr,
            })
            .unwrap()
        };
        let id = |n: &Node| EndpointId::from_bytes(&n.device_id()).unwrap();

        let first = a.accept_join(id(&b), &req(&b)).unwrap();
        assert!(!first.is_empty());
        // The joiner lost the answer and asks again with a new KeyPackage: same Welcome back.
        let again = a.accept_join(id(&b), &req(&b)).unwrap();
        assert_eq!(first, again);
        // Another device with the same link is refused.
        assert!(a.accept_join(id(&c), &req(&c)).is_err());
        a.close().await;
        b.close().await;
        c.close().await;
    }

    #[tokio::test]
    async fn checks_a_relay_and_tells_it_from_a_mailbox() {
        let url = "http://127.0.0.1:34871";
        let _relay = spawn_relay(url).await.unwrap();
        assert!(check_server(url, false).await.is_ok());
        assert!(check_server(url, true).await.is_err());
        assert!(check_server("http://127.0.0.1:1", false).await.is_err());
    }
}
