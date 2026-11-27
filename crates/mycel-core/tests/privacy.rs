//! Presence ("online", "last seen") and the privacy settings between two nodes.
use mycel_core::mycel_proto::{CallAction, Event, EventBody};
use mycel_core::{Node, Opts, Privacy, Who};
use std::time::Duration;
use tokio::sync::mpsc::Receiver;

async fn until(what: &str, cond: impl Fn() -> bool) {
    for _ in 0..150 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("not reached: {what}");
}

/// The next call action `rx` reports.
async fn call_action(rx: &mut Receiver<Event>) -> CallAction {
    let wait = async {
        loop {
            if let EventBody::Call { action, .. } = rx.recv().await.unwrap().body {
                return action;
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(15), wait)
        .await
        .expect("no call event")
}

#[tokio::test(flavor = "multi_thread")]
async fn presence_and_privacy() {
    let dirs: Vec<_> = (0..2).map(|_| tempfile::tempdir().unwrap()).collect();
    let (a, mut ra) = Node::open(dirs[0].path(), "pa", &Opts::default())
        .await
        .unwrap();
    let (b, mut rb) = Node::open(dirs[1].path(), "pb", &Opts::default())
        .await
        .unwrap();
    a.create_space().unwrap();
    a.set_profile("Аня", "", 0).unwrap();
    b.join(&a.invite().await.unwrap()).await.unwrap();
    let (ia, ib) = (a.device_id(), b.device_id());
    until("a's profile at b", || {
        b.profiles().unwrap().contains_key(&ia)
    })
    .await;
    let (to_a, to_b) = (Some(ia), Some(ib));

    // On screen → online; away → offline with the time.
    a.set_active(true);
    b.set_active(true);
    until("b online", || a.presence(&ib).online).await;
    b.set_active(false);
    until("b offline", || !a.presence(&ib).online).await;
    let p = a.presence(&ib);
    assert!(!p.hidden && p.seen.is_some(), "{p:?}");
    b.set_active(true);
    until("b online again", || a.presence(&ib).online).await;

    // Hidden from everyone, and then b does not see a either.
    let set = |n: &Node, f: fn(&mut Privacy)| {
        let mut p = n.privacy();
        f(&mut p);
        n.set_privacy(p).unwrap();
    };
    set(&b, |p| p.last_seen = Who::Nobody);
    until("b hidden", || a.presence(&ib).hidden).await;
    assert!(!a.presence(&ib).online);
    let p = b.presence(&ia);
    assert!(p.hidden && !p.online, "{p:?}");

    // Contacts only: a space member b never wrote to is not one, until b writes.
    set(&b, |p| p.last_seen = Who::Contacts);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(a.presence(&ib).hidden && b.presence(&ia).hidden);
    b.open_dm(ia).unwrap();
    b.send_text(&to_a, "привет", None).unwrap();
    until("dm at a", || a.history(&to_b).is_ok_and(|h| !h.is_empty())).await;
    // The next presence (here: a settings save; else the periodic sync) shows it.
    set(&b, |_| {});
    until("visible to a contact", || a.presence(&ib).online).await;
    assert!(!b.presence(&ia).hidden);

    // Read receipts off: the partner stays at one tick, and so do we for theirs.
    set(&b, |p| p.read_receipts = false);
    let m = a.send_text(&to_b, "читал?", None).unwrap();
    until("delivered", || {
        a.statuses(&to_b).unwrap().contains(&(m.id, 1))
    })
    .await;
    until("at b", || !b.unread(&to_a).unwrap().is_empty()).await;
    b.mark_read(&to_a).unwrap();
    assert!(b.unread(&to_a).unwrap().is_empty());
    let mine = b.send_text(&to_a, "ответ", None).unwrap();
    until("reply at a", || a.unread(&to_b).unwrap().contains(&mine.id)).await;
    a.mark_read(&to_b).unwrap();
    until("delivered to a", || {
        b.statuses(&to_a).unwrap().contains(&(mine.id, 1))
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(a.statuses(&to_b).unwrap().contains(&(m.id, 1)));
    assert!(b.statuses(&to_a).unwrap().contains(&(mine.id, 1)));

    // Calls from nobody: declined, never reported to b.
    set(&b, |p| p.calls = Who::Nobody);
    while rb.try_recv().is_ok() {}
    a.call_offer(&to_b, false).unwrap();
    assert_eq!(call_action(&mut ra).await, CallAction::Reject);
    a.call_end().unwrap();
    assert_eq!(call_action(&mut rb).await, CallAction::End);
    // Contacts: a is one by now, so the call rings.
    set(&b, |p| p.calls = Who::Contacts);
    a.call_offer(&to_b, false).unwrap();
    assert_eq!(
        call_action(&mut rb).await,
        CallAction::Offer { video: false }
    );

    a.close().await;
    b.close().await;
}
