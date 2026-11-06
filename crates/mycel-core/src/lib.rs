//! Thin node API: space create/invite/join, send_text, incoming event stream, history, sync.

use anyhow::{Context, Result, bail, ensure};
use data_encoding::BASE32_NOPAD;
use mycel_crypto::mls::{Incoming, Mls};
use mycel_crypto::{SecretKey, envelope_id, sign_event, verify_event};
use mycel_net::{EndpointAddr, EndpointId, EndpointTicket, JoinInbox, Net};
use mycel_proto::{Envelope, Event, EventBody, JoinReq, JoinResp, SyncMsg};
use mycel_store::Store;
use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;

pub use mycel_proto;

const LINK: &str = "mycel://join/";

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

    /// Decrypts and stores one envelope; returns a new text event if any.
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
}

impl Node {
    /// Opens the encrypted store (creating the device key on first run), binds the network and
    /// syncs with known peers. Returns the node and a stream of new verified events from peers.
    pub async fn open(
        dir: &Path,
        password: &str,
        port: u16,
        relay: bool,
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
        let (net, mut inbox, joins) = Net::bind(key, port, relay).await?;
        let node = Arc::new(Self {
            st: Mutex::new(State { store, mls }),
            net,
        });
        let (tx, rx) = mpsc::channel(256);
        let n = node.clone();
        tokio::spawn(async move {
            while let Some((from, msg)) = inbox.recv().await {
                match n.handle(from, msg) {
                    Ok(evs) => {
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
        Ok((node, rx))
    }

    /// Ticket with this node's id and current addresses.
    pub async fn ticket(&self, wait_relay: bool) -> String {
        if wait_relay {
            self.net.online().await;
        }
        EndpointTicket::new(self.net.addr()).to_string()
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

    /// One-time invite link: base32(space id, one-time secret, admin ticket). The ticket's id is the admin key.
    pub async fn invite(&self, wait_relay: bool) -> Result<String> {
        let ticket = self.ticket(wait_relay).await;
        let mut secret = [0u8; 16];
        getrandom::fill(&mut secret)?;
        let st = self.st.lock().unwrap();
        ensure!(st.admin()? == self.device_id(), "only the admin can invite");
        st.store
            .set_meta(&format!("invite:{}", hex(&secret)), &[])?;
        let bytes = postcard::to_allocvec(&(st.group()?, secret, ticket))?;
        Ok(format!("{LINK}{}", BASE32_NOPAD.encode(&bytes)))
    }

    /// Joins a space by invite link: proves the secret, sends a KeyPackage, applies the Welcome.
    pub async fn join(&self, link: &str) -> Result<()> {
        let b32 = link
            .trim()
            .strip_prefix(LINK)
            .context("not a mycel invite")?;
        let (gid, secret, ticket): ([u8; 32], [u8; 16], String) =
            postcard::from_bytes(&BASE32_NOPAD.decode(b32.as_bytes())?)?;
        let admin: EndpointAddr = ticket.parse::<EndpointTicket>()?.into();
        let req = {
            let st = self.st.lock().unwrap();
            ensure!(!st.mls.has_group(), "already in a space");
            JoinReq {
                secret,
                key_package: st.mls.key_package()?,
                addr: EndpointTicket::new(self.net.addr()).to_string(),
            }
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
        add_peers(&st.store, &resp.peers, self.net.id())
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
                timestamp_ms: SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64,
                body: EventBody::Text {
                    text: text.into(),
                    reply_to: None,
                    mentions: vec![],
                },
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

    pub async fn close(&self) {
        self.net.close().await
    }

    /// Asks every known peer for what we miss (and offers what they miss).
    fn sync(&self) -> Result<()> {
        let st = self.st.lock().unwrap();
        if !st.mls.has_group() {
            return Ok(());
        }
        let msg = SyncMsg::Have {
            ids: env_ids(&st.store)?,
            reply: true,
        };
        self.broadcast(st.store.peers()?, msg);
        Ok(())
    }

    /// Fire-and-forget send to each peer; sync on the next connection covers failures.
    fn broadcast(&self, peers: Vec<Vec<u8>>, msg: SyncMsg) {
        for p in peers {
            let Ok(t) = std::str::from_utf8(&p)
                .unwrap_or("")
                .parse::<EndpointTicket>()
            else {
                continue;
            };
            let addr: EndpointAddr = t.into();
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
                    let ids = env_ids(&st.store)?;
                    self.broadcast(to, SyncMsg::Have { ids, reply: false });
                }
            }
            SyncMsg::Peers(list) => add_peers(&st.store, &list, self.net.id())?,
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
        let ticket: EndpointTicket = req.addr.parse()?;
        ensure!(
            ticket.endpoint_addr().id == from,
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
        st.store.upsert_peer(from.as_bytes(), req.addr.as_bytes())?;
        self.broadcast(peers.clone(), SyncMsg::Envs(vec![env]));
        self.broadcast(peers.clone(), SyncMsg::Peers(vec![req.addr]));
        let peers = peers
            .into_iter()
            .map(String::from_utf8)
            .collect::<Result<_, _>>()?;
        Ok(postcard::to_allocvec(&JoinResp { welcome, peers })?)
    }
}

fn env_ids(store: &Store) -> Result<Vec<[u8; 32]>> {
    Ok(store
        .envelopes()?
        .into_iter()
        .filter_map(|(id, _)| id.try_into().ok())
        .collect())
}

fn add_peers(store: &Store, tickets: &[String], me: EndpointId) -> Result<()> {
    for t in tickets {
        let id = t.parse::<EndpointTicket>()?.endpoint_addr().id;
        if id != me {
            store.upsert_peer(id.as_bytes(), t.as_bytes())?;
        }
    }
    Ok(())
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|b| format!("{b:02x}")).collect()
}
