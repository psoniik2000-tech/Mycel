use mycel_core::Node;
use mycel_core::mycel_proto::EventBody;
use std::time::Duration;

fn text(ev: &mycel_core::mycel_proto::Event) -> &str {
    match &ev.body {
        EventBody::Text { text, .. } => text,
        _ => panic!("not text"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn message_delivered_saved_and_replied() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (a, mut rx_a) = Node::open(da.path(), "pa", 0, false).await.unwrap();
    let (b, mut rx_b) = Node::open(db.path(), "pb", 0, false).await.unwrap();

    b.add_peer(&a.ticket(false).await).unwrap();
    b.send_text("hello").await.unwrap();
    let got = tokio::time::timeout(Duration::from_secs(10), rx_a.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(text(&got), "hello");
    assert_eq!(got.author_device, b.device_id());

    // A learned B from the incoming connection and can reply.
    a.send_text("hi back").await.unwrap();
    let back = tokio::time::timeout(Duration::from_secs(10), rx_b.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(text(&back), "hi back");
    assert_eq!(back.lamport, 2);

    a.close().await;
    b.close().await;
    // History survives reopening the encrypted store.
    let hist = mycel_store::Store::open(da.path(), "pa")
        .unwrap()
        .events()
        .unwrap();
    assert_eq!(hist.len(), 2);
}
