//! mycel-anchor: CLI node. Password from MYCEL_PASSWORD or the first stdin line.

use anyhow::{Context, Result, bail};
use mycel_core::mycel_proto::{Event, EventBody};
use mycel_core::{Node, Opts};
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};

mod panel;
mod update;

const USAGE: &str = "usage: mycel-anchor [--dir DIR] [--port PORT] [--n0-relay] [--keep-days N] [--panel PORT] [--no-update]
                    <http://PUBLIC-IP:PORT | init | id | members | chat | invite | join <link> | serve <relay-url>
                     | allow <key|@name> [dir] [box] [files] | deny <key|@name> | allowlist
                     | backup <file> | restore <file>>
  http://PUBLIC-IP:PORT: the main server only (no password, no space): relay, username directory,
        mailbox and media for offline users, phone pushes with DIR/fcm.json (Firebase service
        account), data in DIR (default mycel-data); a control panel on http://127.0.0.1:3341
        (--panel PORT, 0 = off); updates itself from the GitHub releases (--no-update: never)
  allow/deny/allowlist: a closed main server: once DIR/allow.json lists anyone, only they may register
        a username (dir), use the mailbox (box) and keep media (files); no roles = all three
  backup/restore: the directory, its key and the allowlist in one file (restore with the server stopped)
  init: create a space (you are its admin); invite: print a one-time link, then chat; join: enter a space, then chat
  serve http://PUBLIC-IP:PORT: run 24/7 as an anchor: an iroh-relay on PORT (TCP) for the members,
        store-and-forward for offline members, events kept --keep-days (default 30),
        and the username directory (mycel/dir/1, data in DIR)
  --n0-relay: also use the public n0 relays (off by default)";

#[tokio::main]
async fn main() -> Result<()> {
    let mut dir = PathBuf::from("mycel-data");
    let mut opts = Opts::default();
    let mut keep_days = 30u64;
    let (mut panel_port, mut auto_update) = (3341u16, true);
    let mut rest = vec![];
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--dir" => dir = args.next().unwrap_or_default().into(),
            "--port" => opts.port = args.next().unwrap_or_default().parse()?,
            "--n0-relay" => opts.n0 = true,
            "--keep-days" => keep_days = args.next().unwrap_or_default().parse()?,
            "--panel" => panel_port = args.next().unwrap_or_default().parse()?,
            "--no-update" => auto_update = false,
            _ => rest.push(a),
        }
    }
    let rest: Vec<&str> = rest.iter().map(String::as_str).collect();

    // Main server administration: files in DIR, applied at once (no restart).
    match rest[..] {
        ["allow", who, ref roles @ ..] => {
            let roles: Vec<String> = roles.iter().map(|r| r.to_string()).collect();
            println!("allowed {}", mycel_core::allow(&dir, who, &roles)?);
            return Ok(());
        }
        ["deny", who] => {
            let had = mycel_core::deny(&dir, who)?;
            println!("{}", if had { "removed" } else { "was not listed" });
            return Ok(());
        }
        ["allowlist"] => {
            let l = mycel_core::allowlist(&dir)?;
            if l.is_empty() {
                println!("empty: the server is open to everyone");
            }
            for (who, roles) in l {
                println!("{who} {}", roles.join(" "));
            }
            return Ok(());
        }
        ["backup", file] => {
            let n = mycel_core::backup(&dir, file.as_ref())?;
            println!("{n} files saved to {file}");
            return Ok(());
        }
        ["restore", file] => {
            let n = mycel_core::restore(&dir, file.as_ref())?;
            println!("{n} files restored into {}", dir.display());
            return Ok(());
        }
        _ => {}
    }

    // Main server (§4.6): not a member of any space, so no password and no node.
    if let [url] = rest[..]
        && url.starts_with("http")
    {
        // Started by an update: the old process is still letting go of the port.
        if std::env::var_os("MYCEL_RESTARTED").is_some() {
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        let s = mycel_core::spawn_anchor(url, &dir).await?;
        let id = s.dir_id().context("directory did not start")?;
        println!(
            "main server up: {url}#{id}
data in {}; pushes {}; allowlist: {}; Ctrl+C to stop",
            dir.display(),
            if dir.join("fcm.json").exists() {
                "on"
            } else {
                "off (no fcm.json)"
            },
            match mycel_core::allowlist(&dir)?.len() {
                0 => "empty, open to everyone".to_string(),
                n => format!("{n} entries"),
            }
        );
        println!("version {}", update::VERSION);
        if panel_port != 0 {
            let p = panel::Panel {
                dir: dir.clone(),
                url: url.to_string(),
                id: id.to_string(),
                started: std::time::Instant::now(),
            };
            tokio::spawn(async move {
                if let Err(e) = panel::run(p, panel_port).await {
                    eprintln!("control panel failed: {e:#}");
                }
            });
        }
        if auto_update {
            tokio::spawn(update::auto());
        }
        tokio::signal::ctrl_c().await?;
        return Ok(());
    }

    // The relay runs in this process; kept alive until exit.
    let _relay = match rest[..] {
        ["serve", url] => {
            opts.own_relay = Some(url.to_string());
            opts.keep_blobs = true;
            Some(mycel_core::spawn_anchor(url, &dir).await?)
        }
        _ => None,
    };

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let password = match std::env::var("MYCEL_PASSWORD") {
        Ok(p) => p,
        Err(_) => {
            // shortcut: password is echoed; hide input (rpassword) when the CLI gets real users.
            eprint!("password: ");
            lines.next_line().await?.unwrap_or_default()
        }
    };

    let (node, mut rx) = Node::open(&dir, &password, &opts).await?;
    match rest[..] {
        ["init"] => {
            node.create_space()?;
            println!("space created in {}", dir.display());
        }
        ["id"] => println!("{}", node.ticket().await),
        ["members"] => {
            let admin = node.admin()?;
            for m in node.members() {
                let tag = if m == admin { " (admin)" } else { "" };
                println!("{}{tag}", short(&m));
            }
        }
        ["serve", url] => {
            node.admin()
                .context("join a space first: mycel-anchor join <link>")?;
            println!("anchor up, relay {url}, keeping {keep_days} days");
            if let Some(id) = _relay.as_ref().and_then(|r| r.dir_id()) {
                println!("main server (usernames): {url}#{id}");
            }
            // The anchor reaches its own relay through the public URL, like everyone else.
            if !node.relay_connected().await {
                eprintln!(
                    "WARNING: cannot reach own relay at {url}. Members outside this network will not get through.\n  \
                     Check: the TCP port is forwarded on the router to this PC; the firewall allows it;\n  \
                     the IP is really public (the router's WAN IP equals 2ip.ru; otherwise the provider's NAT, CGNAT);\n  \
                     the router supports NAT loopback (opening {url} from this PC should answer)."
                );
            }
            tokio::spawn(async move { while rx.recv().await.is_some() {} });
            loop {
                // Retries blobs whose authors were offline when their events came.
                node.clone().fetch_missing();
                match node.prune(keep_days).await {
                    Ok(0) => {}
                    Ok(n) => println!("pruned {n} old records"),
                    Err(e) => eprintln!("prune failed: {e}"),
                }
                tokio::time::sleep(Duration::from_secs(3600)).await;
            }
        }
        ["chat"] | ["invite"] | ["join", _] => {
            if let ["join", link] = rest[..] {
                node.join(link).await?;
                println!("joined, {} members", node.members().len());
            }
            if rest[..] == ["invite"] {
                println!("{}", node.invite().await?);
            }
            let me = node.device_id();
            for ev in node.history(&None)? {
                print_event(&ev, me);
            }
            tokio::spawn(async move {
                while let Some(ev) = rx.recv().await {
                    print_event(&ev, me);
                }
            });
            while let Some(line) = lines.next_line().await? {
                if !line.trim().is_empty() {
                    node.send_text(&None, &line, None)?;
                }
            }
        }
        _ => bail!(USAGE),
    }
    node.close().await;
    Ok(())
}

fn short(id: &[u8; 32]) -> String {
    id[..4].iter().map(|b| format!("{b:02x}")).collect()
}

fn print_event(ev: &Event, me: [u8; 32]) {
    let text = match &ev.body {
        EventBody::Text { text, .. } => text.clone(),
        EventBody::Media { kind, blob, .. } => format!("<{kind:?} {} bytes>", blob.size),
        _ => return,
    };
    let who = if ev.author_device == me {
        "me".to_string()
    } else {
        short(&ev.author_device)
    };
    println!("[{who}] {text}");
}
