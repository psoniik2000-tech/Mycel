//! §4.6: usernames on a main server (anchor relay + directory), found and added as contacts.
use mycel_core::{Node, Opts, dir};
use std::time::Duration;

async fn until(cond: impl Fn() -> bool) {
    for _ in 0..150 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("condition not reached");
}

#[tokio::test(flavor = "multi_thread")]
async fn register_lookup_conflict_rename_and_add() {
    let url = "http://127.0.0.1:34881";
    let data = tempfile::tempdir().unwrap();
    let server = mycel_core::spawn_anchor(url, data.path()).await.unwrap();
    let opts = Opts {
        extra_relays: vec![url.into()],
        // The main server is the mailbox too.
        mailbox: Some(url.into()),
        ..Default::default()
    };
    let dirs = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
    assert!(mycel_core::check_server(url, true).await.is_ok());
    let (a, _ra) = Node::open(dirs[0].path(), "pa", &opts).await.unwrap();
    let (b, _rb) = Node::open(dirs[1].path(), "pb", &opts).await.unwrap();
    for (n, name) in [(&a, "Аня"), (&b, "Боря")] {
        n.create_space().unwrap();
        n.set_profile(name, "", 1).unwrap();
    }

    // The id comes from the relay port; the answer pins it.
    let pinned = a.dir_register(url, "Alice_1").await.unwrap();
    assert_eq!(pinned, format!("{url}#{}", server.dir_id().unwrap()));
    assert!(
        a.dir_register(&pinned, "@alice_1").await.is_ok(),
        "own update"
    );
    let e = b.dir_register(url, "alice_1").await.unwrap_err();
    assert!(e.to_string().contains(dir::DIR_TAKEN), "{e}");
    assert!(b.dir_register(url, "bo").await.is_err(), "too short");
    b.dir_register(url, "bob_22").await.unwrap();

    let r = b.dir_lookup(url, "ALICE_1").await.unwrap().unwrap();
    assert_eq!((r.key, r.name.as_str()), (a.device_id(), "Аня"));
    assert_eq!(
        b.dir_search(url, "@al").await.unwrap(),
        [("alice_1".to_string(), "Аня".to_string())]
    );
    assert!(b.dir_search(url, "zz").await.unwrap().is_empty());

    // Not findable: out of the search, still found by the whole name.
    let mut p = a.privacy();
    p.findable = false;
    a.set_privacy(p).unwrap();
    a.dir_register(url, "alice_1").await.unwrap();
    assert!(b.dir_search(url, "al").await.unwrap().is_empty());
    assert!(b.dir_lookup(url, "alice_1").await.unwrap().is_some());

    // Renaming frees the old name.
    a.dir_register(url, "alice_2").await.unwrap();
    assert!(b.dir_lookup(url, "alice_1").await.unwrap().is_none());
    let me = b.dir_name_of(url, a.device_id()).await.unwrap().unwrap();
    assert_eq!(me.username, "alice_2");

    let a_id = a.device_id();
    // Adding by username makes a contact once; the link is reusable.
    assert_eq!(b.dir_add(url, "alice_2").await.unwrap(), a.device_id());
    assert_eq!(b.contacts().unwrap(), [a.device_id()]);
    until(|| a.contacts().unwrap() == [b.device_id()]).await;
    assert_eq!(b.dir_add(url, "alice_2").await.unwrap(), a.device_id());
    assert_eq!(b.contacts().unwrap(), [a.device_id()]);
    assert!(b.dir_add(url, "bob_22").await.is_err(), "not yourself");

    // Who came by the public link is not A's contact until A writes to them.
    let mut p = a.privacy();
    p.last_seen = mycel_core::Who::Contacts;
    a.set_privacy(p).unwrap();
    assert!(a.presence(&b.device_id()).hidden);
    a.send_text(&Some(b.device_id()), "привет", None).unwrap();
    assert!(!a.presence(&b.device_id()).hidden);

    // A is offline: the main server keeps B's message until A pulls it.
    a.close().await;
    drop(a);
    b.send_text(&Some(a_id), "пока тебя не было", None).unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    b.close().await;
    let (a, _ra) = Node::open(dirs[0].path(), "pa", &opts).await.unwrap();
    let b_id = b.device_id();
    until(|| {
        a.history(&Some(b_id))
            .unwrap()
            .iter()
            .any(|e| matches!(&e.body, mycel_core::mycel_proto::EventBody::Text { text, .. } if text == "пока тебя не было"))
    })
    .await;

    // Survives a restart of the server.
    drop(server);
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let server = mycel_core::spawn_anchor(url, data.path()).await.unwrap();
    assert_eq!(pinned, format!("{url}#{}", server.dir_id().unwrap()));
    let r = a.dir_lookup(&pinned, "alice_2").await.unwrap().unwrap();
    assert_eq!(r.key, a_id);
    a.close().await;
}
