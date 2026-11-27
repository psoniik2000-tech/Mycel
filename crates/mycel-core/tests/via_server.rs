//! §4.6 end to end: two clients that share only a main server (its relay, directory and mailbox)
//! find each other by username, chat, and get what was sent while they were away.
use mycel_core::{Node, Opts, mycel_proto::EventBody};
use std::path::Path;
use std::time::Duration;

async fn until(what: &str, cond: impl Fn() -> bool) {
    for _ in 0..300 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out: {what}");
}

fn has(node: &Node, peer: [u8; 32], want: &str) -> bool {
    node.history(&Some(peer))
        .unwrap()
        .iter()
        .any(|e| matches!(&e.body, EventBody::Text { text, .. } if text == want))
}

/// Envelopes the server's mailbox holds (one per line, a file per topic).
fn boxed(data: &Path) -> usize {
    std::fs::read_dir(data.join("box"))
        .unwrap()
        .map(|f| {
            let text = std::fs::read_to_string(f.unwrap().path()).unwrap();
            text.lines().count()
        })
        .sum()
}

#[tokio::test(flavor = "multi_thread")]
async fn two_clients_through_the_main_server() {
    // Nothing here may hang CI.
    tokio::time::timeout(Duration::from_secs(180), scenario())
        .await
        .expect("scenario hung");
}

async fn scenario() {
    let free = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
    let url = format!("http://127.0.0.1:{}", free.local_addr().unwrap().port());
    drop(free);
    let data = tempfile::tempdir().unwrap();
    let _server = mycel_core::spawn_anchor(&url, data.path()).await.unwrap();
    let opts = Opts {
        n0: false,
        extra_relays: vec![url.clone()],
        mailbox: Some(url.clone()),
        ..Default::default()
    };
    let dirs = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
    let (a, _ra) = Node::open(dirs[0].path(), "pa", &opts).await.unwrap();
    let (b, _rb) = Node::open(dirs[1].path(), "pb", &opts).await.unwrap();
    for (node, name, username) in [(&a, "Аня", "anya_1"), (&b, "Боря", "borya_1")] {
        node.create_space().unwrap();
        node.set_profile(name, "", 1).unwrap();
        node.dir_register(&url, username).await.unwrap();
    }
    let (a_id, b_id) = (a.device_id(), b.device_id());

    // B knows nothing about A but the name.
    assert_eq!(
        b.dir_search(&url, "any").await.unwrap(),
        [("anya_1".to_string(), "Аня".to_string())]
    );
    assert_eq!(b.dir_add(&url, "@anya_1").await.unwrap(), a_id);
    until("A got the contact", || a.contacts().unwrap() == [b_id]).await;

    b.send_text(&Some(a_id), "привет, Аня", None).unwrap();
    until("A got the text", || has(&a, b_id, "привет, Аня")).await;
    a.send_text(&Some(b_id), "привет, Боря", None).unwrap();
    until("B got the text", || has(&b, a_id, "привет, Боря")).await;

    b.close().await;
    drop(b);
    // Late receipts of the exchange land first, so the next new envelope is the message.
    let mut n = 0;
    while n != boxed(data.path()) {
        n = boxed(data.path());
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    a.send_text(&Some(b_id), "пока тебя не было", None).unwrap();
    until("the server kept the text", || boxed(data.path()) > n).await;
    // A leaves too: only the server's mailbox can hand the message over now.
    a.close().await;
    let (b, _rb) = Node::open(dirs[1].path(), "pb", &opts).await.unwrap();
    assert_eq!(b.contacts().unwrap(), [a_id]);
    until("B got the text from the mailbox", || {
        has(&b, a_id, "пока тебя не было")
    })
    .await;
    b.close().await;
}
