//! Thin node API: open, send_text, incoming event stream, history.

use anyhow::{Context, Result};
use mycel_crypto::{SecretKey, sign_event, verify_event};
use mycel_net::{EndpointAddr, EndpointId, EndpointTicket, Net};
use mycel_proto::{Envelope, Event, EventBody};
use mycel_store::Store;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;

pub use mycel_proto;

pub struct Node {
    store: Mutex<Store>,
    net: Net,
}

impl Node {
    /// Opens the encrypted store (creating the device key on first run) and binds the network.
    /// Returns the node and a stream of new verified events from peers.
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
        let (net, mut inbox) = Net::bind(key, port, relay).await?;
        let node = Arc::new(Self {
            store: Mutex::new(store),
            net,
        });
        let (tx, rx) = mpsc::channel(256);
        let n = node.clone();
        tokio::spawn(async move {
            while let Some((from, env)) = inbox.recv().await {
                match n.receive(from, &env) {
                    Ok(Some(ev)) => {
                        if tx.send(ev).await.is_err() {
                            break;
                        }
                    }
                    Ok(None) => {}
                    Err(e) => eprintln!("mycel: dropped envelope from {}: {e}", from.fmt_short()),
                }
            }
        });
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

    pub fn add_peer(&self, ticket: &str) -> Result<()> {
        let t: EndpointTicket = ticket.trim().parse()?;
        self.store
            .lock()
            .unwrap()
            .upsert_peer(t.endpoint_addr().id.as_bytes(), t.to_string().as_bytes())
    }

    pub fn history(&self) -> Result<Vec<Event>> {
        let rows = self.store.lock().unwrap().events()?;
        rows.iter().map(|b| Ok(postcard::from_bytes(b)?)).collect()
    }

    /// Signs, stores and sends a text event to every known peer.
    pub async fn send_text(&self, text: &str) -> Result<Event> {
        let (ev, peers) = {
            let store = self.store.lock().unwrap();
            let head = store.head()?;
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
            let data = postcard::to_allocvec(&ev)?;
            store.insert_event(&ev.id, ev.lamport, ev.timestamp_ms, &data)?;
            (ev, store.peers()?)
        };
        let env = Envelope {
            version: 1,
            space_id: [0; 16],
            group_id: [0; 32],
            epoch: 0,
            // shortcut: no E2E, ciphertext is the plain signed Event; MLS in 1b.
            ciphertext: postcard::to_allocvec(&ev)?,
        };
        // shortcut: direct send to each peer, no sync/retry for offline peers; sync in phase 1b+.
        for p in peers {
            let addr: EndpointAddr = std::str::from_utf8(&p)?.parse::<EndpointTicket>()?.into();
            let id = addr.id;
            if let Err(e) = self.net.send(addr, &env).await {
                eprintln!("mycel: send to {} failed: {e}", id.fmt_short());
            }
        }
        Ok(ev)
    }

    pub async fn close(&self) {
        self.net.close().await
    }

    fn receive(&self, from: EndpointId, env: &Envelope) -> Result<Option<Event>> {
        let ev: Event = postcard::from_bytes(&env.ciphertext)?;
        // shortcut: only direct author→peer delivery, so sender must be the author; relax with sync.
        anyhow::ensure!(
            ev.author_device == *from.as_bytes(),
            "author is not the sender"
        );
        anyhow::ensure!(verify_event(&ev), "bad signature");
        let store = self.store.lock().unwrap();
        let new = store.insert_event(
            &ev.id,
            ev.lamport,
            ev.timestamp_ms,
            &postcard::to_allocvec(&ev)?,
        )?;
        if !store.has_peer(from.as_bytes())? {
            let t = EndpointTicket::new(EndpointAddr::new(from));
            store.upsert_peer(from.as_bytes(), t.to_string().as_bytes())?;
        }
        Ok(new.then_some(ev))
    }
}
