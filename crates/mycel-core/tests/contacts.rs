use mycel_core::mycel_proto::{Envelope, EventBody};
use mycel_core::{Node, Opts};
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

/// §4.2: two independent users become contacts by a link from either side (here a non-admin
/// member of a space invites a fresh node); the pair group has its own space id, and neither
/// side's other people see it or are seen.
#[tokio::test(flavor = "multi_thread")]
async fn contacts_by_link_closed_to_others() {
    let dirs: Vec<_> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let mut nodes = vec![];
    for (i, d) in dirs.iter().enumerate() {
        nodes.push(
            Node::open(d.path(), &format!("p{i}"), &Opts::default())
                .await
                .unwrap()
                .0,
        );
    }
    let (a, c, b) = (&nodes[0], &nodes[1], &nodes[2]);
    // A's circle: A (admin) and C. B is a fresh install with nothing.
    a.create_space().unwrap();
    a.set_profile("Аня", "", 1).unwrap();
    c.join(&a.invite().await.unwrap()).await.unwrap();
    c.set_profile("Вика", "", 2).unwrap();
    until(|| a.profiles().unwrap().len() == 2).await;

    // C (not an admin) adds B; B gets its own space on the way. A link works once.
    let link = c.contact_invite().await.unwrap();
    b.join(&link).await.unwrap();
    assert!(b.join(&link).await.is_err());
    b.set_profile("Боря", "", 3).unwrap();
    assert_eq!(b.contacts().unwrap(), [c.device_id()]);
    assert_eq!(c.contacts().unwrap(), [b.device_id()]);
    assert_eq!(b.members(), [b.device_id()]);
    until(|| {
        c.profiles()
            .unwrap()
            .get(&b.device_id())
            .map(|p| p.0.clone())
            == Some("Боря".into())
            && b.profiles()
                .unwrap()
                .get(&c.device_id())
                .map(|p| p.0.clone())
                == Some("Вика".into())
    })
    .await;

    // A 1:1 chat both ways, opened like any other.
    let (kb, kc) = (Some(b.device_id()), Some(c.device_id()));
    c.open_dm(b.device_id()).unwrap();
    c.send_text(&kb, "привет, Боря", None).unwrap();
    until(|| texts(b, &kc) == ["привет, Боря"]).await;
    b.send_text(&kc, "привет, Вика", None).unwrap();
    until(|| texts(c, &kb) == ["привет, Боря", "привет, Вика"]).await;
    a.send_text(&None, "круг", None).unwrap();
    until(|| texts(c, &None) == ["круг"]).await;

    // B knows nothing of A, A nothing of B; C's space chat is not B's.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!b.profiles().unwrap().contains_key(&a.device_id()));
    assert!(!a.profiles().unwrap().contains_key(&b.device_id()));
    assert!(a.dms().unwrap().is_empty() && a.contacts().unwrap().is_empty());
    assert!(texts(b, &None).is_empty());
    assert_eq!(b.dms().unwrap(), [c.device_id()]);

    for n in &nodes {
        n.close().await;
    }
    let spaces = |i: usize| {
        let store = mycel_store::Store::open(dirs[i].path(), &format!("p{i}")).unwrap();
        let mut s: Vec<[u8; 16]> = store
            .envelopes()
            .unwrap()
            .iter()
            .map(|(_, e)| postcard::from_bytes::<Envelope>(e).unwrap().space_id)
            .collect();
        s.sort();
        s.dedup();
        s
    };
    // A keeps no envelope of the pair group (not even ciphertext); B none of A's circle.
    let (sa, sb, sc) = (spaces(0), spaces(2), spaces(1));
    assert_eq!(sa.len(), 1);
    assert_eq!(sc.len(), 2);
    assert!(sb.iter().all(|s| !sa.contains(s)));
}
