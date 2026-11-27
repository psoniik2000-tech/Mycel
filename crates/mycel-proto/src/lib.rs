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
    /// "Typing…" / "recording a voice message…" in a group's chat: ephemeral, never stored.
    Typing {
        group_id: [u8; 32],
        voice: bool,
    },
    /// The sender is in the app (`online`) or was `ago` seconds ago; `ago: None` = hidden from
    /// the receiver. A new variant: older apps skip it.
    Presence {
        online: bool,
        ago: Option<u32>,
    },
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
    /// Multi-device (§4.4): the account root lists its devices; a linked device names its
    /// account (`devices` empty). A device belongs to an account only when both agree.
    Account {
        account: UserId,
        devices: Vec<DeviceId>,
    },
    /// A Text or Media message with what older clients cannot read (a new variant: they skip
    /// it). It disappears `ttl_secs` after its `timestamp_ms` (0 = never) and/or was forwarded.
    /// A chat's timer is set by `ChatMeta { disappearing: Some(d) }` (zero = off).
    Extra {
        body: Box<EventBody>,
        ttl_secs: u32,
        forwarded: Option<Forwarded>,
    },
}

/// A sticker is a `Text` whose text is its emoji and whose first mention is this tag plus the id
/// of a picture shipped with the app (`<set>/<name>`), zero-padded: the wire format stays, and
/// older clients (or ones without that set) show the emoji.
const STICKER: &[u8] = b"sticker:";

/// The mention that makes a `Text` a sticker; `None` for an id that does not fit or is unsafe.
pub fn sticker_tag(id: &str) -> Option<UserId> {
    let safe = |c: u8| c.is_ascii_lowercase() || c.is_ascii_digit() || b"-/".contains(&c);
    let mut tag = [0; 32];
    let end = STICKER.len() + id.len();
    if id.is_empty() || end > tag.len() || !id.bytes().all(safe) {
        return None;
    }
    tag[..STICKER.len()].copy_from_slice(STICKER);
    tag[STICKER.len()..end].copy_from_slice(id.as_bytes());
    Some(tag)
}

/// The original author of a forwarded message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Forwarded {
    pub name: String,
    pub device: DeviceId,
}

impl EventBody {
    /// The message inside `Extra`, else itself.
    pub fn msg(&self) -> &EventBody {
        match self {
            EventBody::Extra { body, .. } => body,
            b => b,
        }
    }

    /// The sticker id, when this message is one.
    pub fn sticker(&self) -> Option<&str> {
        let EventBody::Text { text, mentions, .. } = self.msg() else {
            return None;
        };
        let tag = mentions.first()?;
        let id = tag.strip_prefix(STICKER)?.split(|&c| c == 0).next()?;
        let id = std::str::from_utf8(id).ok()?;
        // The text comes from a peer: only an emoji-sized one may hide behind a picture.
        (text.len() <= 32 && sticker_tag(id).as_ref() == Some(tag)).then_some(id)
    }

    pub fn ttl_secs(&self) -> u32 {
        match self {
            EventBody::Extra { ttl_secs, .. } => *ttl_secs,
            _ => 0,
        }
    }

    pub fn forwarded(&self) -> Option<&Forwarded> {
        match self {
            EventBody::Extra { forwarded, .. } => forwarded.as_ref(),
            _ => None,
        }
    }
}

impl Event {
    /// Unix ms when a disappearing message is gone.
    pub fn expires_ms(&self) -> Option<u64> {
        let t = self.body.ttl_secs();
        (t > 0).then(|| self.timestamp_ms.saturating_add(t as u64 * 1000))
    }
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

/// Ties the media sent together into an album. It rides in `mime` as a parameter, so the wire
/// format stays and older clients show each item on its own.
const ALBUM: &str = ";album=";

impl BlobRef {
    pub fn album(&self) -> Option<&str> {
        self.mime.split_once(ALBUM).map(|(_, id)| id)
    }

    /// The mime type without the album parameter.
    pub fn mime_type(&self) -> &str {
        self.mime.split_once(ALBUM).map_or(&self.mime, |(m, _)| m)
    }

    pub fn set_album(&mut self, id: &str) {
        self.mime = format!("{}{ALBUM}{id}", self.mime_type());
    }
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
    fn sticker_tags() {
        let body = |text: &str, tag: Option<UserId>| EventBody::Text {
            text: text.into(),
            reply_to: None,
            mentions: tag.into_iter().collect(),
        };
        let tag = sticker_tag("f3d/2764-200d-1f525");
        assert_eq!(body("❤️", tag).sticker(), Some("f3d/2764-200d-1f525"));
        assert_eq!(body(&"x".repeat(33), tag).sticker(), None);
        assert_eq!(body("❤️", None).sticker(), None);
        assert_eq!(body("❤️", Some([7; 32])).sticker(), None);
        for bad in ["", "../x", "A", "f3d/0123456789012345678901"] {
            assert_eq!(sticker_tag(bad), None, "{bad}");
        }
        // Junk after the zero padding is not a sticker either.
        let mut junk = tag.unwrap();
        junk[31] = 1;
        assert_eq!(body("❤️", Some(junk)).sticker(), None);
    }

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
        // The album tag changes no field of the wire format and replaces an older one.
        let mut tagged = blob.clone();
        assert_eq!(tagged.album(), None);
        tagged.set_album("a1");
        tagged.set_album("b2");
        assert_eq!(tagged.mime, "image/jpeg;album=b2");
        assert_eq!(
            (tagged.mime_type(), tagged.album()),
            ("image/jpeg", Some("b2"))
        );
        let ev = Event {
            id: [3; 32],
            author_device: [4; 32],
            parents: vec![[5; 32]],
            lamport: 7,
            timestamp_ms: 1_700_000_000_000,
            body: EventBody::Media {
                kind: MediaKind::Photo,
                blob: tagged,
                caption: None,
                reply_to: Some([6; 32]),
            },
            signature: vec![9; 64],
        };
        let bytes = postcard::to_allocvec(&ev).unwrap();
        assert_eq!(postcard::from_bytes::<Event>(&bytes).unwrap(), ev);
    }
}
