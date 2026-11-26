use mycel_core::mycel_proto::EventBody;
use mycel_core::{Node, Opts};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Same protocol as `mailbox/worker.js`, in memory.
async fn mock_mailbox() -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    type Rows = Vec<(String, String, Vec<u8>)>;
    let rows: Arc<Mutex<Rows>> = Arc::default();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = l.accept().await.unwrap();
            let rows = rows.clone();
            tokio::spawn(async move {
                let mut buf = vec![];
                let mut chunk = [0u8; 4096];
                let (head, body) = loop {
                    let n = s.read(&mut chunk).await.unwrap();
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..i]).to_string();
                        let len = head
                            .lines()
                            .find_map(|l| {
                                l.to_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        while buf.len() < i + 4 + len {
                            let n = s.read(&mut chunk).await.unwrap();
                            buf.extend_from_slice(&chunk[..n]);
                        }
                        break (head, buf[i + 4..i + 4 + len].to_vec());
                    }
                };
                let mut parts = head.split(' ');
                let (method, path) = (parts.next().unwrap(), parts.next().unwrap());
                let (path, query) = path.split_once('?').unwrap_or((path, ""));
                let seg: Vec<_> = path.trim_start_matches("/box/").split('/').collect();
                let reply = {
                    let mut rows = rows.lock().unwrap();
                    if method == "POST" {
                        if !rows.iter().any(|r| r.0 == seg[0] && r.1 == seg[1]) {
                            rows.push((seg[0].into(), seg[1].into(), body));
                        }
                        "ok".to_string()
                    } else {
                        let after: usize = query.trim_start_matches("after=").parse().unwrap();
                        rows.iter()
                            .enumerate()
                            .filter(|(i, r)| i + 1 > after && r.0 == seg[0])
                            .map(|(i, r)| {
                                format!("{} {}\n", i + 1, data_encoding::BASE64.encode(&r.2))
                            })
                            .collect()
                    }
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                    reply.len()
                );
                s.write_all(resp.as_bytes()).await.unwrap();
            });
        }
    });
    url
}

/// B is offline when A writes, and A is gone when B comes back: B gets it from the mailbox.
#[tokio::test(flavor = "multi_thread")]
async fn offline_message_through_mailbox() {
    let opts = Opts {
        mailbox: Some(mock_mailbox().await),
        ..Default::default()
    };
    let dirs: Vec<_> = (0..2).map(|_| tempfile::tempdir().unwrap()).collect();
    let (a, _rx_a) = Node::open(dirs[0].path(), "pa", &opts).await.unwrap();
    let (b, _rx_b) = Node::open(dirs[1].path(), "pb", &opts).await.unwrap();
    a.create_space().unwrap();
    b.join(&a.invite().await.unwrap()).await.unwrap();
    b.close().await;
    drop(b);

    a.send_text(&None, "while you were away", None).unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    a.close().await;
    drop(a);

    let (b, mut rx_b) = Node::open(dirs[1].path(), "pb", &opts).await.unwrap();
    let ev = loop {
        let ev = tokio::time::timeout(Duration::from_secs(10), rx_b.recv())
            .await
            .expect("timeout")
            .expect("closed");
        if matches!(ev.body, EventBody::Text { .. }) {
            break ev;
        }
    };
    assert!(matches!(&ev.body, EventBody::Text { text, .. } if text == "while you were away"));
    b.close().await;
}

/// A contact's pair group has its own mailbox topic: B, offline, still gets A's message (§4.2).
#[tokio::test(flavor = "multi_thread")]
async fn offline_contact_message_through_mailbox() {
    let opts = Opts {
        mailbox: Some(mock_mailbox().await),
        ..Default::default()
    };
    let dirs: Vec<_> = (0..2).map(|_| tempfile::tempdir().unwrap()).collect();
    let (a, _rx_a) = Node::open(dirs[0].path(), "pa", &opts).await.unwrap();
    let (b, _rx_b) = Node::open(dirs[1].path(), "pb", &opts).await.unwrap();
    a.create_space().unwrap();
    b.join(&a.contact_invite().await.unwrap()).await.unwrap();
    let kb = Some(b.device_id());
    b.close().await;
    drop(b);

    a.send_text(&kb, "лично тебе", None).unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    a.close().await;
    drop(a);

    let (b, mut rx_b) = Node::open(dirs[1].path(), "pb", &opts).await.unwrap();
    let ev = loop {
        let ev = tokio::time::timeout(Duration::from_secs(10), rx_b.recv())
            .await
            .expect("timeout")
            .expect("closed");
        if matches!(ev.body, EventBody::Text { .. }) {
            break ev;
        }
    };
    assert!(matches!(&ev.body, EventBody::Text { text, .. } if text == "лично тебе"));
    b.close().await;
}
