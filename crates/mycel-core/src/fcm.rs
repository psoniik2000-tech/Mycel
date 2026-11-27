//! FCM HTTP v1 wake-ups from the main server (the logic of `mailbox/worker.js`): a data-only
//! "something new" message, no content. The key is the Firebase service account JSON `fcm.json`.

use anyhow::{Context, Result, bail};
use data_encoding::{BASE64, BASE64URL_NOPAD};
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use serde::Deserialize;
use std::path::Path;

#[derive(Deserialize)]
struct Key {
    project_id: String,
    client_email: String,
    private_key: String,
}

pub(crate) struct Fcm {
    project: String,
    email: String,
    key: RsaKeyPair,
    http: reqwest::Client,
    /// OAuth token and its expiry (unix s).
    auth: tokio::sync::Mutex<(String, u64)>,
}

impl Fcm {
    /// None when the file is missing: pushes are off, the server works.
    pub(crate) fn load(file: &Path) -> Result<Option<Self>> {
        let text = match std::fs::read(file) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let k: Key = serde_json::from_slice(&text).context("fcm.json")?;
        let pem: String = k
            .private_key
            .lines()
            .filter(|l| !l.starts_with("-----"))
            .collect();
        let der = BASE64.decode(pem.trim().as_bytes())?;
        let key = RsaKeyPair::from_pkcs8(&der).map_err(|e| anyhow::anyhow!("fcm key: {e}"))?;
        Ok(Some(Self {
            project: k.project_id,
            email: k.client_email,
            key,
            http: crate::tls_client(std::time::Duration::from_secs(30))?,
            auth: Default::default(),
        }))
    }

    /// OAuth token for FCM (JWT signed with the RS256 key), cached until a minute before expiry.
    async fn token(&self) -> Result<String> {
        let mut auth = self.auth.lock().await;
        let now = crate::now_ms()? / 1000;
        if auth.1 > now + 60 {
            return Ok(auth.0.clone());
        }
        let enc = |v: serde_json::Value| BASE64URL_NOPAD.encode(v.to_string().as_bytes());
        let unsigned = format!(
            "{}.{}",
            enc(serde_json::json!({"alg": "RS256", "typ": "JWT"})),
            enc(serde_json::json!({
                "iss": self.email,
                "scope": "https://www.googleapis.com/auth/firebase.messaging",
                "aud": "https://oauth2.googleapis.com/token",
                "iat": now,
                "exp": now + 3600,
            }))
        );
        let mut sig = vec![0; self.key.public().modulus_len()];
        self.key
            .sign(
                &RSA_PKCS1_SHA256,
                &ring::rand::SystemRandom::new(),
                unsigned.as_bytes(),
                &mut sig,
            )
            .map_err(|_| anyhow::anyhow!("fcm sign"))?;
        let body = format!(
            "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer&assertion={unsigned}.{}",
            BASE64URL_NOPAD.encode(&sig)
        );
        let r = self
            .http
            .post("https://oauth2.googleapis.com/token")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await?
            .text()
            .await?;
        let j: serde_json::Value = serde_json::from_str(&r)?;
        let Some(t) = j["access_token"].as_str() else {
            bail!("oauth: {r}");
        };
        *auth = (
            t.to_string(),
            now + j["expires_in"].as_u64().unwrap_or(3600),
        );
        Ok(auth.0.clone())
    }

    /// Wakes one phone. Ok(false): the app was removed or the token replaced, forget it.
    pub(crate) async fn send(&self, token: &str) -> Result<bool> {
        let auth = self.token().await?;
        let msg = serde_json::json!({"message": {
            "token": token,
            "data": {"t": "msg"},
            "android": {"priority": "high", "ttl": "86400s"},
        }});
        let r = self
            .http
            .post(format!(
                "https://fcm.googleapis.com/v1/projects/{}/messages:send",
                self.project
            ))
            .bearer_auth(auth)
            .header("content-type", "application/json")
            .body(msg.to_string())
            .send()
            .await?;
        Ok(!matches!(r.status().as_u16(), 400 | 404))
    }
}
