use mycel_core::mycel_proto::{BlobRef, Event, MediaKind};
use mycel_core::{Node, Opts, media};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::mpsc::Receiver;

/// Next Media event (text and receipts are skipped).
async fn recv_media(rx: &mut Receiver<Event>) -> Event {
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(20), rx.recv())
            .await
            .expect("timeout")
            .expect("closed");
        if media(&ev).is_some() {
            return ev;
        }
    }
}

async fn until_blob(n: &Node, b: &BlobRef) {
    for _ in 0..300 {
        if n.has_blob(b).await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("blob never arrived");
}

fn meta(name: &str) -> BlobRef {
    BlobRef {
        hash: [0; 32],
        size: 0,
        key: [0; 32],
        mime: "application/octet-stream".into(),
        name: Some(name.into()),
        preview: None,
        duration_ms: None,
        dims: None,
    }
}

/// §8.1: A sends files; B downloads one from A; the anchor keeps blobs, so C gets one from the
/// anchor while A is offline; an interrupted download resumes instead of starting over.
#[tokio::test(flavor = "multi_thread")]
async fn blobs_send_anchor_resume() {
    use iroh_relay::server::{RelayConfig, Server, ServerConfig};
    let mut cfg = ServerConfig::default();
    cfg.relay = Some(RelayConfig::new(([127, 0, 0, 1], 0)));
    let relay = Server::spawn(cfg).await.unwrap();
    let url = format!("http://{}", relay.http_addr().unwrap());

    let dirs: Vec<_> = (0..5).map(|_| tempfile::tempdir().unwrap()).collect();
    let anchor = Opts {
        own_relay: Some(url),
        keep_blobs: true,
        ..Default::default()
    };
    let (x, _rx_x) = Node::open(dirs[3].path(), "px", &anchor).await.unwrap();
    let (a, _rx_a) = Node::open(dirs[0].path(), "pa", &Opts::default())
        .await
        .unwrap();
    let (b, mut rx_b) = Node::open(dirs[1].path(), "pb", &Opts::default())
        .await
        .unwrap();
    let (c, mut rx_c) = Node::open(dirs[2].path(), "pc", &Opts::default())
        .await
        .unwrap();
    a.create_space().unwrap();
    for n in [&x, &b, &c] {
        n.join(&a.invite().await.unwrap()).await.unwrap();
    }
    for _ in 0..100 {
        if [&a, &b, &c, &x].iter().all(|n| n.members().len() == 4) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let src = dirs[4].path().join("five.bin");
    let five: Vec<u8> = (0..5u32 << 20)
        .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
        .collect();
    std::fs::write(&src, &five).unwrap();
    let ev = a
        .send_media(&None, MediaKind::File, &src, meta("five.bin"), None)
        .await
        .unwrap();
    let blob = media(&ev).unwrap().1.clone();
    assert_eq!(blob.size, 5 << 20);

    // B gets it from A; the plaintext matches.
    let got = recv_media(&mut rx_b).await;
    assert_eq!(got.id, ev.id);
    b.fetch(&blob, a.device_id(), |_| {}).await.unwrap();
    assert_eq!(
        std::fs::read(b.media_path(&blob).await.unwrap()).unwrap(),
        five
    );

    // A second, bigger file; the anchor takes both, then A goes offline.
    let big = dirs[4].path().join("big.bin");
    let data: Vec<u8> = (0..20u32 << 20)
        .map(|i| (i.wrapping_mul(40503) >> 7) as u8)
        .collect();
    std::fs::write(&big, &data).unwrap();
    let ev2 = a
        .send_media(&None, MediaKind::Video, &big, meta("big.mp4"), None)
        .await
        .unwrap();
    let blob2 = media(&ev2).unwrap().1.clone();
    until_blob(&x, &blob).await;
    until_blob(&x, &blob2).await;
    let a_id = a.device_id();
    a.close().await;
    drop(a);

    // C gets the 5 MB file from the anchor alone.
    recv_media(&mut rx_c).await;
    c.fetch(&blob, a_id, |_| {}).await.unwrap();
    assert_eq!(
        std::fs::read(c.media_path(&blob).await.unwrap()).unwrap(),
        five
    );

    // B's download of the big one breaks in the middle (slowed down so the abort lands mid-way)...
    let seen = Arc::new(AtomicU64::new(0));
    let (b2, s2, bl) = (b.clone(), seen.clone(), blob2.clone());
    let task = tokio::spawn(async move {
        b2.fetch(&bl, a_id, |n| {
            s2.store(n, Ordering::Relaxed);
            std::thread::sleep(Duration::from_millis(5));
        })
        .await
    });
    while seen.load(Ordering::Relaxed) < 4 << 20 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    task.abort();
    let _ = task.await;
    assert!(!b.has_blob(&blob2).await);
    // ...and resumes: only the rest is read again.
    let read = b.fetch(&blob2, a_id, |_| {}).await.unwrap();
    assert!(
        read > 0 && read < blob2.size,
        "read {read} of {}",
        blob2.size
    );
    assert_eq!(
        std::fs::read(b.media_path(&blob2).await.unwrap()).unwrap(),
        data
    );

    for n in [b, c, x] {
        n.close().await;
    }
}
