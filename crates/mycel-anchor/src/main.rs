//! mycel-anchor: CLI node. Password from MYCEL_PASSWORD or the first stdin line.

use anyhow::{Result, bail};
use mycel_core::Node;
use mycel_core::mycel_proto::{Event, EventBody};
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, BufReader};

const USAGE: &str = "usage: mycel-anchor [--dir DIR] [--port PORT] [--no-relay] <init | id | members | chat | invite | join <link>>
  init: create a space (you are its admin); invite: print a one-time link, then chat; join: enter a space, then chat";

#[tokio::main]
async fn main() -> Result<()> {
    let mut dir = PathBuf::from("mycel-data");
    let mut port = 0u16;
    let mut relay = true;
    let mut rest = vec![];
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--dir" => dir = args.next().unwrap_or_default().into(),
            "--port" => port = args.next().unwrap_or_default().parse()?,
            "--no-relay" => relay = false,
            _ => rest.push(a),
        }
    }

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let password = match std::env::var("MYCEL_PASSWORD") {
        Ok(p) => p,
        Err(_) => {
            // shortcut: password is echoed; hide input (rpassword) when the CLI gets real users.
            eprint!("password: ");
            lines.next_line().await?.unwrap_or_default()
        }
    };

    let (node, mut rx) = Node::open(&dir, &password, port, relay).await?;
    let rest: Vec<&str> = rest.iter().map(String::as_str).collect();
    match rest[..] {
        ["init"] => {
            node.create_space()?;
            println!("space created in {}", dir.display());
        }
        ["id"] => println!("{}", node.ticket(relay).await),
        ["members"] => {
            let admin = node.admin()?;
            for m in node.members() {
                let tag = if m == admin { " (admin)" } else { "" };
                println!("{}{tag}", short(&m));
            }
        }
        ["chat"] | ["invite"] | ["join", _] => {
            if let ["join", link] = rest[..] {
                node.join(link).await?;
                println!("joined, {} members", node.members().len());
            }
            if rest[..] == ["invite"] {
                println!("{}", node.invite(relay).await?);
            }
            let me = node.device_id();
            for ev in node.history()? {
                print_event(&ev, me);
            }
            tokio::spawn(async move {
                while let Some(ev) = rx.recv().await {
                    print_event(&ev, me);
                }
            });
            while let Some(line) = lines.next_line().await? {
                if !line.trim().is_empty() {
                    node.send_text(&line)?;
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
    if let EventBody::Text { text, .. } = &ev.body {
        let who = if ev.author_device == me {
            "me".to_string()
        } else {
            short(&ev.author_device)
        };
        println!("[{who}] {text}");
    }
}
