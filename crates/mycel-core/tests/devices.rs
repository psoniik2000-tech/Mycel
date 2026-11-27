use mycel_core::mycel_proto::EventBody;
use mycel_core::{Node, Opts};
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

/// §4.4: a phone (A) links a PC (B) to its account; the contact C sees one contact with one
/// name and one chat, and both of A's devices see everything that was and is written there.
#[tokio::test(flavor = "multi_thread")]
async fn two_devices_one_contact() {
    let dirs: Vec<_> = (0..4).map(|_| tempfile::tempdir().unwrap()).collect();
    let mut nodes = vec![];
    for (i, d) in dirs.iter().enumerate() {
        nodes.push(
            Node::open(d.path(), &format!("p{i}"), &Opts::default())
                .await
                .unwrap()
                .0,
        );
    }
    let (a, b, c, d) = (&nodes[0], &nodes[1], &nodes[2], &nodes[3]);
    a.create_space().unwrap();
    a.set_profile("Аня", "А", 1).unwrap();
    c.join(&a.contact_invite().await.unwrap()).await.unwrap();
    c.set_profile("Вика", "", 2).unwrap();
    let (ka, kc) = (Some(a.device_id()), Some(c.device_id()));
    c.send_text(&ka, "до привязки", None).unwrap();
    until("1", || texts(a, &kc) == ["до привязки"]).await;

    // B links to A: same account, the chat and its history come along.
    let link = a.device_invite().await.unwrap();
    b.join(&link).await.unwrap();
    assert!(b.join(&link).await.is_err());
    assert_eq!(b.account(), a.device_id());
    assert_eq!(b.devices(), [a.device_id(), b.device_id()]);
    assert_eq!(b.dms().unwrap(), [c.device_id()]);
    assert_eq!(texts(b, &kc), ["до привязки"]);
    until("2", || a.devices() == [a.device_id(), b.device_id()]).await;

    // C: B is A — one contact, A's name.
    until("3", || c.account_of(&b.device_id()) == a.device_id()).await;
    assert_eq!(c.contacts().unwrap(), [a.device_id()]);
    assert_eq!(c.dms().unwrap(), [a.device_id()]);
    until("4", || {
        c.profiles()
            .unwrap()
            .get(&b.device_id())
            .map(|p| p.0.clone())
            == Some("Аня".into())
    })
    .await;

    // From B into the same chat; C's answer reaches both devices.
    b.send_text(&kc, "с компьютера", None).unwrap();
    until("5", || texts(c, &ka) == ["до привязки", "с компьютера"]).await;
    until("6", || texts(a, &kc) == ["до привязки", "с компьютера"]).await;
    c.send_text(&ka, "вижу", None).unwrap();
    until("7", || {
        texts(a, &kc).last().map(String::as_str) == Some("вижу")
    })
    .await;
    until("8", || {
        texts(b, &kc).last().map(String::as_str) == Some("вижу")
    })
    .await;

    // Read on B clears the unread on A.
    until("9", || !a.unread(&kc).unwrap().is_empty()).await;
    until("10", || !b.unread(&kc).unwrap().is_empty()).await;
    b.mark_read(&kc).unwrap();
    until("11", || a.unread(&kc).unwrap().is_empty()).await;

    // A contact made on A after the link is B's chat too.
    d.join(&a.contact_invite().await.unwrap()).await.unwrap();
    let kd = Some(d.device_id());
    until("12", || b.dms().unwrap().contains(&d.device_id())).await;
    d.send_text(&ka, "новый контакт", None).unwrap();
    until("13", || texts(b, &kd) == ["новый контакт"]).await;

    // Unlinked: B leaves the account.
    a.unlink_device(b.device_id()).unwrap();
    until("14", || c.account_of(&b.device_id()) == b.device_id()).await;
    assert_eq!(a.devices(), [a.device_id()]);

    for n in &nodes {
        n.close().await;
    }
}
