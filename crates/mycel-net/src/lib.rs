//! iroh transport (DESIGN.md §6.1 T1–T2, §14.3): Envelopes over ALPN `mycel/sync/1`.

use anyhow::Result;
use iroh::Endpoint;
use iroh::endpoint::{Connection, presets};
use mycel_proto::Envelope;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

pub use iroh::{EndpointAddr, EndpointId, SecretKey};
pub use iroh_tickets::endpoint::EndpointTicket;

pub const ALPN: &[u8] = b"mycel/sync/1";
const MAX_ENVELOPE: usize = 1 << 20;

/// Incoming envelopes with the authenticated sender.
pub type Inbox = mpsc::Receiver<(EndpointId, Envelope)>;

#[derive(Clone)]
pub struct Net {
    ep: Endpoint,
    conns: Arc<Mutex<HashMap<EndpointId, Connection>>>,
    tx: mpsc::Sender<(EndpointId, Envelope)>,
}

impl Net {
    /// `relay = false`: no relay, no address lookup (LAN / tests). `port = 0`: any free port.
    pub async fn bind(key: SecretKey, port: u16, relay: bool) -> Result<(Self, Inbox)> {
        // shortcut: default n0 relay and discovery; replace with our own iroh-relay on anchors in phase 2.
        let mut b = if relay {
            Endpoint::builder(presets::N0)
        } else {
            Endpoint::builder(presets::Minimal)
        };
        b = b.secret_key(key).alpns(vec![ALPN.to_vec()]);
        if port != 0 {
            b = b
                .clear_ip_transports()
                .bind_addr(SocketAddr::from(([0, 0, 0, 0], port)))?;
        }
        let ep = b.bind().await?;
        let (tx, rx) = mpsc::channel(256);
        let net = Self {
            ep,
            conns: Default::default(),
            tx,
        };
        let n = net.clone();
        tokio::spawn(async move {
            while let Some(incoming) = n.ep.accept().await {
                if let Ok(conn) = incoming.await {
                    n.track(conn);
                }
            }
        });
        Ok((net, rx))
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

    /// Waits until the relay is connected, so `addr()` includes it.
    pub async fn online(&self) {
        self.ep.online().await
    }

    /// One uni stream per envelope; the stream end is the frame boundary.
    pub async fn send(&self, to: EndpointAddr, env: &Envelope) -> Result<()> {
        let bytes = postcard::to_allocvec(env)?;
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

    pub async fn close(&self) {
        self.ep.close().await
    }

    /// Caches the connection for replies and reads envelopes from it until it closes.
    fn track(&self, conn: Connection) {
        let peer = conn.remote_id();
        self.conns.lock().unwrap().insert(peer, conn.clone());
        let tx = self.tx.clone();
        tokio::spawn(async move {
            while let Ok(mut rs) = conn.accept_uni().await {
                let Ok(bytes) = rs.read_to_end(MAX_ENVELOPE).await else {
                    continue;
                };
                if let Ok(env) = postcard::from_bytes::<Envelope>(&bytes)
                    && tx.send((peer, env)).await.is_err()
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
