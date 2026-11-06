use mycel_core::Node;
use mycel_core::mycel_proto::{Event, EventBody};
use std::time::Duration;
use tokio::sync::mpsc::Receiver;

fn text(ev: &Event) -> &str {
    match &ev.body {
        EventBody::Text { text, .. } => text,
        _ => panic!("not text"),
    }
}

async fn recv(rx: &mut Receiver<Event>) -> Event {
    tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("timeout")
        .expect("closed")
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
    let (a, mut rx_a) = Node::open(dirs[0].path(), "pa", 0, false).await.unwrap();
    let (b, mut rx_b) = Node::open(dirs[1].path(), "pb", 0, false).await.unwrap();
    let (c, _rx_c) = Node::open(dirs[2].path(), "pc", 0, false).await.unwrap();
    a.create_space().unwrap();

    let link = a.invite(false).await.unwrap();
    assert!(link.starts_with("mycel://join/"));
    b.join(&link).await.unwrap();
    // One-time secret: a second use is refused.
    let (d, _rx_d) = Node::open(dirs[3].path(), "pd", 0, false).await.unwrap();
    assert!(d.join(&link).await.is_err());
    d.close().await;

    c.join(&a.invite(false).await.unwrap()).await.unwrap();
    assert_eq!(c.members().len(), 3);
    until(|| b.members().len() == 3).await;

    // C goes offline; A and B chat.
    let c_id = c.device_id();
    c.close().await;
    drop(c);
    a.send_text("hello from a").unwrap();
    assert_eq!(text(&recv(&mut rx_b).await), "hello from a");
    b.send_text("hello from b").unwrap();
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
    let (c, mut rx_c) = Node::open(dirs[2].path(), "pc", 0, false).await.unwrap();
    assert_eq!(c.device_id(), c_id);
    let mut got = [recv(&mut rx_c).await, recv(&mut rx_c).await];
    got.sort_by_key(|e| e.lamport);
    assert_eq!(text(&got[0]), "hello from a");
    assert_eq!(text(&got[1]), "hello from b");
    assert_eq!(got[0].author_device, a.device_id());

    // MLS keeps working after the restart.
    c.send_text("c is back").unwrap();
    let ev = recv(&mut rx_b).await;
    assert_eq!(text(&ev), "c is back");
    assert_eq!(ev.lamport, 3);
    let hist: Vec<_> = c.history().unwrap();
    assert_eq!(
        hist.iter().map(text).collect::<Vec<_>>(),
        ["hello from a", "hello from b", "c is back"]
    );

    b.close().await;
    c.close().await;
}
