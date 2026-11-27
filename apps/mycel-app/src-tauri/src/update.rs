//! In-app updates (start and Settings → Приложение): the newest GitHub release, its installer
//! downloaded to the temp folder with progress, then run (Windows) or shown to the system installer (Android).

use serde::Serialize;
use std::path::Path;
use tauri::{AppHandle, Emitter};

const RELEASES: &str = "https://api.github.com/repos/psoniik2000-tech/Mycel/releases/latest";
const DOWNLOADS: &str = "https://github.com/psoniik2000-tech/Mycel/releases/download/";

#[derive(Serialize)]
pub struct Update {
    version: String,
    url: String,
}

#[derive(Serialize, Clone)]
struct Progress {
    done: u64,
    total: u64,
}

/// The release asset of this platform: `Mycel-<ver>` followed by this suffix.
fn suffix() -> Option<&'static str> {
    if cfg!(target_os = "android") {
        Some("-android.apk")
    } else if cfg!(windows) {
        Some("-windows-setup.exe")
    } else {
        None
    }
}

/// `1.2.3` or `v1.2.3` → numbers; compared as lists, so 0.1.10 is newer than 0.1.9.
fn parts(v: &str) -> Option<Vec<u64>> {
    v.trim_start_matches('v')
        .split('.')
        .map(|p| p.parse().ok())
        .collect()
}

/// The newest release if it is newer than this app and has an installer for this platform.
#[tauri::command]
pub async fn update_check(app: AppHandle) -> Result<Option<Update>, String> {
    let Some(suffix) = suffix() else {
        return Ok(None);
    };
    let json = mycel_core::https_text(RELEASES)
        .await
        .map_err(|e| e.to_string())?;
    let rel: serde_json::Value = serde_json::from_str(&json).map_err(|e| e.to_string())?;
    let tag = rel["tag_name"].as_str().unwrap_or_default();
    let cur = app.package_info().version.to_string();
    let (Some(new), Some(old)) = (parts(tag), parts(&cur)) else {
        return Ok(None);
    };
    if new <= old {
        return Ok(None);
    }
    let version = tag.trim_start_matches('v').to_string();
    let want = format!("Mycel-{version}{suffix}");
    let url = rel["assets"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|a| a["name"].as_str() == Some(want.as_str()))
        .and_then(|a| a["browser_download_url"].as_str());
    Ok(url.map(|url| Update {
        version,
        url: url.to_string(),
    }))
}

/// Downloads the installer (`update-progress` events), then runs it.
#[tauri::command]
pub async fn update_install(app: AppHandle, url: String, version: String) -> Result<(), String> {
    let suffix = suffix().ok_or("Обновление для этой платформы не предусмотрено")?;
    // Only from this repository's release downloads: the URL comes from the page.
    if !url.starts_with(DOWNLOADS) || version.contains(['/', '\\']) {
        return Err("Неверная ссылка обновления".into());
    }
    let to = std::env::temp_dir().join(format!("Mycel-{version}{suffix}"));
    let emit = app.clone();
    mycel_core::https_download(&url, &to, move |done, total| {
        let _ = emit.emit("update-progress", Progress { done, total });
    })
    .await
    .map_err(|e| e.to_string())?;
    install(&app, &to)
}

/// Windows: the installer runs and Mycel quits so the files can be replaced.
#[cfg(windows)]
fn install(app: &AppHandle, file: &Path) -> Result<(), String> {
    std::process::Command::new(file)
        .spawn()
        .map_err(|e| e.to_string())?;
    app.exit(0);
    Ok(())
}

/// Android: the system installer opens on the APK (`Update.kt`); the app stays up.
#[cfg(target_os = "android")]
fn install(_app: &AppHandle, file: &Path) -> Result<(), String> {
    crate::vault::with_class("org.mycel.app.Update", |env, class, ctx| {
        use jni::objects::{JObject, JValue};
        let path = JObject::from(env.new_string(file.to_string_lossy())?);
        env.call_static_method(
            class,
            "install",
            "(Landroid/content/Context;Ljava/lang/String;)V",
            &[JValue::Object(ctx), JValue::Object(&path)],
        )?;
        Ok(())
    })
}

#[cfg(not(any(windows, target_os = "android")))]
fn install(_app: &AppHandle, _file: &Path) -> Result<(), String> {
    Err("Обновление для этой платформы не предусмотрено".into())
}
