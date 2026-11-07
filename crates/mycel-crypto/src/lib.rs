//! Device key and event signing (DESIGN.md §4.1). The device key is the iroh key, so NodeId = device public key.

pub mod mls;

pub use iroh_base::SecretKey;
use iroh_base::{PublicKey, Signature};
use mycel_proto::{Event, SignedAddr};

/// Sets author, id (BLAKE3 of the other fields) and signature.
pub fn sign_event(key: &SecretKey, mut ev: Event) -> Event {
    ev.author_device = *key.public().as_bytes();
    ev.id = event_id(&ev);
    ev.signature = key.sign(&ev.id).to_bytes().to_vec();
    ev
}

/// Checks that id matches the content and is signed by `author_device`.
pub fn verify_event(ev: &Event) -> bool {
    let (Ok(pk), Ok(sig)) = (
        PublicKey::from_bytes(&ev.author_device),
        <[u8; 64]>::try_from(ev.signature.as_slice()),
    ) else {
        return false;
    };
    ev.id == event_id(ev) && pk.verify(&ev.id, &Signature::from_bytes(&sig)).is_ok()
}

/// Signs this node's address with its key (§6.2).
pub fn sign_addr(key: &SecretKey, ticket: String, relay: Option<String>, ts_ms: u64) -> SignedAddr {
    let mut a = SignedAddr {
        ticket,
        relay,
        ts_ms,
        sig: vec![],
    };
    a.sig = key.sign(&addr_msg(&a)).to_bytes().to_vec();
    a
}

/// Checks the signature against `owner` (the id inside the ticket).
pub fn verify_addr(a: &SignedAddr, owner: &[u8; 32]) -> bool {
    let (Ok(pk), Ok(sig)) = (
        PublicKey::from_bytes(owner),
        <[u8; 64]>::try_from(a.sig.as_slice()),
    ) else {
        return false;
    };
    pk.verify(&addr_msg(a), &Signature::from_bytes(&sig))
        .is_ok()
}

fn addr_msg(a: &SignedAddr) -> Vec<u8> {
    postcard::to_allocvec(&(b"mycel-addr", &a.ticket, &a.relay, a.ts_ms)).expect("addr serializes")
}

/// Content id of an envelope, used by sync.
pub fn envelope_id(env: &mycel_proto::Envelope) -> [u8; 32] {
    blake3::hash(&postcard::to_allocvec(env).expect("envelope serializes")).into()
}

fn event_id(ev: &Event) -> [u8; 32] {
    let mut e = ev.clone();
    e.id = [0; 32];
    e.signature.clear();
    blake3::hash(&postcard::to_allocvec(&e).expect("event serializes")).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use mycel_proto::EventBody;

    #[test]
    fn sign_verify_tamper() {
        let ev = Event {
            id: [0; 32],
            author_device: [0; 32],
            parents: vec![],
            lamport: 1,
            timestamp_ms: 0,
            body: EventBody::Typing { active: true },
            signature: vec![],
        };
        let mut ev = sign_event(&SecretKey::generate(), ev);
        assert!(verify_event(&ev));
        ev.lamport = 2;
        assert!(!verify_event(&ev));
    }

    #[test]
    fn addr_sign_verify() {
        let k = SecretKey::generate();
        let mut a = sign_addr(&k, "t".into(), None, 5);
        assert!(verify_addr(&a, k.public().as_bytes()));
        a.relay = Some("http://evil".into());
        assert!(!verify_addr(&a, k.public().as_bytes()));
    }
}
