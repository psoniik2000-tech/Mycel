//! Thin node API: space create/invite/join, send_text, incoming event stream, history, sync.

use anyhow::{Context, Result, bail, ensure};
use data_encoding::BASE32_NOPAD;
use mycel_crypto::mls::{Incoming, Mls};
use mycel_crypto::{SecretKey, envelope_id, sign_addr, sign_event, verify_addr, verify_event};
use mycel_net::{EndpointAddr, EndpointId, EndpointTicket, JoinInbox, Net};
use mycel_proto::{
    Envelope, Event, EventBody, EventId, JoinReq, JoinResp, ReceiptKind, SignedAddr, SyncMsg,
};
use mycel_store::Store;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;

pub use mycel_net::RelayUrl;
pub use mycel_proto;

const LINK: &str = "mycel://join/";

/// Network options of a node.
#[derive(Debug, Default, Clone)]
pub struct Opts {
    /// iroh UDP port, 0 = any free one.
    pub port: u16,
    /// URL of the iroh-relay this node serves (anchor), advertised to the members.
    pub own_relay: Option<String>,
    /// Also use the public n0 relays and address lookup (off by default, last resort).
    pub n0: bool,
}

// shortcut: a node is in exactly one space = one MLS group; multiple chats per space later.
struct State {
    store: Store,
    mls: Mls,
}

impl State {
    fn save_mls(&self) -> Result<()> {
        self.store.set_meta("mls", &self.mls.state())
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

    fn envelope(&self, ciphertext: Vec<u8>, epoch: u64) -> Result<Envelope> {
        let group_id = self.group()?;
        Ok(Envelope {
            version: 1,
            space_id: group_id[..16].try_into()?,
            group_id,
            epoch,
            ciphertext,
        })
    }

    fn store_env(&self, env: &Envelope) -> Result<()> {
        self.store
            .insert_envelope(&envelope_id(env), &postcard::to_allocvec(env)?)
    }

    fn store_event(&self, ev: &Event) -> Result<bool> {
        self.store.insert_event(
            &ev.id,
            ev.lamport,
            ev.timestamp_ms,
            &postcard::to_allocvec(ev)?,
        )
    }

    /// Decrypts and stores one envelope; returns a new event if any.
    fn receive(&mut self, env: &Envelope) -> Result<Option<Event>> {
        ensure!(env.group_id == self.group()?, "foreign group");
        if self.store.has_envelope(&envelope_id(env))? {
            return Ok(None);
        }
        let out = match self.mls.process(&env.ciphertext, &self.admin()?) {
            Ok(Incoming::App { sender, data }) => {
                let ev: Event = postcard::from_bytes(&data)?;
                ensure!(verify_event(&ev), "bad signature");
                ensure!(ev.author_device == sender, "author is not the MLS sender");
                self.store_event(&ev)?.then_some(ev)
            }
            Ok(Incoming::Commit) => None,
            // Before our join (or past retention): unreadable for us, kept so we can forward it.
            Err(_) if env.epoch < self.mls.epoch() => None,
            Err(e) => return Err(e),
        };
        self.save_mls()?;
        self.store_env(env)?;
        Ok(out)
    }
}

pub struct Node {
    st: Mutex<State>,
    net: Net,
    own_relay: Option<String>,
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
        let mls = Mls::new(
            &key,
            store.meta("mls")?.as_deref(),
            store.meta("group")?.as_deref(),
        )?;
        let st = State { store, mls };
        let mut relays = st.relays()?;
        relays.extend(opts.own_relay.clone());
        let (net, mut inbox, joins) = Net::bind(key, opts.port, &relays, opts.n0).await?;
        let node = Arc::new(Self {
            st: Mutex::new(st),
            net,
            own_relay: opts.own_relay.clone(),
        });
        let (tx, rx) = mpsc::channel(256);
        let n = node.clone();
        tokio::spawn(async move {
            while let Some((from, msg)) = inbox.recv().await {
                match n.handle(from, msg) {
                    Ok(evs) => {
                        n.send_receipt(&evs);
                        for ev in evs {
                            let _ = tx.send(ev).await;
                        }
                    }
                    Err(e) => eprintln!("mycel: dropped message from {}: {e}", from.fmt_short()),
                }
            }
        });
        tokio::spawn(node.clone().serve_joins(joins));
        node.sync()?;
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
        if !self.relays().is_empty() {
            self.net.online().await;
        }
        EndpointTicket::new(self.net.addr()).to_string()
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
        ensure!(!st.mls.has_group(), "already in a space");
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
    pub async fn join(&self, link: &str) -> Result<()> {
        let b32 = link
            .trim()
            .strip_prefix(LINK)
            .context("not a mycel invite")?;
        let (gid, secret, ticket, relays): ([u8; 32], [u8; 16], String, Vec<String>) =
            postcard::from_bytes(&BASE32_NOPAD.decode(b32.as_bytes())?)?;
        let admin: EndpointAddr = ticket.parse::<EndpointTicket>()?.into();
        {
            let st = self.st.lock().unwrap();
            ensure!(!st.mls.has_group(), "already in a space");
            self.learn_relays(&st, &relays)?;
        }
        if !relays.is_empty() {
            self.net.online().await;
        }
        let req = JoinReq {
            secret,
            key_package: self.st.lock().unwrap().mls.key_package()?,
            addr: self.my_addr()?,
        };
        let admin_id = admin.id;
        let resp = self.net.join(admin, &postcard::to_allocvec(&req)?).await?;
        let resp: JoinResp = postcard::from_bytes(&resp).context("join refused")?;
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
        self.add_peers(&st, &resp.peers)
    }

    /// Device ids of the space members.
    pub fn members(&self) -> Vec<[u8; 32]> {
        self.st.lock().unwrap().mls.members()
    }

    pub fn admin(&self) -> Result<[u8; 32]> {
        self.st.lock().unwrap().admin()
    }

    /// Events ordered by (lamport, ts, id).
    pub fn history(&self) -> Result<Vec<Event>> {
        let rows = self.st.lock().unwrap().store.events()?;
        rows.iter().map(|b| Ok(postcard::from_bytes(b)?)).collect()
    }

    /// Signs, encrypts (MLS), stores and sends a text event to every known peer.
    pub fn send_text(&self, text: &str) -> Result<Event> {
        self.send_event(EventBody::Text {
            text: text.into(),
            reply_to: None,
            mentions: vec![],
        })
    }

    /// Delivery of own text events (§7.4): 0 ⏱ nobody confirmed yet, 1 ✓ some member device has it,
    /// 2 ✓✓ every other member device has it (by their Delivered receipts).
    // shortcut: "has it" = a receipt up to an event with lamport >= ours; use per-author clocks if concurrent sends mislead it.
    pub fn statuses(&self) -> Result<Vec<(EventId, u8)>> {
        let me = self.device_id();
        let others: Vec<_> = self.members().into_iter().filter(|m| *m != me).collect();
        let hist = self.history()?;
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
            .filter(|e| e.author_device == me && matches!(e.body, EventBody::Text { .. }))
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

    /// Store-and-forward retention (§7.3): drops envelopes and events older than `days`.
    // shortcut: time-based only; also drop once every member has receipted, if anchors run out of disk.
    pub fn prune(&self, days: u64) -> Result<usize> {
        let cutoff = now_ms()?.saturating_sub(days * 86_400_000);
        self.st.lock().unwrap().store.prune(cutoff as i64)
    }

    pub async fn close(&self) {
        self.net.close().await
    }

    /// Peers we currently hold a connection to (the UI's connection indicator).
    pub fn peers_online(&self) -> usize {
        self.net.connected()
    }

    /// Signs, encrypts (MLS), stores and sends an event to every known peer.
    fn send_event(&self, body: EventBody) -> Result<Event> {
        let (ev, env, peers) = {
            let mut st = self.st.lock().unwrap();
            let head = st.store.head()?;
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
            let epoch = st.mls.epoch();
            let ct = st.mls.encrypt(&postcard::to_allocvec(&ev)?)?;
            st.save_mls()?;
            let env = st.envelope(ct, epoch)?;
            st.store_event(&ev)?;
            st.store_env(&env)?;
            (ev, env, st.store.peers()?)
        };
        self.broadcast(peers, SyncMsg::Envs(vec![env]));
        Ok(ev)
    }

    /// One Delivered receipt per received batch, up to its newest text from someone else (§7.4).
    // shortcut: receipts are stored events (~members per message); prune or aggregate if history grows too big.
    fn send_receipt(&self, evs: &[Event]) {
        let me = self.device_id();
        let last = evs
            .iter()
            .filter(|e| e.author_device != me && matches!(e.body, EventBody::Text { .. }))
            .max_by_key(|e| e.lamport);
        if let Some(e) = last {
            let body = EventBody::Receipt {
                up_to: e.id,
                kind: ReceiptKind::Delivered,
            };
            if let Err(e) = self.send_event(body) {
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
        let members = st.mls.members();
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
        if !st.mls.has_group() {
            return Ok(());
        }
        let peers = st.store.peers()?;
        self.broadcast(peers.clone(), SyncMsg::Peers(self.peer_list(&st)?));
        let ids = env_ids(&st.store)?;
        self.broadcast(peers, SyncMsg::Have { ids, reply: true });
        Ok(())
    }

    /// Fire-and-forget send to each peer; sync on the next connection covers failures.
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
                let id = addr.id;
                if let Err(e) = net.send(addr, &msg).await {
                    eprintln!("mycel: send to {} failed: {e}", id.fmt_short());
                }
            });
        }
    }

    fn handle(&self, from: EndpointId, msg: SyncMsg) -> Result<Vec<Event>> {
        let mut st = self.st.lock().unwrap();
        ensure!(
            st.mls.members().contains(from.as_bytes()),
            "sender is not a member"
        );
        if !st.store.has_peer(from.as_bytes())? {
            let t = EndpointTicket::new(EndpointAddr::new(from));
            st.store
                .upsert_peer(from.as_bytes(), t.to_string().as_bytes())?;
        }
        let mut out = vec![];
        match msg {
            SyncMsg::Envs(mut envs) => {
                // Commits must apply before later-epoch messages.
                envs.sort_by_key(|e| e.epoch);
                for env in envs {
                    match st.receive(&env) {
                        Ok(Some(ev)) => out.push(ev),
                        Ok(None) => {}
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
                }
                Err(e) => eprintln!("mycel: join from {} refused: {e}", from.fmt_short()),
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
        let epoch = st.mls.epoch();
        let (id, commit, welcome) = st.mls.add_member(&req.key_package)?;
        ensure!(id == *from.as_bytes(), "KeyPackage is not the sender's");
        st.save_mls()?;
        let env = st.envelope(commit, epoch)?;
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

fn now_ms() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64)
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|b| format!("{b:02x}")).collect()
}
