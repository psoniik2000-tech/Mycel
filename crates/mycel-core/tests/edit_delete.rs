use mycel_core::{Node, Opts, edits_and_pins, reactions};
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
async fn edit_delete_pin_between_two_nodes() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (a, _ra) = Node::open(da.path(), "pa", &Opts::default()).await.unwrap();
    let (b, _rb) = Node::open(db.path(), "pb", &Opts::default()).await.unwrap();
    a.create_space().unwrap();
    b.join(&a.invite().await.unwrap()).await.unwrap();
    until(|| a.members().len() == 2).await;
    let has = |n: &Node, id| n.history(&None).unwrap().iter().any(|e| e.id == id);

    // Edit by the author shows on the other side; a reply keeps its target.
    let m = a.send_text(&None, "helo", None).unwrap();
    until(|| has(&b, m.id)).await;
    a.edit(m.id, "hello").unwrap();
    until(|| edits_and_pins(&b.history(&None).unwrap()).0.get(&m.id) == Some(&"hello".into()))
        .await;
    assert!(b.edit(m.id, "hijack").is_err());
    let r = b.send_text(&None, "re", Some(m.id)).unwrap();
    until(|| has(&a, r.id)).await;

    // Pin.
    b.pin(m.id, true).unwrap();
    until(|| edits_and_pins(&a.history(&None).unwrap()).1 == vec![m.id]).await;

    // Reaction; the same one again takes it back.
    let r_of = |n: &Node| reactions(&n.history(&None).unwrap()).get(&m.id).cloned();
    b.react(m.id, "👍").unwrap();
    until(|| r_of(&a) == Some(vec![("👍".into(), vec![b.device_id()])])).await;
    b.react(m.id, "👍").unwrap();
    until(|| r_of(&a).is_none()).await;

    // Only the author or the admin deletes for everyone; it is gone on both nodes.
    assert!(b.delete(m.id, true).is_err());
    b.delete(r.id, true).unwrap();
    assert!(!has(&b, r.id));
    until(|| !has(&a, r.id)).await;
    let x = b.send_text(&None, "spam", None).unwrap();
    until(|| has(&a, x.id)).await;
    a.delete(x.id, true).unwrap(); // admin
    until(|| !has(&b, x.id)).await;

    // Delete for me only stays local.
    a.delete(m.id, false).unwrap();
    assert!(!has(&a, m.id));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(has(&b, m.id));
    a.close().await;
    b.close().await;
}
