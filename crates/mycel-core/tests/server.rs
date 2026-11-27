//! §4.6: the main server's guards (allowlist, rate and size limits) and media kept for
//! authors who went offline.
use mycel_core::mycel_proto::{BlobRef, MediaKind};
use mycel_core::{Node, Opts, dir, media};
use std::time::Duration;

/// reqwest is built without a default TLS provider.
fn tls() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

fn opts(url: &str) -> Opts {
    Opts {
        extra_relays: vec![url.into()],
        mailbox: Some(url.into()),
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn allowlist_closes_directory_and_mailbox() {
    let url = "http://127.0.0.1:34891";
    tls();
    let data = tempfile::tempdir().unwrap();
    let _server = mycel_core::spawn_anchor(url, data.path()).await.unwrap();
    let dirs = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
    let (a, _ra) = Node::open(dirs[0].path(), "pa", &opts(url)).await.unwrap();
    let (b, _rb) = Node::open(dirs[1].path(), "pb", &opts(url)).await.unwrap();
    for n in [&a, &b] {
        n.create_space().unwrap();
    }
    let http = reqwest::Client::new();
    let topic = "a".repeat(64);
    let post = |id: char| {
        http.post(format!("{url}/box/{topic}/{}", id.to_string().repeat(64)))
            .body("x")
            .send()
    };
    assert_eq!(post('1').await.unwrap().status(), 200, "open server");

    // Closed: A by key, B's username by name, B only for the directory.
    let a_key = data_encoding::HEXLOWER.encode(&a.device_id());
    mycel_core::allow(data.path(), &a_key, &[]).unwrap();
    mycel_core::allow(data.path(), "@bob_22", &["dir".into()]).unwrap();
    assert_eq!(mycel_core::allowlist(data.path()).unwrap().len(), 2);

    a.dir_register(url, "alice_1").await.unwrap();
    let e = b.dir_register(url, "bob_other").await.unwrap_err();
    assert!(e.to_string().contains(dir::DIR_DENIED), "{e}");
    b.dir_register(url, "bob_22").await.unwrap();
    // Lookups stay open.
    assert!(b.dir_lookup(url, "alice_1").await.unwrap().is_some());

    assert_eq!(post('2').await.unwrap().status(), 403, "unsigned");
    let r = http
        .get(format!("{url}/box/{topic}?after=0"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);

    assert!(mycel_core::deny(data.path(), &a_key).unwrap());
    let e = a.dir_register(url, "alice_1").await.unwrap_err();
    assert!(e.to_string().contains(dir::DIR_DENIED), "{e}");

    // Backup and restore keep the directory and the list.
    let f = data.path().join("backup.json");
    assert_eq!(mycel_core::backup(data.path(), &f).unwrap(), 3);
    let other = tempfile::tempdir().unwrap();
    assert_eq!(mycel_core::restore(other.path(), &f).unwrap(), 3);
    assert_eq!(mycel_core::allowlist(other.path()).unwrap().len(), 1);
    assert!(other.path().join("dir_key").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn rate_and_size_limits() {
    let url = "http://127.0.0.1:34892";
    tls();
    let data = tempfile::tempdir().unwrap();
    let _server = mycel_core::spawn_anchor(url, data.path()).await.unwrap();
    let http = reqwest::Client::new();
    let topic = "b".repeat(64);
    let r = http
        .post(format!("{url}/box/{topic}/{}", "c".repeat(64)))
        .body(vec![0u8; (1 << 20) + 1])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 413, "envelope over 1 MiB");
    let r = http
        .post(format!("{url}/blob/{}", "d".repeat(64)))
        .body("x")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401, "a blob needs a key for its quota");
    let mut codes = vec![];
    for _ in 0..620 {
        let r = http
            .get(format!("{url}/box/{topic}?after=0"))
            .send()
            .await
            .unwrap();
        codes.push(r.status().as_u16());
    }
    // The minute may turn over mid-loop; then the count starts again.
    assert!(codes.contains(&429), "per-IP limit");
    assert_eq!(codes[0], 200);
}

#[tokio::test(flavor = "multi_thread")]
async fn blob_comes_from_the_server_when_the_author_is_offline() {
    let url = "http://127.0.0.1:34893";
    tls();
    let data = tempfile::tempdir().unwrap();
    let _server = mycel_core::spawn_anchor(url, data.path()).await.unwrap();
    let dirs: Vec<_> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let (a, _ra) = Node::open(dirs[0].path(), "pa", &opts(url)).await.unwrap();
    let (b, _rb) = Node::open(dirs[1].path(), "pb", &opts(url)).await.unwrap();
    a.create_space().unwrap();
    b.join(&a.invite().await.unwrap()).await.unwrap();

    let src = dirs[2].path().join("photo.jpg");
    let bytes: Vec<u8> = (0..300_000u32).map(|i| (i * 7 % 251) as u8).collect();
    std::fs::write(&src, &bytes).unwrap();
    let meta = BlobRef {
        hash: [0; 32],
        size: 0,
        key: [0; 32],
        mime: "image/jpeg".into(),
        name: Some("photo.jpg".into()),
        preview: None,
        duration_ms: None,
        dims: None,
    };
    let ev = a
        .send_media(&None, MediaKind::Photo, &src, meta, None)
        .await
        .unwrap();
    let blob = media(&ev).unwrap().1.clone();
    // The upload runs in the background: wait until the server has it, then A leaves.
    let at = format!("{url}/blob/{}", data_encoding::HEXLOWER.encode(&blob.hash));
    for i in 0.. {
        if reqwest::get(&at).await.unwrap().status() == 200 {
            break;
        }
        assert!(i < 100, "never uploaded");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let a_id = a.device_id();
    a.close().await;
    drop(a);

    b.fetch(&blob, a_id, |_| {}).await.unwrap();
    assert_eq!(
        std::fs::read(b.media_path(&blob).await.unwrap()).unwrap(),
        bytes
    );
}
