use mycel_core::{Node, Opts};
use std::time::Duration;

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
async fn disappearing_and_forwarded() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (a, _ra) = Node::open(da.path(), "pa", &Opts::default()).await.unwrap();
    let (b, _rb) = Node::open(db.path(), "pb", &Opts::default()).await.unwrap();
    a.create_space().unwrap();
    b.join(&a.invite().await.unwrap()).await.unwrap();
    until(|| a.members().len() == 2).await;
    let has = |n: &Node, id| n.history(&None).unwrap().iter().any(|e| e.id == id);

    // A message from before the timer stays; forwarding names its author.
    b.set_profile("Боб", "Б", 1).unwrap();
    let old = b.send_text(&None, "old", None).unwrap();
    until(|| has(&a, old.id) && a.profiles().unwrap().contains_key(&b.device_id())).await;
    a.forward(&[old.id], &None).unwrap();
    let fwd = a.history(&None).unwrap().pop().unwrap();
    let f = fwd.body.forwarded().unwrap();
    assert_eq!((f.name.as_str(), f.device), ("Боб", b.device_id()));
    assert_eq!(fwd.body.ttl_secs(), 0);

    // A sticker arrives as one (its emoji as the text), stays one when forwarded, cannot be edited.
    assert!(b.send_sticker(&None, "../x", "🐱", None).is_err());
    let s = b.send_sticker(&None, "f3d/1f431", "🐱", None).unwrap();
    until(|| has(&a, s.id)).await;
    assert_eq!(a.event(&s.id).unwrap().1.body.sticker(), Some("f3d/1f431"));
    assert!(b.edit(s.id, "text").is_err());
    a.forward(&[s.id], &None).unwrap();
    let copy = a.history(&None).unwrap().pop().unwrap();
    assert_eq!(copy.body.sticker(), Some("f3d/1f431"));

    // The timer reaches both sides; messages sent then carry it, on either side.
    a.set_timer(&None, 2).unwrap();
    until(|| b.timer(&None).unwrap() == 2).await;
    let m = a.send_text(&None, "bye", None).unwrap();
    let r = b.send_text(&None, "ok", None).unwrap();
    assert_eq!((m.body.ttl_secs(), r.body.ttl_secs()), (2, 2));
    until(|| has(&b, m.id) && has(&a, r.id)).await;

    // Past the TTL: hidden at once, then erased on every device.
    tokio::time::sleep(Duration::from_millis(2100)).await;
    for n in [&a, &b] {
        assert!(!has(n, m.id) && !has(n, r.id));
        n.expire().unwrap();
        assert!(n.event(&m.id).is_err() && n.event(&r.id).is_err());
        assert!(has(n, old.id));
    }

    // Off again: new messages stay.
    b.set_timer(&None, 0).unwrap();
    until(|| a.timer(&None).unwrap() == 0).await;
    assert_eq!(a.send_text(&None, "stay", None).unwrap().body.ttl_secs(), 0);
    a.close().await;
    b.close().await;
}
