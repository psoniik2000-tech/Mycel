//! Wire types (DESIGN.md §14).

use serde::{Deserialize, Serialize};
use std::time::Duration;

pub type Blake3Hash = [u8; 32];
pub type EventId = Blake3Hash;
pub type DeviceId = [u8; 32];
pub type UserId = [u8; 32];
// shortcut: Vec instead of [u8; 64] because serde derives arrays only up to 32; switch to ed25519 Signature in phase 1.
pub type Ed25519Signature = Vec<u8>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    pub version: u8,
    pub space_id: [u8; 16],
    pub group_id: [u8; 32],
    pub epoch: u64,
    pub ciphertext: Vec<u8>,
}

/// Messages on ALPN `mycel/sync/1` (§7.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SyncMsg {
    Envs(Vec<Envelope>),
    /// BLAKE3 ids of the sender's envelopes; the peer answers with the ones missing here.
    // shortcut: full ID list, Negentropy when there's >10k events.
    Have {
        ids: Vec<Blake3Hash>,
        reply: bool,
    },
    /// Member addresses, each signed by its owner (§6.2 peer exchange).
    Peers(Vec<SignedAddr>),
}

/// A node's address (EndpointTicket string), signed by the node key over (ticket, relay, ts_ms).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedAddr {
    pub ticket: String,
    /// URL of the iroh-relay this node serves (anchors only); members use it as a relay.
    pub relay: Option<String>,
    pub ts_ms: u64,
    pub sig: Ed25519Signature,
}

/// Request on ALPN `mycel/join/1` (§4.3); `addr` is the joiner's own address.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinReq {
    pub secret: [u8; 16],
    pub key_package: Vec<u8>,
    pub addr: SignedAddr,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinResp {
    pub welcome: Vec<u8>,
    pub peers: Vec<SignedAddr>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub id: Blake3Hash,
    pub author_device: DeviceId,
    pub parents: Vec<Blake3Hash>,
    pub lamport: u64,
    pub timestamp_ms: u64,
    pub body: EventBody,
    pub signature: Ed25519Signature,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventBody {
    Text {
        text: String,
        reply_to: Option<EventId>,
        mentions: Vec<UserId>,
    },
    Media {
        kind: MediaKind,
        blob: BlobRef,
        caption: Option<String>,
        reply_to: Option<EventId>,
    },
    Edit {
        target: EventId,
        new_text: String,
    },
    Delete {
        target: EventId,
    },
    Reaction {
        target: EventId,
        emoji: String,
        remove: bool,
    },
    Receipt {
        up_to: EventId,
        kind: ReceiptKind,
    },
    Typing {
        active: bool,
    },
    Call {
        call_id: [u8; 16],
        action: CallAction,
    },
    ChatMeta {
        name: Option<String>,
        avatar: Option<BlobRef>,
        disappearing: Option<Duration>,
    },
    Pin {
        target: EventId,
        pinned: bool,
    },
    /// The author's name and avatar (a letter or emoji, a palette color index), sent to the
    /// space chat; `key_package` (last resort) lets any member open a 1:1 chat with them.
    Profile {
        name: String,
        avatar: String,
        color: u8,
        key_package: Vec<u8>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobRef {
    pub hash: Blake3Hash,
    pub size: u64,
    pub key: [u8; 32],
    pub mime: String,
    pub name: Option<String>,
    pub preview: Option<Preview>,
    pub duration_ms: Option<u32>,
    pub dims: Option<(u32, u32)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Preview {
    Blurhash(String),
    Waveform(Vec<u8>),
    Frame(Vec<u8>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MediaKind {
    File,
    Photo,
    Video,
    Voice,
    VideoNote,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReceiptKind {
    Delivered,
    Read,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CallAction {
    Offer { video: bool },
    Accept,
    Reject,
    End,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_roundtrip() {
        let blob = BlobRef {
            hash: [1; 32],
            size: 42,
            key: [2; 32],
            mime: "image/jpeg".into(),
            name: Some("a.jpg".into()),
            preview: Some(Preview::Blurhash("LEHV6n".into())),
            duration_ms: None,
            dims: Some((640, 480)),
        };
        let ev = Event {
            id: [3; 32],
            author_device: [4; 32],
            parents: vec![[5; 32]],
            lamport: 7,
            timestamp_ms: 1_700_000_000_000,
            body: EventBody::Media {
                kind: MediaKind::Photo,
                blob,
                caption: None,
                reply_to: Some([6; 32]),
            },
            signature: vec![9; 64],
        };
        let bytes = postcard::to_allocvec(&ev).unwrap();
        assert_eq!(postcard::from_bytes::<Event>(&bytes).unwrap(), ev);
    }
}
