//! The main server's control panel: a page on 127.0.0.1 only (open it in a browser on the same PC).
//! Status, the allowlist, a backup download and the update button.

use anyhow::Result;
use std::path::{Path, PathBuf};
use std::time::Instant;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const PAGE: &str = include_str!("panel.html");

pub struct Panel {
    pub dir: PathBuf,
    pub url: String,
    pub id: String,
    pub started: Instant,
}

pub async fn run(p: Panel, port: u16) -> Result<()> {
    let l = TcpListener::bind(("127.0.0.1", port)).await?;
    println!("control panel: http://127.0.0.1:{port}");
    let p = std::sync::Arc::new(p);
    loop {
        let (s, _) = l.accept().await?;
        let p = p.clone();
        tokio::spawn(async move {
            if let Err(e) = conn(&p, s, port).await {
                eprintln!("panel: {e:#}");
            }
        });
    }
}

// shortcut: one request per connection, body by Content-Length only; enough for our own page.
async fn conn(p: &Panel, mut s: TcpStream, port: u16) -> Result<()> {
    let mut buf = vec![];
    let mut chunk = [0u8; 8192];
    let head_end = loop {
        let n = s.read(&mut chunk).await?;
        anyhow::ensure!(n > 0, "closed");
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        anyhow::ensure!(buf.len() < 64 << 10, "header too big");
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let header = |name: &str| {
        head.lines()
            .filter_map(|l| l.split_once(':'))
            .find(|(k, _)| k.trim().eq_ignore_ascii_case(name))
            .map(|(_, v)| v.trim().to_string())
    };
    let len: usize = header("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    anyhow::ensure!(len < 1 << 20, "body too big");
    while buf.len() < head_end + len {
        let n = s.read(&mut chunk).await?;
        anyhow::ensure!(n > 0, "closed");
        buf.extend_from_slice(&chunk[..n]);
    }
    let body = String::from_utf8_lossy(&buf[head_end..head_end + len]).to_string();
    let mut first = head.split_whitespace();
    let (method, path) = (first.next().unwrap_or(""), first.next().unwrap_or(""));
    // Another site in the browser must not drive the panel: only our host name (no DNS
    // rebinding), and API calls carry a header a cross-site form cannot set.
    let host = header("host").unwrap_or_default();
    let ok_host = [format!("127.0.0.1:{port}"), format!("localhost:{port}")].contains(&host);
    let (code, ctype, out): (u16, &str, Vec<u8>) = if !ok_host {
        (403, "text/plain", b"wrong host".to_vec())
    } else if (method, path) == ("GET", "/") {
        (200, "text/html; charset=utf-8", PAGE.into())
    } else if header("x-mycel").as_deref() != Some("1") {
        (403, "text/plain", b"no x-mycel header".to_vec())
    } else {
        match api(p, method, path, body.trim()).await {
            Ok((ctype, v)) => (200, ctype, v),
            Err(e) => (400, "text/plain; charset=utf-8", format!("{e:#}").into()),
        }
    };
    let extra = if path == "/api/backup" && code == 200 {
        "Content-Disposition: attachment; filename=\"mycel-server-backup.bin\"\r\n"
    } else {
        ""
    };
    let head = format!(
        "HTTP/1.1 {code} X\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n",
        out.len()
    );
    s.write_all(head.as_bytes()).await?;
    s.write_all(&out).await?;
    Ok(())
}

async fn api(p: &Panel, method: &str, path: &str, body: &str) -> Result<(&'static str, Vec<u8>)> {
    let json = |v: serde_json::Value| Ok(("application/json", v.to_string().into_bytes()));
    match (method, path) {
        ("GET", "/api/status") => {
            let names = std::fs::read_to_string(p.dir.join("directory.json"))
                .ok()
                .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
                .and_then(|v| v["names"].as_object().map(|m| m.len()))
                .unwrap_or(0);
            let allow: Vec<_> = mycel_core::allowlist(&p.dir)?
                .into_iter()
                .map(|(who, roles)| serde_json::json!({"who": who, "roles": roles}))
                .collect();
            json(serde_json::json!({
                "version": crate::update::VERSION,
                "url": p.url,
                "id": p.id,
                "uptime": p.started.elapsed().as_secs(),
                "push": p.dir.join("fcm.json").exists(),
                "names": names,
                "bytes": size(&p.dir),
                "dir": p.dir.display().to_string(),
                "allow": allow,
            }))
        }
        ("POST", "/api/allow") => {
            let mut w = body.split_whitespace();
            let who = w.next().unwrap_or_default();
            let roles: Vec<String> = w.map(str::to_string).collect();
            json(serde_json::json!(mycel_core::allow(&p.dir, who, &roles)?))
        }
        ("POST", "/api/deny") => json(serde_json::json!(mycel_core::deny(&p.dir, body)?)),
        ("GET", "/api/backup") => {
            let f = std::env::temp_dir().join(format!("mycel-backup-{}.bin", std::process::id()));
            mycel_core::backup(&p.dir, &f)?;
            let v = std::fs::read(&f);
            let _ = std::fs::remove_file(&f);
            Ok(("application/octet-stream", v?))
        }
        ("GET", "/api/update") => json(match crate::update::check().await? {
            Some((v, _)) => serde_json::json!(v),
            None => serde_json::Value::Null,
        }),
        ("POST", "/api/update") => match crate::update::check().await? {
            Some((_, url)) => {
                // Answer first; the install restarts the process.
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    if let Err(e) = crate::update::install(&url).await {
                        eprintln!("update failed: {e:#}");
                    }
                });
                json(serde_json::json!("installing"))
            }
            None => json(serde_json::json!("up to date")),
        },
        _ => anyhow::bail!("no such call"),
    }
}

fn size(p: &Path) -> u64 {
    let Ok(rd) = std::fs::read_dir(p) else {
        return 0;
    };
    rd.flatten()
        .map(|e| match e.metadata() {
            Ok(m) if m.is_dir() => size(&e.path()),
            Ok(m) => m.len(),
            Err(_) => 0,
        })
        .sum()
}
