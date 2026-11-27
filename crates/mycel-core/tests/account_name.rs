//! §4.6 + §4.4: a username belongs to the account; any of its devices updates it, and whoever
//! writes to the name reaches every device of the account.
use mycel_core::mycel_proto::EventBody;
use mycel_core::{Node, Opts, dir};
use std::time::Duration;

async fn until(what: &str, cond: impl Fn() -> bool) {
    for _ in 0..200 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("not reached: {what}");
}

fn texts(n: &Node, chat: &Option<[u8; 32]>) -> Vec<String> {
    n.history(chat)
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.body {
            EventBody::Text { text, .. } => Some(text),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn linked_device_shares_the_username() {
    let url = "http://127.0.0.1:34882";
    let data = tempfile::tempdir().unwrap();
    let _server = mycel_core::spawn_anchor(url, data.path()).await.unwrap();
    let opts = Opts {
        extra_relays: vec![url.into()],
        mailbox: Some(url.into()),
        ..Default::default()
    };
    let dirs: Vec<_> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let mut nodes = vec![];
    for (i, d) in dirs.iter().enumerate() {
        nodes.push(
            Node::open(d.path(), &format!("p{i}"), &opts)
                .await
                .unwrap()
                .0,
        );
    }
    let (a, a2, b) = (&nodes[0], &nodes[1], &nodes[2]);
    a.create_space().unwrap();
    a.set_profile("Аня", "", 1).unwrap();
    b.create_space().unwrap();
    b.set_profile("Боря", "", 2).unwrap();
    a.dir_register(url, "anna_1").await.unwrap();

    a2.join(&a.device_invite().await.unwrap()).await.unwrap();
    until("linked", || a.devices() == [a.device_id(), a2.device_id()]).await;
    // The linked device updates the account's name, the root still can, others cannot.
    a2.dir_register(url, "anna_1").await.unwrap();
    let r = b.dir_lookup(url, "anna_1").await.unwrap().unwrap();
    assert_eq!((r.key, r.owner()), (a2.device_id(), a.device_id()));
    let e = b.dir_register(url, "anna_1").await.unwrap_err();
    assert!(e.to_string().contains(dir::DIR_TAKEN), "{e}");
    let me = b.dir_name_of(url, a.device_id()).await.unwrap().unwrap();
    assert_eq!(me.username, "anna_1");

    // B writes by username (the record now points at A2) and reaches both devices.
    let acc = b.dir_add(url, "anna_1").await.unwrap();
    until("one account", || b.account_of(&acc) == a.device_id()).await;
    let ka = Some(a.device_id());
    b.send_text(&ka, "привет", None).unwrap();
    let kb = Some(b.device_id());
    until("root", || texts(a, &kb) == ["привет"]).await;
    until("linked", || texts(a2, &kb) == ["привет"]).await;

    a.dir_register(url, "anna_1").await.unwrap();
    for n in &nodes {
        n.close().await;
    }
}
