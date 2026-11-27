use mycel_core::mycel_proto::{EventBody, ReceiptKind};
use mycel_core::{Node, Opts, edits_and_pins};
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

/// §17 п.1: names via Profile; a 1:1 chat is a 2-member MLS group that a third member
/// relays but can neither read nor see; calls in it are just for the two.
#[tokio::test(flavor = "multi_thread")]
async fn names_and_private_chat_closed_to_third() {
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
    let (a, b, c) = (&nodes[0], &nodes[1], &nodes[2]);
    a.create_space().unwrap();
    // Set before the others join: they still get it (members re-send on a join).
    a.set_profile("Аня", "", 1).unwrap();
    b.join(&a.invite().await.unwrap()).await.unwrap();
    c.join(&a.invite().await.unwrap()).await.unwrap();
    until(|| nodes.iter().all(|n| n.members().len() == 3)).await;

    // Names reach everyone.
    until(|| {
        [b, c]
            .iter()
            .all(|n| n.profiles().unwrap().contains_key(&a.device_id()))
    })
    .await;
    for (n, name) in nodes[1..].iter().zip(["Боря", "Вика"]) {
        n.set_profile(name, "", 1).unwrap();
    }
    until(|| nodes.iter().all(|n| n.profiles().unwrap().len() == 3)).await;
    assert_eq!(c.profiles().unwrap()[&b.device_id()].0, "Боря");

    // A opens a chat with B; both sides write and edit, and A forwards a space message into it.
    let (ka, kb) = (Some(a.device_id()), Some(b.device_id()));
    a.open_dm(b.device_id()).unwrap();
    a.open_dm(b.device_id()).unwrap(); // no second group
    let hi = a.send_text(&kb, "секрет для Бори", None).unwrap();
    until(|| texts(b, &ka) == ["секрет для Бори"]).await;
    // §7.4: ✓ once B's device has it, ✓✓ once B read it; a second mark_read sends nothing.
    let st = |n: &Node| {
        n.statuses(&kb)
            .unwrap()
            .iter()
            .find(|(id, _)| *id == hi.id)
            .unwrap()
            .1
    };
    until(|| st(a) == 1).await;
    let reads = |n: &Node| {
        n.history(&ka)
            .unwrap()
            .iter()
            .filter(|e| {
                matches!(
                    e.body,
                    EventBody::Receipt {
                        kind: ReceiptKind::Read,
                        ..
                    }
                )
            })
            .count()
    };
    assert_eq!(b.unread(&ka).unwrap(), [hi.id]);
    b.mark_read(&ka).unwrap();
    b.mark_read(&ka).unwrap();
    assert_eq!(reads(b), 1);
    assert!(b.unread(&ka).unwrap().is_empty());
    until(|| st(a) == 2).await;
    assert_eq!(b.dms().unwrap(), vec![a.device_id()]);
    b.send_text(&ka, "ответ Ане", Some(hi.id)).unwrap();
    until(|| texts(a, &kb).len() == 2).await;
    a.edit(hi.id, "тайна для Бори").unwrap();
    until(|| {
        edits_and_pins(&b.history(&ka).unwrap()).0.get(&hi.id) == Some(&"тайна для Бори".into())
    })
    .await;
    let open = a.send_text(&None, "всем", None).unwrap();
    until(|| texts(c, &None) == ["всем"]).await;
    a.forward(&[open.id], &kb).unwrap();
    until(|| texts(b, &ka).len() == 3).await;
    assert_eq!(texts(a, &None), ["всем"]);

    // A 1:1 call: B sees it in the chat with A, C does not see it at all.
    let (call, _) = a.call_offer(&kb, false).unwrap();
    until(|| b.call_chat(call).is_ok()).await;
    assert_eq!(b.call_chat(call).unwrap(), ka);
    a.call_end().unwrap();

    // C got the ciphertext (it relays it) but no chat, no event and no plaintext.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(c.dms().unwrap().is_empty());
    assert!(c.call_chat(call).is_err());
    let all = c.all_events().unwrap();
    assert!(all.iter().all(|(chat, _)| chat.is_none()));
    assert!(texts(c, &None) == ["всем"]);
    for n in &nodes {
        n.close().await;
    }
    let store = mycel_store::Store::open(dirs[2].path(), "p2").unwrap();
    let envs = store.envelopes().unwrap();
    assert!(envs.len() > 3, "C keeps 1:1 envelopes to forward them");
    for (_, env) in envs {
        assert!(!env.windows(6).any(|w| w == &"тайна".as_bytes()[..6]));
        assert!(!env.windows(6).any(|w| w == &"секрет".as_bytes()[..6]));
    }
}
