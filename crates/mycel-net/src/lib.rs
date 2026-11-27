//! iroh transport (DESIGN.md §6.1 T1–T2, §14.3): Envelopes over ALPN `mycel/sync/1`.

use anyhow::Result;
use iroh::endpoint::presets;
use iroh::{Endpoint, RelayConfig, RelayMap, RelayMode, TransportAddr, Watcher};
use mycel_proto::SyncMsg;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

pub use iroh::endpoint::Connection;
pub use iroh::{EndpointAddr, EndpointId, RelayUrl, SecretKey};
pub use iroh_tickets::endpoint::EndpointTicket;

pub const ALPN: &[u8] = b"mycel/sync/1";
pub const JOIN_ALPN: &[u8] = b"mycel/join/1";
pub const BLOBS_ALPN: &[u8] = b"mycel/blobs/1";
/// Call media (§9): audio as QUIC datagrams, video one uni stream per frame.
pub const CALL_ALPN: &[u8] = b"mycel/call/1";
// shortcut: one message per stream, capped at 16 MiB (a full-history Envs batch); chunk when histories grow.
const MAX_MSG: usize = 16 << 20;
/// How long a cached connection gets to acknowledge a message before it is replaced.
const ACK_TIMEOUT: Duration = Duration::from_secs(5);
/// No home relay this long (our relays down: anchor PC off, server gone) → the fallback relays come back.
const NO_RELAY_FOR: Duration = Duration::from_secs(15);
/// While on the fallback relays, our relays are probed this often; one answering → back to them.
const PROBE_EVERY: Duration = Duration::from_secs(30);

/// Incoming sync messages with the authenticated sender.
pub type Inbox = mpsc::Receiver<(EndpointId, SyncMsg)>;
/// Incoming join requests: (sender, request bytes, reply). Dropping the reply refuses.
pub type JoinInbox = mpsc::Receiver<(EndpointId, Vec<u8>, oneshot::Sender<Vec<u8>>)>;
/// Incoming `mycel/blobs/1` and `mycel/call/1` connections; the owner checks membership and serves them.
pub type ConnInbox = mpsc::Receiver<Connection>;

#[derive(Clone)]
pub struct Net {
    ep: Endpoint,
    conns: Arc<Mutex<HashMap<EndpointId, Connection>>>,
    /// Unix ms of the last message received from each peer.
    seen: Arc<Mutex<HashMap<EndpointId, u64>>>,
    tx: mpsc::Sender<(EndpointId, SyncMsg)>,
    /// Our relays: every dial also tries them, so peers whose stored address names a dead relay
    /// (n0 blocked) are still found on the relay we share.
    relays: Arc<Mutex<Vec<RelayUrl>>>,
    /// Relays used only while none of ours is reachable (the public n0 ones when `n0` is on).
    fallback: Arc<Mutex<Vec<RelayUrl>>>,
    fallback_on: Arc<AtomicBool>,
    /// Gets the peer of every cached sync connection that closed (`on_lost`).
    lost: Arc<Mutex<Option<mpsc::UnboundedSender<EndpointId>>>>,
}

impl Net {
    /// Relays are our anchors' iroh-relay URLs (§6.1 T2); more can be added with `add_relay`.
    /// `n0 = true` also uses the public n0 relays and address lookup (last resort). `port = 0`: any free port.
    pub async fn bind(
        key: SecretKey,
        port: u16,
        relays: &[String],
        n0: bool,
    ) -> Result<(Self, Inbox, JoinInbox, ConnInbox)> {
        let mut b = if n0 {
            Endpoint::builder(presets::N0)
        } else {
            Endpoint::builder(presets::Minimal).relay_mode(RelayMode::Custom(RelayMap::empty()))
        };
        b = b.secret_key(key).alpns(vec![
            ALPN.to_vec(),
            JOIN_ALPN.to_vec(),
            BLOBS_ALPN.to_vec(),
            CALL_ALPN.to_vec(),
        ]);
        if port != 0 {
            b = b
                .clear_ip_transports()
                .bind_addr(SocketAddr::from(([0, 0, 0, 0], port)))?;
        }
        let ep = b.bind().await?;
        let (tx, rx) = mpsc::channel(256);
        let (jtx, jrx) = mpsc::channel(16);
        let (btx, brx) = mpsc::channel(16);
        let net = Self {
            ep,
            conns: Default::default(),
            seen: Default::default(),
            tx,
            relays: Default::default(),
            fallback: Arc::new(Mutex::new(if n0 {
                RelayMode::Default.relay_map().urls()
            } else {
                vec![]
            })),
            fallback_on: Arc::new(AtomicBool::new(n0)),
            lost: Default::default(),
        };
        let n = net.clone();
        tokio::spawn(async move {
            while let Some(incoming) = n.ep.accept().await {
                let Ok(conn) = incoming.await else { continue };
                if conn.alpn() == JOIN_ALPN {
                    tokio::spawn(serve_join(conn, jtx.clone()));
                } else if conn.alpn() == BLOBS_ALPN || conn.alpn() == CALL_ALPN {
                    let _ = btx.send(conn).await;
                } else {
                    n.track(conn);
                }
            }
        });
        for r in relays {
            net.add_relay(r).await?;
        }
        tokio::spawn(net.clone().watch_relays());
        Ok((net, rx, jrx, brx))
    }

    /// Without this, a space whose relays are all down (anchor PC off) leaves us on no relay at
    /// all: brings the fallback relays back after `NO_RELAY_FOR`, drops them again once one of
    /// ours answers.
    async fn watch_relays(self) {
        let mut down_since: Option<tokio::time::Instant> = None;
        let mut probed = tokio::time::Instant::now();
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if self.ep.is_closed() {
                break;
            }
            let fallback = self.fallback.lock().unwrap().clone();
            let ours = self.relays.lock().unwrap().clone();
            if fallback.is_empty() || ours.is_empty() {
                continue;
            }
            if !self.fallback_on.load(Ordering::Relaxed) {
                if self.relay_up() {
                    down_since = None;
                } else if down_since
                    .get_or_insert_with(tokio::time::Instant::now)
                    .elapsed()
                    >= NO_RELAY_FOR
                {
                    eprintln!("mycel: no relay of ours is reachable, using the fallback ones");
                    for u in fallback.iter().filter(|u| !ours.contains(u)) {
                        let cfg = Arc::new(RelayConfig::new(u.clone(), None));
                        self.ep.insert_relay(u.clone(), cfg).await;
                    }
                    self.fallback_on.store(true, Ordering::Relaxed);
                    down_since = None;
                    probed = tokio::time::Instant::now();
                }
            } else if probed.elapsed() >= PROBE_EVERY {
                probed = tokio::time::Instant::now();
                let mut any = false;
                for u in &ours {
                    any |= answers(u).await;
                }
                if any {
                    eprintln!("mycel: a relay of ours is back, leaving the fallback ones");
                    self.drop_fallback().await;
                }
            }
        }
    }

    /// Removes the fallback relays (not ours) from the relay map.
    async fn drop_fallback(&self) {
        let ours = self.relays.lock().unwrap().clone();
        let fallback = self.fallback.lock().unwrap().clone();
        for u in fallback.iter().filter(|u| !ours.contains(u)) {
            self.ep.remove_relay(u).await;
        }
        self.fallback_on.store(false, Ordering::Relaxed);
    }

    /// Tests: relays to fall back on instead of the n0 ones (not in the relay map until ours fail).
    #[doc(hidden)]
    pub fn set_fallback(&self, urls: Vec<RelayUrl>) {
        *self.fallback.lock().unwrap() = urls;
        self.fallback_on.store(false, Ordering::Relaxed);
    }

    /// Whether a home relay is connected right now.
    pub fn relay_up(&self) -> bool {
        self.ep
            .home_relay_status()
            .get()
            .iter()
            .any(|s| s.is_connected())
    }

    /// Peers whose cached sync connection closed come out of the returned receiver (replacing an
    /// older one), so the owner can reconnect and catch up at once, not on the next periodic sync.
    pub fn on_lost(&self) -> mpsc::UnboundedReceiver<EndpointId> {
        let (tx, rx) = mpsc::unbounded_channel();
        *self.lost.lock().unwrap() = Some(tx);
        rx
    }

    /// The OS network may have changed (resume from sleep, app back in the foreground):
    /// iroh re-checks its paths and relay now.
    pub async fn network_change(&self) {
        self.ep.network_change().await
    }

    /// Adds a relay to the relay map; the endpoint picks the best one as its home relay.
    /// Once a space has its own relay (an anchor or an added server), the public n0 relays
    /// are dropped, so every member meets there; n0 address lookup stays.
    pub async fn add_relay(&self, url: &str) -> Result<()> {
        let url: RelayUrl = url.parse()?;
        let cfg = Arc::new(RelayConfig::new(url.clone(), None));
        self.relays.lock().unwrap().push(url.clone());
        self.ep.insert_relay(url, cfg).await;
        self.drop_fallback().await;
        Ok(())
    }

    fn reach(&self, to: EndpointAddr) -> EndpointAddr {
        let rs = self.relays.lock().unwrap().clone();
        to.with_addrs(rs.into_iter().map(TransportAddr::Relay))
    }

    pub fn id(&self) -> EndpointId {
        self.ep.id()
    }

    pub fn secret_key(&self) -> &SecretKey {
        self.ep.secret_key()
    }

    pub fn addr(&self) -> EndpointAddr {
        self.ep.addr()
    }

    /// Waits (up to 5 s) until a relay is connected, so `addr()` includes it.
    pub async fn online(&self) {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), self.ep.online()).await;
    }

    /// One uni stream per message; the stream end is the frame boundary.
    pub async fn send(&self, to: EndpointAddr, msg: &SyncMsg) -> Result<()> {
        let bytes = postcard::to_allocvec(msg)?;
        let cached = self.conns.lock().unwrap().get(&to.id).cloned();
        if let Some(conn) = cached {
            // A cached connection can be half-dead (phone switched network, VPN): the write
            // succeeds locally but never arrives. No ack in time → drop it and dial afresh.
            if let Ok(Ok(())) = tokio::time::timeout(ACK_TIMEOUT, write(&conn, &bytes)).await {
                return Ok(());
            }
            conn.close(0u32.into(), b"stale");
        }
        let conn = self.ep.connect(self.reach(to), ALPN).await?;
        self.track(conn.clone());
        write(&conn, &bytes).await
    }

    /// One request/response on `mycel/join/1`.
    pub async fn join(&self, to: EndpointAddr, req: &[u8]) -> Result<Vec<u8>> {
        let conn = self.ep.connect(self.reach(to), JOIN_ALPN).await?;
        let (mut s, mut r) = conn.open_bi().await?;
        s.write_all(req).await?;
        s.finish()?;
        let resp = r.read_to_end(MAX_MSG).await?;
        conn.close(0u32.into(), b"");
        Ok(resp)
    }

    /// A fresh connection on `alpn` (blobs: iroh-blobs requests run over it; calls).
    pub async fn connect(&self, to: EndpointAddr, alpn: &[u8]) -> Result<Connection> {
        Ok(self.ep.connect(self.reach(to), alpn).await?)
    }

    pub async fn close(&self) {
        self.ep.close().await
    }

    pub fn is_closed(&self) -> bool {
        self.ep.is_closed()
    }

    /// Number of peers with an open sync connection.
    pub fn connected(&self) -> usize {
        let conns = self.conns.lock().unwrap();
        conns
            .values()
            .filter(|c| c.close_reason().is_none())
            .count()
    }

    /// Link to `peer`: `Some(true)` via a relay, `Some(false)` direct, `None` no open connection;
    /// and when (unix ms) we last received a message from it.
    pub fn link(&self, peer: EndpointId) -> (Option<bool>, Option<u64>) {
        let conn = self.conns.lock().unwrap().get(&peer).cloned();
        let link = conn.filter(|c| c.close_reason().is_none()).map(|c| {
            c.paths()
                .iter()
                .find(|p| p.is_selected())
                .is_some_and(|p| p.is_relay())
        });
        (link, self.seen.lock().unwrap().get(&peer).copied())
    }

    /// Caches the connection for replies and reads envelopes from it until it closes.
    fn track(&self, conn: Connection) {
        let peer = conn.remote_id();
        self.conns.lock().unwrap().insert(peer, conn.clone());
        let (tx, seen) = (self.tx.clone(), self.seen.clone());
        let (conns, lost) = (self.conns.clone(), self.lost.clone());
        tokio::spawn(async move {
            while let Ok(mut rs) = conn.accept_uni().await {
                let Ok(bytes) = rs.read_to_end(MAX_MSG).await else {
                    continue;
                };
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_millis() as u64);
                seen.lock().unwrap().insert(peer, now);
                if let Ok(msg) = postcard::from_bytes::<SyncMsg>(&bytes)
                    && tx.send((peer, msg)).await.is_err()
                {
                    break;
                }
            }
            // Still the cached one (not replaced by a newer dial): the link to `peer` is gone.
            let mut cs = conns.lock().unwrap();
            if cs
                .get(&peer)
                .is_some_and(|c| c.stable_id() == conn.stable_id())
            {
                cs.remove(&peer);
                if let Some(l) = &*lost.lock().unwrap() {
                    let _ = l.send(peer);
                }
            }
        });
    }
}

/// The relay's host accepts a TCP connection within 3 s.
async fn answers(url: &RelayUrl) -> bool {
    let (Some(host), Some(port)) = (url.host_str(), url.port_or_known_default()) else {
        return false;
    };
    let host = host.trim_end_matches('.').to_string();
    let dial = tokio::net::TcpStream::connect((host, port));
    matches!(
        tokio::time::timeout(Duration::from_secs(3), dial).await,
        Ok(Ok(_))
    )
}

async fn write(conn: &Connection, bytes: &[u8]) -> Result<()> {
    let mut s = conn.open_uni().await?;
    s.write_all(bytes).await?;
    s.finish()?;
    // Delivered only once the peer acknowledged all of it.
    s.stopped().await?;
    Ok(())
}

async fn serve_join(
    conn: Connection,
    jtx: mpsc::Sender<(EndpointId, Vec<u8>, oneshot::Sender<Vec<u8>>)>,
) -> Result<()> {
    let (mut s, mut r) = conn.accept_bi().await?;
    let req = r.read_to_end(64 << 10).await?;
    let (tx, rx) = oneshot::channel();
    jtx.send((conn.remote_id(), req, tx)).await?;
    s.write_all(&rx.await?).await?;
    s.finish()?;
    conn.closed().await;
    Ok(())
}
