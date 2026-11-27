//! Mentions in a group, and forwarding into another space.
use mycel_core::mycel_proto::{BlobRef, MediaKind};
use mycel_core::{Node, Opts, media};
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
async fn mention_reaches_the_mentioned_and_forward_into_another_space() {
    let dirs: [_; 3] = std::array::from_fn(|_| tempfile::tempdir().unwrap());
    let (a, _ra) = Node::open(dirs[0].path(), "pa", &Opts::default())
        .await
        .unwrap();
    let (b, _rb) = Node::open(dirs[1].path(), "pb", &Opts::default())
        .await
        .unwrap();
    a.create_space().unwrap();
    a.set_profile("Аня", "", 1).unwrap();
    b.join(&a.invite().await.unwrap()).await.unwrap();
    until(|| a.members().len() == 2).await;
    let has = |n: &Node, id| n.history(&None).unwrap().iter().any(|e| e.id == id);

    let m = a
        .send_mention(&None, "@Боб привет", None, vec![b.account()])
        .unwrap();
    let plain = a.send_text(&None, "всем", None).unwrap();
    until(|| has(&b, m.id) && has(&b, plain.id)).await;
    let got = b.event(&m.id).unwrap().1;
    assert!(b.mentions_me(&got) && !a.mentions_me(&m));
    assert!(!b.mentions_me(&b.event(&plain.id).unwrap().1));
    assert!(b.unread(&None).unwrap().contains(&m.id));

    // Into a space of its own: text and media, naming the author, without the mention.
    let (g, _rg) = Node::open(dirs[2].path(), "pg", &Opts::default())
        .await
        .unwrap();
    g.create_space().unwrap();
    let file = dirs[2].path().join("f.txt");
    std::fs::write(&file, b"data").unwrap();
    let meta = BlobRef {
        hash: [0; 32],
        size: 0,
        key: [0; 32],
        mime: "text/plain".into(),
        name: Some("f.txt".into()),
        preview: None,
        duration_ms: None,
        dims: None,
    };
    let sent = a
        .send_media(&None, MediaKind::File, &file, meta, None)
        .await
        .unwrap();
    a.forward_into(&[m.id, sent.id], &g, &None).await.unwrap();
    let hist = g.history(&None).unwrap();
    let [.., text, doc] = &hist[..] else {
        panic!("two copies")
    };
    assert_eq!(text.body.forwarded().unwrap().name, "Аня");
    assert!(!b.mentions_me(text));
    let blob = media(doc).unwrap().1;
    let path = g.media_path(blob).await.unwrap();
    assert_eq!(std::fs::read(path).unwrap(), b"data");
    for n in [&a, &b, &g] {
        n.close().await;
    }
}
