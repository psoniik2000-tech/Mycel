//! Calls (DESIGN.md §9): signalling as `Call` events in the chat, media on ALPN `mycel/call/1`.
//! Mesh: each pair of participants holds one connection, dialled by the smaller device id.
//! Frames are encoded in the UI (WebCodecs); here they are only encrypted and moved.

use crate::{Chat, Node, hex};
use anyhow::{Context, Result, ensure};
use mycel_net::{CALL_ALPN, Connection, EndpointAddr, EndpointId};
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
    /// The chat whose members take part (the space chat: a group call; a 1:1 chat: just us two).
    chat: Chat,
    /// Frame keys derive from it per sender; an invite carries it to someone outside the chat.
    secret: [u8; 32],
    key: [u8; 32],
    video: bool,
    seq: u64,
    conns: HashMap<EndpointId, CallConn>,
}

/// Connection, its ordered video queue and (sent, lost) packets at the last `call_stats`.
type CallConn = (Connection, mpsc::Sender<Vec<u8>>, (u64, u64));

/// Link quality to one participant (the signal icon in the call).
#[derive(Debug, Clone)]
pub struct LinkStats {
    pub peer: [u8; 32],
    /// Round-trip time of the selected path, ms (QUIC estimate).
    pub rtt_ms: u32,
    /// Share of our packets lost since the previous query.
    pub loss: f64,
    pub relay: bool,
}

impl Node {
    /// Starts a call: sends an Offer to the chat. Others join with `call_accept`.
    pub fn call_offer(self: &Arc<Self>, chat: &Chat, video: bool) -> Result<([u8; 16], Event)> {
        ensure!(self.call.lock().unwrap().is_none(), "already in a call");
        let mut id = [0u8; 16];
        getrandom::fill(&mut id)?;
        Ok((id, self.join_call(id, *chat, CallAction::Offer { video })?))
    }

    /// Invites the members of `chat` (a 1:1 chat or another group) to the call we are in.
    pub fn call_invite(&self, chat: &Chat) -> Result<Event> {
        let (id, secret, video, conns) = {
            let g = self.call.lock().unwrap();
            let c = g.as_ref().context("not in a call")?;
            let conns: Vec<_> = c.conns.keys().map(|p| *p.as_bytes()).collect();
            (c.id, c.secret, c.video, conns)
        };
        // Connected ones and those who joined but are still being dialled.
        let mut all = conns;
        for p in self.call_members(id)? {
            if !all.contains(&p) && p != self.device_id() {
                all.push(p);
            }
        }
        let peers = {
            let st = self.st.lock().unwrap();
            all.iter().filter_map(|p| st.store.peer(p).ok()).collect()
        };
        let action = CallAction::Invite {
            video,
            secret,
            peers,
        };
        self.call_event(id, chat, action)
    }

    pub fn call_accept(self: &Arc<Self>, id: [u8; 16]) -> Result<Event> {
        ensure!(self.call.lock().unwrap().is_none(), "already in a call");
        self.join_call(id, self.call_chat(id)?, CallAction::Accept)
    }

    /// In the call before the event goes out, so a peer dialling us on it is not refused.
    fn join_call(self: &Arc<Self>, id: [u8; 16], chat: Chat, action: CallAction) -> Result<Event> {
        self.enter_call(id, chat)?;
        self.call_event(id, &chat, action).inspect_err(|_| {
            self.call.lock().unwrap().take();
        })
    }

    pub fn call_reject(&self, id: [u8; 16]) -> Result<Event> {
        self.call_event(id, &self.call_chat(id)?, CallAction::Reject)
    }

    /// The chat a call was offered in.
    pub fn call_chat(&self, id: [u8; 16]) -> Result<Chat> {
        self.all_events()?
            .into_iter()
            .find(|(_, e)| matches!(e.body, EventBody::Call { call_id, .. } if call_id == id))
            .map(|(c, _)| c)
            .context("no such call")
    }

    /// Leaves the current call (sends End); no-op outside a call.
    pub fn call_end(&self) -> Result<Option<Event>> {
        let Some(c) = self.call.lock().unwrap().take() else {
            return Ok(None);
        };
        for (conn, ..) in c.conns.values() {
            conn.close(0u32.into(), b"end");
        }
        self.call_event(c.id, &c.chat, CallAction::End).map(Some)
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
        for (conn, video, _) in c.conns.values() {
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

    /// Per-participant RTT and packet loss since the previous query.
    pub fn call_stats(&self) -> Vec<LinkStats> {
        let mut g = self.call.lock().unwrap();
        let Some(c) = g.as_mut() else { return vec![] };
        c.conns
            .iter_mut()
            .map(|(peer, (conn, _, prev))| {
                let st = conn.stats();
                let (ds, dl) = (
                    st.sent_packets.saturating_sub(prev.0),
                    st.lost_packets.saturating_sub(prev.1),
                );
                *prev = (st.sent_packets, st.lost_packets);
                let paths = conn.paths();
                let path = paths.iter().find(|p| p.is_selected());
                LinkStats {
                    peer: *peer.as_bytes(),
                    rtt_ms: path.as_ref().map_or(0, |p| p.rtt().as_millis() as u32),
                    loss: if ds == 0 { 0.0 } else { dl as f64 / ds as f64 },
                    relay: path.is_some_and(|p| p.is_relay()),
                }
            })
            .collect()
    }

    /// Devices in call `id` by the chat history: last action Offer/Accept = in, End/Reject = out.
    pub fn call_members(&self, id: [u8; 16]) -> Result<Vec<[u8; 32]>> {
        let mut last = HashMap::new();
        for (_, e) in self.all_events()? {
            if let EventBody::Call { call_id, action } = e.body
                && call_id == id
            {
                last.insert(e.author_device, action);
            }
        }
        Ok(last
            .into_iter()
            .filter(|(_, a)| {
                matches!(
                    a,
                    CallAction::Offer { .. } | CallAction::Accept | CallAction::Invite { .. }
                )
            })
            .map(|(d, _)| d)
            .collect())
    }

    /// Sends a fresh Invite to every chat we invited into call `id`.
    fn reinvite(&self, id: [u8; 16]) -> Result<()> {
        let me = self.device_id();
        let mut chats = vec![];
        for (chat, e) in self.all_events()? {
            if e.author_device == me
                && matches!(e.body, EventBody::Call { call_id, action: CallAction::Invite { .. } } if call_id == id)
                && !chats.contains(&chat)
            {
                chats.push(chat);
            }
        }
        for chat in chats {
            self.call_invite(&chat)?;
        }
        Ok(())
    }

    /// Reacts to a peer's Call event: dial a newcomer (if we are the smaller id), drop a leaver.
    pub(crate) fn on_call_event(self: &Arc<Self>, ev: &Event) {
        let EventBody::Call { call_id, action } = &ev.body else {
            return;
        };
        let call_id = *call_id;
        let Ok(peer) = EndpointId::from_bytes(&ev.author_device) else {
            return;
        };
        let mut g = self.call.lock().unwrap();
        let Some(c) = g.as_mut().filter(|c| c.id == call_id) else {
            return;
        };
        match action {
            CallAction::Accept => {
                let dial = self.device_id() < ev.author_device && !c.conns.contains_key(&peer);
                drop(g);
                if dial {
                    self.dial(call_id, ev.author_device, None);
                }
                // Those we invited do not see this Accept: tell them who is in now.
                if let Err(e) = self.reinvite(call_id) {
                    eprintln!("mycel: re-invite failed: {e}");
                }
            }
            // A fresh list from the inviter: dial whoever joined since (they do not know us).
            CallAction::Invite { peers, .. } => {
                let me = self.device_id();
                let new: Vec<_> = peers
                    .iter()
                    .filter_map(|a| crate::peer_addr(a))
                    .filter(|a| *a.id.as_bytes() != me && !c.conns.contains_key(&a.id))
                    .collect();
                drop(g);
                for a in new {
                    self.dial(call_id, *a.id.as_bytes(), Some(a));
                }
            }
            CallAction::End | CallAction::Reject => {
                if let Some((conn, ..)) = c.conns.remove(&peer) {
                    conn.close(0u32.into(), b"end");
                }
            }
            _ => {}
        }
    }

    /// The call's secret: from an invite to it, else the MLS exporter of the chat's group (current epoch) over the call id.
    // shortcut: secret from the epoch at join time; a commit mid-call desyncs it only if it lands between two peers' joins.
    fn call_secret(&self, id: [u8; 16], chat: &Chat) -> Result<([u8; 32], bool, Vec<Vec<u8>>)> {
        let mut video = false;
        for (_, e) in self.all_events()? {
            match e.body {
                EventBody::Call {
                    call_id,
                    action:
                        CallAction::Invite {
                            secret,
                            peers,
                            video,
                        },
                } if call_id == id => return Ok((secret, video, peers)),
                EventBody::Call {
                    call_id,
                    action: CallAction::Offer { video: v },
                } if call_id == id => video = v,
                _ => {}
            }
        }
        let st = self.st.lock().unwrap();
        let k = st.mls.export(&st.gid(chat)?, "mycel call", &id, 32)?;
        Ok((k.try_into().expect("32 bytes"), video, vec![]))
    }

    fn enter_call(self: &Arc<Self>, id: [u8; 16], chat: Chat) -> Result<()> {
        let me = self.device_id();
        let mut seq = [0u8; 8];
        // Random start: rejoining the same call must not reuse nonces.
        getrandom::fill(&mut seq)?;
        let (secret, video, invited) = self.call_secret(id, &chat)?;
        *self.call.lock().unwrap() = Some(Call {
            id,
            chat,
            secret,
            key: frame_key(&secret, &me),
            video,
            seq: u64::from_be_bytes(seq) >> 2,
            conns: HashMap::new(),
        });
        let members = self.call_members(id)?;
        for p in &members {
            if me < *p {
                self.dial(id, *p, None);
            }
        }
        // Participants outside our chat never see our Accept: we dial them all.
        for a in invited.iter().filter_map(|a| crate::peer_addr(a)) {
            let p = *a.id.as_bytes();
            if p != me && !members.contains(&p) {
                self.dial(id, p, Some(a));
            }
        }
        Ok(())
    }

    fn dial(self: &Arc<Self>, id: [u8; 16], peer: [u8; 32], addr: Option<EndpointAddr>) {
        let n = self.clone();
        tokio::spawn(async move {
            let addr = match addr {
                Some(a) => a,
                None => {
                    n.st.lock()
                        .unwrap()
                        .store
                        .peer(&peer)
                        .ok()
                        .and_then(|p| crate::peer_addr(&p))
                        .context("peer address unknown")?
                }
            };
            let conn = n.net.connect(addr, CALL_ALPN).await?;
            n.add_call_conn(conn, Some(id))
        });
    }

    /// Keeps a call connection (ours or incoming) and forwards its decrypted frames.
    // Anyone may connect while we are in a call: without the secret they can neither read our frames nor send ones we accept.
    pub(crate) fn add_call_conn(
        self: &Arc<Self>,
        conn: Connection,
        id: Option<[u8; 16]>,
    ) -> Result<()> {
        let peer = conn.remote_id();
        let from = *peer.as_bytes();
        let (call_id, secret) = self
            .call
            .lock()
            .unwrap()
            .as_ref()
            .map(|c| (c.id, c.secret))
            .context("not in a call")?;
        ensure!(id.is_none_or(|i| i == call_id), "call ended");
        let key = frame_key(&secret, &from);
        let (vtx, mut vrx) = mpsc::channel::<Vec<u8>>(60);
        self.call
            .lock()
            .unwrap()
            .as_mut()
            .context("not in a call")?
            .conns
            .insert(peer, (conn.clone(), vtx, (0, 0)));
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
            // Closed: someone outside our chat leaves without an End we would see.
            let mut g = n.call.lock().unwrap();
            if let Some(call) = g.as_mut()
                && call
                    .conns
                    .get(&peer)
                    .is_some_and(|(x, ..)| x.stable_id() == c.stable_id())
            {
                call.conns.remove(&peer);
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

    fn call_event(&self, call_id: [u8; 16], chat: &Chat, action: CallAction) -> Result<Event> {
        self.send_event(chat, EventBody::Call { call_id, action })
    }
}

/// One sender's frame key in a call.
fn frame_key(secret: &[u8; 32], sender: &[u8; 32]) -> [u8; 32] {
    *blake3::keyed_hash(secret, sender).as_bytes()
}
