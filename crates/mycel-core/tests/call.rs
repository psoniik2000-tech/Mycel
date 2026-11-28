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

/// Repeated calls both ways: every Offer arrives, media flows, End reaches the other side.
#[tokio::test(flavor = "multi_thread")]
async fn five_calls_in_a_row_both_ways() {
    let dirs: Vec<_> = (0..2).map(|_| tempfile::tempdir().unwrap()).collect();
    let (a, mut rx_a) = Node::open(dirs[0].path(), "pa", &Opts::default())
        .await
        .unwrap();
    let (b, mut rx_b) = Node::open(dirs[1].path(), "pb", &Opts::default())
        .await
        .unwrap();
    a.create_space().unwrap();
    b.join(&a.invite().await.unwrap()).await.unwrap();
    let (mut fa, mut fb) = (a.call_frames(), b.call_frames());

    for i in 0..10 {
        // Alternate the caller; the callee hangs up every other time.
        let ((caller, rx_caller, f_caller), (callee, rx_callee, f_callee)) = if i % 2 == 0 {
            ((&a, &mut rx_a, &mut fa), (&b, &mut rx_b, &mut fb))
        } else {
            ((&b, &mut rx_b, &mut fb), (&a, &mut rx_a, &mut fa))
        };
        let (id, _) = caller.call_offer(&None, i % 3 == 0).unwrap();
        assert_eq!(
            recv_call(rx_callee).await,
            (id, CallAction::Offer { video: i % 3 == 0 }),
            "call {i}: offer lost"
        );
        callee.call_accept(id).unwrap();
        assert_eq!(recv_call(rx_caller).await, (id, CallAction::Accept));
        exchange(caller, f_callee, AUDIO, b"hi").await;
        exchange(callee, f_caller, AUDIO, b"hi").await;
        let ((ender, ender_rx), (other, other_rx)) = if i % 4 < 2 {
            ((caller, rx_caller), (callee, rx_callee))
        } else {
            ((callee, rx_callee), (caller, rx_caller))
        };
        ender.call_end().unwrap();
        assert_eq!(recv_call(other_rx).await, (id, CallAction::End));
        // The UI hangs up when the last peer left.
        other.call_end().unwrap();
        assert_eq!(recv_call(ender_rx).await, (id, CallAction::End));
        assert!(a.call_id().is_none() && b.call_id().is_none());
        // Drop stale frames of this call.
        while fa.try_recv().is_ok() {}
        while fb.try_recv().is_ok() {}
    }
}

/// A call in A↔B's private chat; A invites C through A↔C's chat (C is not in the call's chat):
/// C rings, accepts, and media flows between C and B, who share no chat with the call.
#[tokio::test(flavor = "multi_thread")]
async fn invite_into_running_call() {
    let dirs: Vec<_> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let mut nodes = vec![];
    for (i, d) in dirs.iter().enumerate() {
        nodes.push(
            Node::open(d.path(), &format!("p{i}"), &Opts::default())
                .await
                .unwrap(),
        );
    }
    let mut it = nodes.into_iter();
    let ((a, _ra), (b, mut rx_b), (c, mut rx_c)) =
        (it.next().unwrap(), it.next().unwrap(), it.next().unwrap());
    let (a, b, c) = (&a, &b, &c);
    a.create_space().unwrap();
    b.join(&a.invite().await.unwrap()).await.unwrap();
    c.join(&a.invite().await.unwrap()).await.unwrap();
    for _ in 0..150 {
        if [a, b, c].iter().all(|n| n.members().len() == 3) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    for n in [a, b, c] {
        n.set_profile("x", "", 1).unwrap();
    }
    // Key packages (with the profiles) arrive after the join.
    for p in [b.device_id(), c.device_id()] {
        let mut r = a.open_dm(p);
        for _ in 0..150 {
            if r.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            r = a.open_dm(p);
        }
        r.unwrap();
    }
    let (fb, fc) = (&mut b.call_frames(), &mut c.call_frames());
    let (id, _) = a.call_offer(&Some(b.device_id()), false).unwrap();
    assert_eq!(recv_call(&mut rx_b).await.0, id);
    b.call_accept(id).unwrap();
    exchange(a, fb, AUDIO, b"a").await;

    a.call_invite(&Some(c.device_id())).unwrap();
    let (got, action) = recv_call(&mut rx_c).await;
    assert_eq!(got, id);
    assert!(matches!(action, CallAction::Invite { .. }));
    c.call_accept(id).unwrap();

    let f = loop {
        let f = exchange(b, fc, AUDIO, b"from-b").await;
        if f.from == b.device_id() {
            break f;
        }
    };
    assert_eq!(f.data, b"from-b");
    let f = loop {
        let f = exchange(c, fb, AUDIO, b"from-c").await;
        if f.from == c.device_id() {
            break f;
        }
    };
    assert_eq!(f.data, b"from-c");
}
