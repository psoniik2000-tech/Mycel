use mycel_core::{Node, Opts, Typing};
use std::time::Duration;

/// §7.4: a typing signal reaches the partner, is throttled, and never lands in history.
#[tokio::test(flavor = "multi_thread")]
async fn typing_reaches_peer_not_history() {
    let dirs: Vec<_> = (0..2).map(|_| tempfile::tempdir().unwrap()).collect();
    let (a, _rx_a) = Node::open(dirs[0].path(), "pa", &Opts::default())
        .await
        .unwrap();
    let (b, _rx_b) = Node::open(dirs[1].path(), "pb", &Opts::default())
        .await
        .unwrap();
    a.create_space().unwrap();
    b.join(&a.invite().await.unwrap()).await.unwrap();
    let mut tb = b.typing_events();
    let before = (
        a.history(&None).unwrap().len(),
        b.history(&None).unwrap().len(),
    );

    a.typing(&None, false).unwrap();
    let t = tokio::time::timeout(Duration::from_secs(10), tb.recv())
        .await
        .expect("timeout")
        .unwrap();
    assert_eq!(
        t,
        Typing {
            chat: None,
            from: a.device_id(),
            voice: false
        }
    );
    // Within 3 s the same state is not resent; switching to voice is.
    a.typing(&None, false).unwrap();
    a.typing(&None, true).unwrap();
    let t = tokio::time::timeout(Duration::from_secs(10), tb.recv())
        .await
        .expect("timeout")
        .unwrap();
    assert!(t.voice);

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(tb.try_recv().is_err());
    assert_eq!(
        before,
        (
            a.history(&None).unwrap().len(),
            b.history(&None).unwrap().len()
        )
    );
}
