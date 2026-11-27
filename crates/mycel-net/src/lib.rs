//! iroh transport (DESIGN.md §6.1 T1–T2, §14.3): Envelopes over ALPN `mycel/sync/1`.

use anyhow::Result;
use iroh::endpoint::presets;
use iroh::{Endpoint, RelayConfig, RelayMap, RelayMode, TransportAddr};
use mycel_proto::SyncMsg;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
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
const ACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

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
        Ok((net, rx, jrx, brx))
    }

    /// Adds a relay to the relay map; the endpoint picks the best one as its home relay.
    /// Once a space has its own relay (an anchor or an added server), the public n0 relays
    /// are dropped, so every member meets there; n0 address lookup stays.
    pub async fn add_relay(&self, url: &str) -> Result<()> {
        let url: RelayUrl = url.parse()?;
        let cfg = Arc::new(RelayConfig::new(url.clone(), None));
        let ours = {
            let mut rs = self.relays.lock().unwrap();
            rs.push(url.clone());
            rs.clone()
        };
        self.ep.insert_relay(url, cfg).await;
        let n0: Vec<RelayUrl> = RelayMode::Default.relay_map().urls();
        for u in n0.iter().filter(|u| !ours.contains(u)) {
            self.ep.remove_relay(u).await;
        }
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
        });
    }
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
