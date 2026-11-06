//! mycel-anchor: CLI node. Password from MYCEL_PASSWORD or the first stdin line.

use anyhow::{Result, bail};
use mycel_core::Node;
use mycel_core::mycel_proto::{Event, EventBody};
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, BufReader};

const USAGE: &str = "usage: mycel-anchor [--dir DIR] [--port PORT] [--no-relay] <init | id | chat | connect <ticket>>  (connect saves the peer, then chats)";

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
    if let ["connect", ticket] = rest[..] {
        node.add_peer(ticket)?;
    }
    match rest[..] {
        ["init"] => println!("initialized {}", dir.display()),
        ["id"] => println!("{}", node.ticket(relay).await),
        ["chat"] | ["connect", _] => {
            let me = node.device_id();
            for ev in node.history()? {
                print_event(&ev, me);
            }
            println!("ticket: {}", node.ticket(relay).await);
            tokio::spawn(async move {
                while let Some(ev) = rx.recv().await {
                    print_event(&ev, me);
                }
            });
            while let Some(line) = lines.next_line().await? {
                if !line.trim().is_empty() {
                    node.send_text(&line).await?;
                }
            }
        }
        _ => bail!(USAGE),
    }
    node.close().await;
    Ok(())
}

fn print_event(ev: &Event, me: [u8; 32]) {
    if let EventBody::Text { text, .. } = &ev.body {
        let who = if ev.author_device == me {
            "me".to_string()
        } else {
            ev.author_device[..4]
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect()
        };
        println!("[{who}] {text}");
    }
}
