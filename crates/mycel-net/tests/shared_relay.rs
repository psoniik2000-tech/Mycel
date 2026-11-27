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
    let (a, ..) = Net::bind(SecretKey::generate(), 0, &[url.clone()], false)
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
