//! Calls (DESIGN.md §9): signalling as `Call` events in the chat, media on ALPN `mycel/call/1`.
//! Mesh: each pair of participants holds one connection, dialled by the smaller device id.
//! Frames are encoded in the UI (WebCodecs); here they are only encrypted and moved.

use crate::{Node, hex};
use anyhow::{Context, Result, ensure};
use mycel_net::{CALL_ALPN, Connection, EndpointId};
use mycel_proto::{CallAction, Event, EventBody};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Frame kinds: audio goes as a datagram (loss is fine), video as a reliable stream per frame.
pub const AUDIO: u8 = 0;
pub const VIDEO_KEY: u8 = 1;
pub const VIDEO_DELTA: u8 = 2;
const MAX_FRAME: usize = 4 << 20;

/// A decrypted frame from a call participant.
#[derive(Debug, Clone)]
pub struct Frame {
    pub from: [u8; 32],
    pub kind: u8,
    pub seq: u64,
    pub data: Vec<u8>,
}

pub(crate) struct Call {
    id: [u8; 16],
    key: [u8; 32],
    seq: u64,
    /// Connection and its ordered video queue per participant.
    conns: HashMap<EndpointId, (Connection, mpsc::Sender<Vec<u8>>)>,
    /// (sent, lost) packets at the last `call_loss`.
    stats: (u64, u64),
}

impl Node {
    /// Starts a call: sends an Offer to the chat. Others join with `call_accept`.
    pub fn call_offer(self: &Arc<Self>, video: bool) -> Result<([u8; 16], Event)> {
        ensure!(self.call.lock().unwrap().is_none(), "already in a call");
        let mut id = [0u8; 16];
        getrandom::fill(&mut id)?;
        Ok((id, self.join_call(id, CallAction::Offer { video })?))
    }

    pub fn call_accept(self: &Arc<Self>, id: [u8; 16]) -> Result<Event> {
        ensure!(self.call.lock().unwrap().is_none(), "already in a call");
        self.join_call(id, CallAction::Accept)
    }

    /// In the call before the event goes out, so a peer dialling us on it is not refused.
    fn join_call(self: &Arc<Self>, id: [u8; 16], action: CallAction) -> Result<Event> {
        self.enter_call(id)?;
        self.call_event(id, action).inspect_err(|_| {
            self.call.lock().unwrap().take();
        })
    }

    pub fn call_reject(&self, id: [u8; 16]) -> Result<Event> {
        self.call_event(id, CallAction::Reject)
    }

    /// Leaves the current call (sends End); no-op outside a call.
    pub fn call_end(&self) -> Result<Option<Event>> {
        let Some(c) = self.call.lock().unwrap().take() else {
            return Ok(None);
        };
        for (conn, _) in c.conns.values() {
            conn.close(0u32.into(), b"end");
        }
        self.call_event(c.id, CallAction::End).map(Some)
    }

    /// Id of the call we are in.
    pub fn call_id(&self) -> Option<[u8; 16]> {
        self.call.lock().unwrap().as_ref().map(|c| c.id)
    }

    /// Encrypts a frame and sends it to every connected participant.
    pub fn call_send(&self, kind: u8, data: &[u8]) -> Result<()> {
        let mut g = self.call.lock().unwrap();
        let c = g.as_mut().context("not in a call")?;
        c.seq += 1;
        let pkt = mycel_media::seal_frame(&c.key, kind, c.seq, data);
        for (conn, video) in c.conns.values() {
            if kind == AUDIO {
                let _ = conn.send_datagram(pkt.clone().into());
            } else {
                // shortcut: a full queue drops the frame (artifacts until the next key frame, ≤2 s); ask for a key frame if that shows.
                let _ = video.try_send(pkt.clone());
            }
        }
        Ok(())
    }

    /// Frames of the current and later calls go to the returned receiver (replacing an older one).
    pub fn call_frames(&self) -> mpsc::Receiver<Frame> {
        let (tx, rx) = mpsc::channel(512);
        *self.frames.lock().unwrap() = Some(tx);
        rx
    }

    /// Share of packets lost since the previous query, worst link (simple bitrate adaptation).
    pub fn call_loss(&self) -> f64 {
        let mut g = self.call.lock().unwrap();
        let Some(c) = g.as_mut() else { return 0.0 };
        let (sent, lost) = c.conns.values().fold((0, 0), |(s, l), (conn, _)| {
            let st = conn.stats();
            (s + st.udp_tx.datagrams, l + st.lost_packets)
        });
        let (ds, dl) = (
            sent.saturating_sub(c.stats.0),
            lost.saturating_sub(c.stats.1),
        );
        c.stats = (sent, lost);
        if ds == 0 { 0.0 } else { dl as f64 / ds as f64 }
    }

    /// Devices in call `id` by the chat history: last action Offer/Accept = in, End/Reject = out.
    pub fn call_members(&self, id: [u8; 16]) -> Result<Vec<[u8; 32]>> {
        let mut last = HashMap::new();
        for e in self.history()? {
            if let EventBody::Call { call_id, action } = e.body
                && call_id == id
            {
                last.insert(e.author_device, action);
            }
        }
        Ok(last
            .into_iter()
            .filter(|(_, a)| matches!(a, CallAction::Offer { .. } | CallAction::Accept))
            .map(|(d, _)| d)
            .collect())
    }

    /// Reacts to a peer's Call event: dial a newcomer (if we are the smaller id), drop a leaver.
    pub(crate) fn on_call_event(self: &Arc<Self>, ev: &Event) {
        let EventBody::Call { call_id, action } = ev.body else {
            return;
        };
        let Ok(peer) = EndpointId::from_bytes(&ev.author_device) else {
            return;
        };
        let mut g = self.call.lock().unwrap();
        let Some(c) = g.as_mut().filter(|c| c.id == call_id) else {
            return;
        };
        match action {
            CallAction::Accept if self.device_id() < ev.author_device => {
                if !c.conns.contains_key(&peer) {
                    drop(g);
                    self.dial(call_id, ev.author_device);
                }
            }
            CallAction::End | CallAction::Reject => {
                if let Some((conn, _)) = c.conns.remove(&peer) {
                    conn.close(0u32.into(), b"end");
                }
            }
            _ => {}
        }
    }

    /// Per-sender frame key: MLS exporter of the current epoch over (call id, sender).
    // shortcut: key from the epoch at connect time; a commit mid-call desyncs keys only if it lands between two peers' connects.
    fn call_key(&self, id: [u8; 16], sender: &[u8; 32]) -> Result<[u8; 32]> {
        let k =
            self.st
                .lock()
                .unwrap()
                .mls
                .export("mycel call", &[&id[..], sender].concat(), 32)?;
        Ok(k.try_into().expect("32 bytes"))
    }

    fn enter_call(self: &Arc<Self>, id: [u8; 16]) -> Result<()> {
        let me = self.device_id();
        let mut seq = [0u8; 8];
        // Random start: rejoining the same call must not reuse nonces.
        getrandom::fill(&mut seq)?;
        *self.call.lock().unwrap() = Some(Call {
            id,
            key: self.call_key(id, &me)?,
            seq: u64::from_be_bytes(seq) >> 2,
            conns: HashMap::new(),
            stats: (0, 0),
        });
        for p in self.call_members(id)? {
            if me < p {
                self.dial(id, p);
            }
        }
        Ok(())
    }

    fn dial(self: &Arc<Self>, id: [u8; 16], peer: [u8; 32]) {
        let n = self.clone();
        tokio::spawn(async move {
            let addr =
                n.st.lock()
                    .unwrap()
                    .store
                    .peer(&peer)
                    .ok()
                    .and_then(|p| crate::peer_addr(&p))
                    .context("peer address unknown")?;
            let conn = n.net.connect(addr, CALL_ALPN).await?;
            n.add_call_conn(conn, Some(id))
        });
    }

    /// Keeps a call connection (ours or incoming) and forwards its decrypted frames.
    // shortcut: any space member may connect while we are in a call; they could derive the keys anyway (same MLS group).
    pub(crate) fn add_call_conn(
        self: &Arc<Self>,
        conn: Connection,
        id: Option<[u8; 16]>,
    ) -> Result<()> {
        let peer = conn.remote_id();
        let from = *peer.as_bytes();
        ensure!(self.members().contains(&from), "not a member");
        let call_id = self.call_id().context("not in a call")?;
        ensure!(id.is_none_or(|i| i == call_id), "call ended");
        let key = self.call_key(call_id, &from)?;
        let (vtx, mut vrx) = mpsc::channel::<Vec<u8>>(60);
        self.call
            .lock()
            .unwrap()
            .as_mut()
            .context("not in a call")?
            .conns
            .insert(peer, (conn.clone(), vtx));
        // Video frames in order, one uni stream each (the receiver reads streams in order).
        let c = conn.clone();
        tokio::spawn(async move {
            while let Some(pkt) = vrx.recv().await {
                let mut s = c.open_uni().await?;
                s.write_all(&pkt).await?;
                s.finish()?;
            }
            anyhow::Ok(())
        });
        let n = self.clone();
        let c = conn.clone();
        tokio::spawn(async move {
            while let Ok(d) = c.read_datagram().await {
                n.deliver(&key, from, &d).await;
            }
        });
        let n = self.clone();
        tokio::spawn(async move {
            while let Ok(mut s) = conn.accept_uni().await {
                if let Ok(d) = s.read_to_end(MAX_FRAME).await {
                    n.deliver(&key, from, &d).await;
                }
            }
        });
        Ok(())
    }

    async fn deliver(&self, key: &[u8; 32], from: [u8; 32], pkt: &[u8]) {
        let Ok((kind, seq, data)) = mycel_media::open_frame(key, pkt) else {
            eprintln!("mycel: undecryptable call frame from {}", hex(&from[..4]));
            return;
        };
        let tx = self.frames.lock().unwrap().clone();
        if let Some(tx) = tx {
            let _ = tx
                .send(Frame {
                    from,
                    kind,
                    seq,
                    data,
                })
                .await;
        }
    }

    fn call_event(&self, call_id: [u8; 16], action: CallAction) -> Result<Event> {
        self.send_event(EventBody::Call { call_id, action })
    }
}
