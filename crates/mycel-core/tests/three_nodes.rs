use mycel_core::mycel_proto::{Event, EventBody};
use mycel_core::{Node, Opts};
use std::time::Duration;
use tokio::sync::mpsc::Receiver;

fn text(ev: &Event) -> &str {
    match &ev.body {
        EventBody::Text { text, .. } => text,
        _ => panic!("not text"),
    }
}

/// Next text event (receipts are skipped).
async fn recv(rx: &mut Receiver<Event>) -> Event {
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("timeout")
            .expect("closed");
        if matches!(ev.body, EventBody::Text { .. }) {
            return ev;
        }
    }
}

async fn open(dir: &tempfile::TempDir, pw: &str) -> (std::sync::Arc<Node>, Receiver<Event>) {
    Node::open(dir.path(), pw, &Opts::default()).await.unwrap()
}

/// Waits until `cond` holds (gossip and commits arrive asynchronously).
async fn until(cond: impl Fn() -> bool) {
    for _ in 0..100 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("condition not reached");
}

#[tokio::test(flavor = "multi_thread")]
async fn invite_join_encrypt_sync_restart() {
    let dirs: Vec<_> = (0..4).map(|_| tempfile::tempdir().unwrap()).collect();
    let (a, mut rx_a) = open(&dirs[0], "pa").await;
    let (b, mut rx_b) = open(&dirs[1], "pb").await;
    let (c, _rx_c) = open(&dirs[2], "pc").await;
    a.create_space().unwrap();

    let link = a.invite().await.unwrap();
    assert!(link.starts_with("mycel://join/"));
    // Pasted from a messenger: wrapped and padded with spaces.
    let (head, tail) = link.split_at(30);
    b.join(&format!(
        "  {head}
 {tail} 
"
    ))
    .await
    .unwrap();
    // One-time secret: a second use is refused, and says so.
    let (d, _rx_d) = open(&dirs[3], "pd").await;
    let e = d.join(&link).await.unwrap_err().to_string();
    assert!(e.starts_with(mycel_core::JOIN_USED), "{e}");
    let e = d.join("mycel://join/xyz").await.unwrap_err().to_string();
    assert!(e.starts_with(mycel_core::JOIN_BAD_LINK), "{e}");
    d.close().await;

    c.join(&a.invite().await.unwrap()).await.unwrap();
    assert_eq!(c.members().len(), 3);
    until(|| b.members().len() == 3).await;

    // C goes offline; A and B chat.
    let c_id = c.device_id();
    c.close().await;
    drop(c);
    a.send_text(&None, "hello from a", None).unwrap();
    assert_eq!(text(&recv(&mut rx_b).await), "hello from a");
    b.send_text(&None, "hello from b", None).unwrap();
    assert_eq!(text(&recv(&mut rx_a).await), "hello from b");

    // Envelopes are MLS ciphertext: no plaintext in what is stored and forwarded.
    a.close().await;
    for (_, env) in mycel_store::Store::open(dirs[0].path(), "pa")
        .unwrap()
        .envelopes()
        .unwrap()
    {
        assert!(!env.windows(5).any(|w| w == b"hello"));
    }

    // C restarts (MLS state from SQLCipher) and gets both messages from B alone, A is offline.
    let (c, mut rx_c) = open(&dirs[2], "pc").await;
    assert_eq!(c.device_id(), c_id);
    let mut got = [recv(&mut rx_c).await, recv(&mut rx_c).await];
    got.sort_by_key(|e| e.lamport);
    assert_eq!(text(&got[0]), "hello from a");
    assert_eq!(text(&got[1]), "hello from b");
    assert_eq!(got[0].author_device, a.device_id());

    // MLS keeps working after the restart.
    c.send_text(&None, "c is back", None).unwrap();
    let ev = recv(&mut rx_b).await;
    assert_eq!(text(&ev), "c is back");
    let hist: Vec<_> = c.history(&None).unwrap();
    assert_eq!(
        hist.iter()
            .filter(|e| matches!(e.body, EventBody::Text { .. }))
            .map(text)
            .collect::<Vec<_>>(),
        ["hello from a", "hello from b", "c is back"]
    );

    b.close().await;
    c.close().await;
}

/// §7.3–7.4: A writes while C is offline; only the anchor stays up; C gets it from the anchor;
/// A sees ✓ (B and the anchor have it), then ✓✓ once C's receipt reaches A via the anchor.
#[tokio::test(flavor = "multi_thread")]
async fn anchor_store_and_forward_receipts() {
    use iroh_relay::server::{RelayConfig, Server, ServerConfig};
    // Our own relay on localhost, plain HTTP; no n0 anywhere.
    let mut cfg = ServerConfig::default();
    cfg.relay = Some(RelayConfig::new(([127, 0, 0, 1], 0)));
    let relay = Server::spawn(cfg).await.unwrap();
    let url = format!("http://{}", relay.http_addr().unwrap());

    let dirs: Vec<_> = (0..4).map(|_| tempfile::tempdir().unwrap()).collect();
    let anchor = Opts {
        own_relay: Some(url.clone()),
        ..Default::default()
    };
    let (x, _rx_x) = Node::open(dirs[3].path(), "px", &anchor).await.unwrap();
    let (a, _rx_a) = open(&dirs[0], "pa").await;
    let (b, mut rx_b) = open(&dirs[1], "pb").await;
    let (c, _rx_c) = open(&dirs[2], "pc").await;
    a.create_space().unwrap();
    x.join(&a.invite().await.unwrap()).await.unwrap();
    // The admin learned the anchor's relay from its signed address; invites now carry it.
    assert_eq!(a.relays(), [url.as_str()]);
    b.join(&a.invite().await.unwrap()).await.unwrap();
    c.join(&a.invite().await.unwrap()).await.unwrap();
    assert_eq!(c.relays(), [url.as_str()]);
    until(|| [&a, &b, &x].iter().all(|n| n.members().len() == 4)).await;

    c.close().await;
    drop(c);
    let ev = a.send_text(&None, "for c", None).unwrap();
    assert_eq!(text(&recv(&mut rx_b).await), "for c");
    let status = |n: &Node| {
        let s = n.statuses(&None).unwrap();
        s.iter().find(|(id, _)| *id == ev.id).unwrap().1
    };
    until(|| status(&a) == 1).await;

    // Only the anchor stays online.
    a.close().await;
    drop(a);
    b.close().await;
    drop(b);
    let (c, mut rx_c) = open(&dirs[2], "pc").await;
    assert_eq!(text(&recv(&mut rx_c).await), "for c");

    let (a, _rx_a) = open(&dirs[0], "pa").await;
    until(|| status(&a) == 2).await;

    for n in [a, c, x] {
        n.close().await;
    }
}

/// A phone in another network: the invite's ticket has only the relay, no direct address.
#[tokio::test(flavor = "multi_thread")]
async fn join_through_relay_only() {
    use data_encoding::BASE32_NOPAD;
    use iroh_relay::server::{RelayConfig, Server, ServerConfig};
    use mycel_net::{EndpointAddr, EndpointTicket};
    let mut cfg = ServerConfig::default();
    cfg.relay = Some(RelayConfig::new(([127, 0, 0, 1], 0)));
    let relay = Server::spawn(cfg).await.unwrap();
    let url = format!("http://{}", relay.http_addr().unwrap());

    let dirs: Vec<_> = (0..2).map(|_| tempfile::tempdir().unwrap()).collect();
    let pc = Opts {
        own_relay: Some(url.clone()),
        ..Default::default()
    };
    let (a, _rx_a) = Node::open(dirs[0].path(), "pa", &pc).await.unwrap();
    let (b, _rx_b) = open(&dirs[1], "pb").await;
    a.create_space().unwrap();

    let link = a.invite().await.unwrap();
    let raw = BASE32_NOPAD
        .decode(link.strip_prefix("mycel://join/").unwrap().as_bytes())
        .unwrap();
    let (gid, secret, ticket, relays): ([u8; 32], [u8; 16], String, Vec<String>) =
        postcard::from_bytes(&raw).unwrap();
    let full: EndpointAddr = ticket.parse::<EndpointTicket>().unwrap().into();
    let only_relay = EndpointAddr::new(full.id).with_relay_url(url.parse().unwrap());
    let bytes = postcard::to_allocvec(&(
        gid,
        secret,
        EndpointTicket::new(only_relay).to_string(),
        relays,
    ))
    .unwrap();
    b.join(&format!("mycel://join/{}", BASE32_NOPAD.encode(&bytes)))
        .await
        .unwrap();
    until(|| a.members().len() == 2).await;
    a.close().await;
    b.close().await;
}
