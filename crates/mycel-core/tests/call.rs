use mycel_core::mycel_proto::{CallAction, Event, EventBody};
use mycel_core::{AUDIO, Frame, Node, Opts, VIDEO_KEY};
use std::time::Duration;
use tokio::sync::mpsc::Receiver;

/// Next Call event (text and receipts are skipped).
async fn recv_call(rx: &mut Receiver<Event>) -> ([u8; 16], CallAction) {
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("timeout")
            .expect("closed");
        if let EventBody::Call { call_id, action } = ev.body {
            return (call_id, action);
        }
    }
}

/// Sends `kind` frames from `n` every 50 ms until one arrives on `rx` (connections come up async).
async fn exchange(n: &Node, rx: &mut Receiver<Frame>, kind: u8, data: &[u8]) -> Frame {
    for _ in 0..200 {
        n.call_send(kind, data).unwrap();
        if let Ok(Some(f)) = tokio::time::timeout(Duration::from_millis(50), rx.recv()).await {
            return f;
        }
    }
    panic!("no frame arrived");
}

/// §9: Offer → Accept → frames both ways (audio datagrams, a big video frame) → End.
#[tokio::test(flavor = "multi_thread")]
async fn offer_accept_frames_end() {
    let dirs: Vec<_> = (0..2).map(|_| tempfile::tempdir().unwrap()).collect();
    let (a, _rx_a) = Node::open(dirs[0].path(), "pa", &Opts::default())
        .await
        .unwrap();
    let (b, mut rx_b) = Node::open(dirs[1].path(), "pb", &Opts::default())
        .await
        .unwrap();
    a.create_space().unwrap();
    b.join(&a.invite().await.unwrap()).await.unwrap();
    let (mut fa, mut fb) = (a.call_frames(), b.call_frames());

    let (id, _) = a.call_offer(&None, false).unwrap();
    assert!(a.call_offer(&None, false).is_err(), "one call at a time");
    assert_eq!(
        recv_call(&mut rx_b).await,
        (id, CallAction::Offer { video: false })
    );
    b.call_accept(id).unwrap();

    let f = exchange(&a, &mut fb, AUDIO, b"opus-a").await;
    assert_eq!((f.from, f.kind), (a.device_id(), AUDIO));
    assert_eq!(f.data, b"opus-a");
    let f = exchange(&b, &mut fa, AUDIO, b"opus-b").await;
    assert_eq!((f.from, f.data.as_slice()), (b.device_id(), &b"opus-b"[..]));

    // A video key frame is far over one datagram: it goes as a stream.
    let big: Vec<u8> = (0..100_000).map(|i| i as u8).collect();
    let f = loop {
        let f = exchange(&a, &mut fb, VIDEO_KEY, &big).await;
        if f.kind == VIDEO_KEY {
            break f;
        }
    };
    assert_eq!(f.data, big);

    // Link stats: one entry per participant, with an RTT from the live path.
    let st = a.call_stats();
    assert_eq!(st.len(), 1);
    assert_eq!(st[0].peer, b.device_id());
    assert!(st[0].rtt_ms < 5_000 && (0.0..=1.0).contains(&st[0].loss));

    let mut members = a.call_members(id).unwrap();
    members.sort();
    let mut both = vec![a.device_id(), b.device_id()];
    both.sort();
    assert_eq!(members, both);

    b.call_end().unwrap();
    assert!(b.call_send(AUDIO, b"x").is_err());
    for _ in 0..100 {
        if a.call_members(id).unwrap() == vec![a.device_id()] {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(a.call_members(id).unwrap(), vec![a.device_id()]);
}
