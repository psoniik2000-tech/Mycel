//! The main server updates itself from the GitHub releases: the new binary replaces this one,
//! then starts with the same arguments and this process exits.

use anyhow::{Context, Result, bail};

const RELEASES: &str = "https://api.github.com/repos/psoniik2000-tech/Mycel/releases/latest";
const DOWNLOADS: &str = "https://github.com/psoniik2000-tech/Mycel/releases/download/";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

fn asset() -> Option<&'static str> {
    if cfg!(windows) {
        Some("mycel-server-win.exe")
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some("mycel-server-linux")
    } else {
        None
    }
}

/// `v1.2.3` → [1, 2, 3]; compared as lists, so 0.1.10 is newer than 0.1.9.
fn parts(v: &str) -> Option<Vec<u64>> {
    v.trim_start_matches('v')
        .split('.')
        .map(|p| p.parse().ok())
        .collect()
}

/// (version, download url) of a newer release that has a server for this platform.
pub async fn check() -> Result<Option<(String, String)>> {
    let Some(asset) = asset() else {
        return Ok(None);
    };
    let rel: serde_json::Value = serde_json::from_str(&mycel_core::https_text(RELEASES).await?)?;
    let tag = rel["tag_name"].as_str().unwrap_or_default();
    match (parts(tag), parts(VERSION)) {
        (Some(new), Some(old)) if new > old => {}
        _ => return Ok(None),
    }
    let url = rel["assets"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|a| a["name"].as_str() == Some(asset))
        .and_then(|a| a["browser_download_url"].as_str());
    Ok(url.map(|u| (tag.trim_start_matches('v').to_string(), u.to_string())))
}

/// Downloads `url` over this binary and restarts into it.
pub async fn install(url: &str) -> Result<()> {
    if !url.starts_with(DOWNLOADS) {
        bail!("not a Mycel release: {url}");
    }
    let exe = std::env::current_exe()?;
    let new = exe.with_extension("new");
    let old = exe.with_extension("old");
    mycel_core::https_download(url, &new, |_, _| {}).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&new, std::fs::Permissions::from_mode(0o755))?;
    }
    // A running exe may be renamed (Windows too), not overwritten.
    let _ = std::fs::remove_file(&old);
    std::fs::rename(&exe, &old).context("cannot move the running server aside")?;
    if let Err(e) = std::fs::rename(&new, &exe) {
        let _ = std::fs::rename(&old, &exe);
        return Err(e).context("cannot put the new server in place");
    }
    println!("updated, restarting…");
    std::process::Command::new(&exe)
        .args(std::env::args_os().skip(1))
        .env("MYCEL_RESTARTED", "1")
        .spawn()?;
    std::process::exit(0);
}

/// Every 6 hours (first a minute after start): a newer release is installed at once.
pub async fn auto() {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        match check().await {
            Ok(Some((v, url))) => {
                println!("update {v} found, installing");
                if let Err(e) = install(&url).await {
                    eprintln!("update failed: {e:#}");
                }
            }
            Ok(None) => {}
            Err(e) => eprintln!("update check failed: {e:#}"),
        }
        tokio::time::sleep(std::time::Duration::from_secs(6 * 3600 - 60)).await;
    }
}
