//! iroh transport (DESIGN.md §6.1 T1–T2, §14.3): Envelopes over ALPN `mycel/sync/1`.

use anyhow::Result;
use iroh::endpoint::{Connection, presets};
use iroh::{Endpoint, RelayConfig, RelayMap, RelayMode};
use mycel_proto::SyncMsg;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};

pub use iroh::{EndpointAddr, EndpointId, RelayUrl, SecretKey};
pub use iroh_tickets::endpoint::EndpointTicket;

pub const ALPN: &[u8] = b"mycel/sync/1";
pub const JOIN_ALPN: &[u8] = b"mycel/join/1";
// shortcut: one message per stream, capped at 16 MiB (a full-history Envs batch); chunk when histories grow.
const MAX_MSG: usize = 16 << 20;

/// Incoming sync messages with the authenticated sender.
pub type Inbox = mpsc::Receiver<(EndpointId, SyncMsg)>;
/// Incoming join requests: (sender, request bytes, reply). Dropping the reply refuses.
pub type JoinInbox = mpsc::Receiver<(EndpointId, Vec<u8>, oneshot::Sender<Vec<u8>>)>;

#[derive(Clone)]
pub struct Net {
    ep: Endpoint,
    conns: Arc<Mutex<HashMap<EndpointId, Connection>>>,
    tx: mpsc::Sender<(EndpointId, SyncMsg)>,
}

impl Net {
    /// Relays are our anchors' iroh-relay URLs (§6.1 T2); more can be added with `add_relay`.
    /// `n0 = true` also uses the public n0 relays and address lookup (last resort). `port = 0`: any free port.
    pub async fn bind(
        key: SecretKey,
        port: u16,
        relays: &[String],
        n0: bool,
    ) -> Result<(Self, Inbox, JoinInbox)> {
        let mut b = if n0 {
            Endpoint::builder(presets::N0)
        } else {
            Endpoint::builder(presets::Minimal).relay_mode(RelayMode::Custom(RelayMap::empty()))
        };
        b = b
            .secret_key(key)
            .alpns(vec![ALPN.to_vec(), JOIN_ALPN.to_vec()]);
        if port != 0 {
            b = b
                .clear_ip_transports()
                .bind_addr(SocketAddr::from(([0, 0, 0, 0], port)))?;
        }
        let ep = b.bind().await?;
        let (tx, rx) = mpsc::channel(256);
        let (jtx, jrx) = mpsc::channel(16);
        let net = Self {
            ep,
            conns: Default::default(),
            tx,
        };
        let n = net.clone();
        tokio::spawn(async move {
            while let Some(incoming) = n.ep.accept().await {
                let Ok(conn) = incoming.await else { continue };
                if conn.alpn() == JOIN_ALPN {
                    tokio::spawn(serve_join(conn, jtx.clone()));
                } else {
                    n.track(conn);
                }
            }
        });
        for r in relays {
            net.add_relay(r).await?;
        }
        Ok((net, rx, jrx))
    }

    /// Adds a relay to the relay map; the endpoint picks the best one as its home relay.
    pub async fn add_relay(&self, url: &str) -> Result<()> {
        let url: RelayUrl = url.parse()?;
        let cfg = Arc::new(RelayConfig::new(url.clone(), None));
        self.ep.insert_relay(url, cfg).await;
        Ok(())
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
        if let Some(conn) = cached
            && write(&conn, &bytes).await.is_ok()
        {
            return Ok(());
        }
        let conn = self.ep.connect(to, ALPN).await?;
        self.track(conn.clone());
        write(&conn, &bytes).await
    }

    /// One request/response on `mycel/join/1`.
    pub async fn join(&self, to: EndpointAddr, req: &[u8]) -> Result<Vec<u8>> {
        let conn = self.ep.connect(to, JOIN_ALPN).await?;
        let (mut s, mut r) = conn.open_bi().await?;
        s.write_all(req).await?;
        s.finish()?;
        let resp = r.read_to_end(MAX_MSG).await?;
        conn.close(0u32.into(), b"");
        Ok(resp)
    }

    pub async fn close(&self) {
        self.ep.close().await
    }

    /// Number of peers with an open sync connection.
    pub fn connected(&self) -> usize {
        let conns = self.conns.lock().unwrap();
        conns
            .values()
            .filter(|c| c.close_reason().is_none())
            .count()
    }

    /// Caches the connection for replies and reads envelopes from it until it closes.
    fn track(&self, conn: Connection) {
        let peer = conn.remote_id();
        self.conns.lock().unwrap().insert(peer, conn.clone());
        let tx = self.tx.clone();
        tokio::spawn(async move {
            while let Ok(mut rs) = conn.accept_uni().await {
                let Ok(bytes) = rs.read_to_end(MAX_MSG).await else {
                    continue;
                };
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
