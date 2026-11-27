use iroh_relay::server::{RelayConfig, Server, ServerConfig};
use mycel_net::{EndpointAddr, Net, SecretKey};
use mycel_proto::SyncMsg;

/// A peer whose stored address is stale (n0 relay blocked, old ports) is still reached
/// through the relay both sides use.
#[tokio::test]
async fn reach_peer_on_shared_relay() {
    let mut cfg = ServerConfig::default();
    cfg.relay = Some(RelayConfig::new(([127, 0, 0, 1], 0)));
    let relay = Server::spawn(cfg).await.unwrap();
    let url = format!("http://{}", relay.http_addr().unwrap());
    let (a, ..) = Net::bind(SecretKey::generate(), 0, std::slice::from_ref(&url), false)
        .await
        .unwrap();
    let (b, mut inbox, ..) = Net::bind(SecretKey::generate(), 0, &[url], false)
        .await
        .unwrap();
    b.online().await;
    let stale = EndpointAddr::new(b.id())
        .with_relay_url("https://euc1-1.relay.n0.iroh.iroh.link./".parse().unwrap());
    a.send(
        stale,
        &SyncMsg::Have {
            ids: vec![],
            reply: false,
        },
    )
    .await
    .unwrap();
    let (from, _) = tokio::time::timeout(std::time::Duration::from_secs(10), inbox.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(from, a.id());
}

/// With n0 on, a space relay (anchor) replaces the public relays as the home relay.
#[tokio::test]
async fn space_relay_replaces_n0() {
    let mut cfg = ServerConfig::default();
    cfg.relay = Some(RelayConfig::new(([127, 0, 0, 1], 0)));
    let relay = Server::spawn(cfg).await.unwrap();
    let url = format!("http://{}", relay.http_addr().unwrap());
    let (a, ..) = Net::bind(SecretKey::generate(), 0, std::slice::from_ref(&url), true)
        .await
        .unwrap();
    a.online().await;
    let home: Vec<_> = a.addr().relay_urls().map(|u| u.to_string()).collect();
    assert_eq!(home, [format!("{url}/")]);
}

/// A dropped sync connection is reported, so the node can reconnect and catch up at once.
#[tokio::test]
async fn reports_lost_link() {
    let (a, ..) = Net::bind(SecretKey::generate(), 0, &[], false)
        .await
        .unwrap();
    let (b, _inbox, ..) = Net::bind(SecretKey::generate(), 0, &[], false)
        .await
        .unwrap();
    let mut lost = a.on_lost();
    let msg = SyncMsg::Have {
        ids: vec![],
        reply: false,
    };
    a.send(b.addr(), &msg).await.unwrap();
    assert_eq!(a.connected(), 1);
    b.close().await;
    let peer = tokio::time::timeout(std::time::Duration::from_secs(10), lost.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(peer, b.id());
    assert_eq!(a.connected(), 0);
}

/// Our only relay goes down (anchor PC off): the fallback relays take over, and once ours
/// answers again we leave them.
#[tokio::test]
async fn falls_back_while_own_relay_is_down() {
    let spawn = |port: u16| async move {
        let mut cfg = ServerConfig::default();
        cfg.relay = Some(RelayConfig::new(([127, 0, 0, 1], port)));
        Server::spawn(cfg).await.unwrap()
    };
    let own = spawn(0).await;
    let port = own.http_addr().unwrap().port();
    let own_url = format!("http://127.0.0.1:{port}/");
    let backup = spawn(0).await;
    let backup_url = format!("http://{}/", backup.http_addr().unwrap());
    let (a, ..) = Net::bind(
        SecretKey::generate(),
        0,
        std::slice::from_ref(&own_url),
        false,
    )
    .await
    .unwrap();
    a.set_fallback(vec![backup_url.parse().unwrap()]);
    a.online().await;
    let home = |a: &Net| a.addr().relay_urls().map(|u| u.to_string()).next();
    let until = |a: Net, url: String, secs: u64| async move {
        let t = std::time::Instant::now();
        while !(a.relay_up() && home(&a).as_deref() == Some(url.as_str())) {
            assert!(
                t.elapsed().as_secs() < secs,
                "home relay is {:?}, not {url}",
                home(&a)
            );
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    };
    until(a.clone(), own_url.clone(), 10).await;
    own.shutdown().await.unwrap();
    until(a.clone(), backup_url, 60).await;
    let _own = spawn(port).await;
    until(a.clone(), own_url, 90).await;
}
