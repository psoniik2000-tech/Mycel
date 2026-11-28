//! §4.6: two main servers as a cluster: a name registered on one is found on the other, and the
//! app falls back to the second server while the first is off.
use mycel_core::{Node, Opts, cluster, dir};
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn names_reach_the_other_server_and_apps_fall_back() {
    let (ua, ub) = ("http://127.0.0.1:34891", "http://127.0.0.1:34892");
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let sa = mycel_core::spawn_anchor(ua, da.path()).await.unwrap();
    let sb = mycel_core::spawn_anchor(ub, db.path()).await.unwrap();
    let (la, lb) = (
        format!("{ua}#{}", sa.dir_id().unwrap()),
        format!("{ub}#{}", sb.dir_id().unwrap()),
    );
    cluster::add_peer(da.path(), &lb).unwrap();
    cluster::add_peer(db.path(), &la).unwrap();
    assert_eq!(cluster::members_of(ua).await, [la.clone(), lb.clone()]);

    let opts = Opts {
        extra_relays: vec![ua.into(), ub.into()],
        mailbox: Some(format!("{ua} {ub}")),
        ..Default::default()
    };
    let dirs = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
    let (a, _ra) = Node::open(dirs[0].path(), "pa", &opts).await.unwrap();
    let (b, _rb) = Node::open(dirs[1].path(), "pb", &opts).await.unwrap();
    for n in [&a, &b] {
        n.create_space().unwrap();
    }
    a.dir_register(&la, "alice_1").await.unwrap();

    // Copied to B within a sync round.
    let mut found = None;
    for _ in 0..90 {
        found = b.dir_lookup(&lb, "alice_1").await.unwrap();
        if found.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert_eq!(found.expect("synced").key, a.device_id());
    let e = b.dir_register(&lb, "alice_1").await.unwrap_err();
    assert!(e.to_string().contains(dir::DIR_TAKEN), "{e}");

    // A is off: the cluster setting still works through B.
    drop(sa);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let both = format!("{la} {lb}");
    let r = b.dir_lookup(&both, "alice_1").await.unwrap().unwrap();
    assert_eq!(r.key, a.device_id());
}
