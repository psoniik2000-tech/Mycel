//! Several main servers as one (DESIGN.md §4.6): each lists the others in `cluster.json`
//! (`url#id` links, `mycel-server cluster add|remove`, applied without restart). Every 30 s a
//! server copies the others' username records (signed by their owners, so a peer cannot forge
//! one). Apps learn the members at `/cluster` and use each as relay, mailbox and directory
//! fallback, so the service lives on while one server is off.

use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::Path;

const FILE: &str = "cluster.json";
/// How often the directory is copied from each peer.
pub(crate) const SYNC: std::time::Duration = std::time::Duration::from_secs(30);

/// The other servers of the cluster (`url#id`).
pub fn peers(data: &Path) -> Vec<String> {
    std::fs::read(data.join(FILE))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save(data: &Path, l: &[String]) -> Result<()> {
    let f = data.join(FILE);
    let tmp = f.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(l)?)?;
    std::fs::rename(&tmp, &f)?;
    Ok(())
}

/// `url#id` → (url, directory key).
pub(crate) fn parse(link: &str) -> Result<(String, [u8; 32])> {
    let (url, id) = link
        .trim()
        .split_once('#')
        .context("need url#id, as the server prints it")?;
    let id: mycel_net::EndpointId = id.parse().context("bad server id")?;
    let _: mycel_net::RelayUrl = url.parse().context("bad server address")?;
    Ok((url.trim_end_matches('/').to_string(), *id.as_bytes()))
}

/// Adds a peer (its link as it prints at start); returns it normalized.
pub fn add_peer(data: &Path, link: &str) -> Result<String> {
    let (url, id) = parse(link)?;
    let link = format!("{url}#{}", mycel_net::EndpointId::from_bytes(&id)?);
    let mut l = peers(data);
    l.retain(|p| parse(p).map_or(true, |(u, _)| u != url));
    l.push(link.clone());
    save(data, &l)?;
    Ok(link)
}

/// Removes a peer by its link or url; false = it was not listed.
pub fn remove_peer(data: &Path, url: &str) -> Result<bool> {
    let url = url.split('#').next().unwrap_or("").trim_end_matches('/');
    let mut l = peers(data);
    let n = l.len();
    l.retain(|p| p.split('#').next().unwrap_or("").trim_end_matches('/') != url);
    save(data, &l)?;
    Ok(l.len() != n)
}

/// Directory keys of the peers: only they may copy the whole directory.
pub(crate) fn peer_ids(data: &Path) -> HashSet<[u8; 32]> {
    peers(data)
        .iter()
        .filter_map(|p| parse(p).ok())
        .map(|(_, id)| id)
        .collect()
}

/// What `/cluster` answers: this server first, then the peers, one `url#id` per line.
pub(crate) fn members(data: &Path, me: Option<&String>) -> String {
    me.into_iter()
        .cloned()
        .chain(peers(data))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The members of the cluster a main server setting (one or more `url[#id]`, by spaces) belongs
/// to, from the first server that answers; the setting itself when none does.
pub async fn members_of(server: &str) -> Vec<String> {
    for s in server.split_whitespace() {
        let url = s.split('#').next().unwrap_or("").trim_end_matches('/');
        let Ok(b) = crate::Mailbox::new(url) else {
            continue;
        };
        let r = b
            .http
            .get(format!("{url}/cluster"))
            .timeout(std::time::Duration::from_secs(8))
            .send()
            .await
            .and_then(|r| r.error_for_status());
        if let Ok(r) = r
            && let Ok(t) = r.text().await
        {
            let l: Vec<String> = t
                .lines()
                .filter(|l| parse(l).is_ok())
                .map(str::to_string)
                .collect();
            if !l.is_empty() {
                return l;
            }
        }
    }
    server.split_whitespace().map(str::to_string).collect()
}
