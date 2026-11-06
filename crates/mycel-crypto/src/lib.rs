//! Device key and event signing (DESIGN.md §4.1). The device key is the iroh key, so NodeId = device public key.

pub use iroh_base::SecretKey;
use iroh_base::{PublicKey, Signature};
use mycel_proto::Event;

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
}
