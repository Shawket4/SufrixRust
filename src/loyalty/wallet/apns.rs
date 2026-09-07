//! Telling a phone its pass has changed.
//!
//! Apple's model for passes is a **content-free** push: the notification carries
//! an empty payload, and the device responds by calling the pass web service for
//! whatever actually changed. So there is no balance in here, and nothing
//! sensitive crosses APNs.
//!
//! Token-based authentication (a `.p8` key signed ES256), not certificate-based:
//! one key covers every topic on the team, it does not expire annually, and it
//! avoids a second certificate to rotate. Configured by `LOYALTY_APNS_KEY_FILE`
//! (or `LOYALTY_APNS_KEY`), `LOYALTY_APNS_KEY_ID` and the team id the pass
//! already needs.
//!
//! Unset means no push: passes still issue, and a customer sees their new
//! balance the next time they open the pass. Degrade-safe, like everything else
//! in this module.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde::Serialize;

use crate::errors::AppError;

/// Apple refuses a provider token older than an hour and rate-limits minting, so
/// the token is cached and re-minted well inside that window.
const TOKEN_TTL: Duration = Duration::from_secs(45 * 60);

const PROD_HOST: &str = "https://api.push.apple.com";
const SANDBOX_HOST: &str = "https://api.sandbox.push.apple.com";

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.trim().is_empty())
}

fn key_material() -> Option<Vec<u8>> {
    if let Some(path) = env_nonempty("LOYALTY_APNS_KEY_FILE") {
        return std::fs::read(&path)
            .map_err(
                |e| tracing::error!(path = %path, error = %e, "cannot read LOYALTY_APNS_KEY_FILE"),
            )
            .ok();
    }
    env_nonempty("LOYALTY_APNS_KEY").map(|s| s.replace("\\n", "\n").into_bytes())
}

pub fn is_configured() -> bool {
    key_material().is_some()
        && env_nonempty("LOYALTY_APNS_KEY_ID").is_some()
        && super::apple::team_id().is_some()
}

/// Passes live on the production APNs host. The sandbox exists for a pass built
/// against a development profile; an operator switches with
/// `LOYALTY_APNS_SANDBOX=1` rather than by editing anything.
fn host() -> &'static str {
    match env_nonempty("LOYALTY_APNS_SANDBOX").as_deref() {
        Some("1" | "true") => SANDBOX_HOST,
        _ => PROD_HOST,
    }
}

#[derive(Serialize)]
struct Claims {
    iss: String,
    iat: usize,
}

static CACHED: Mutex<Option<(String, Instant)>> = Mutex::new(None);

/// A provider token, minted at most once every [`TOKEN_TTL`].
fn provider_token() -> Result<String, AppError> {
    if let Ok(guard) = CACHED.lock()
        && let Some((token, minted)) = guard.as_ref()
        && minted.elapsed() < TOKEN_TTL
    {
        return Ok(token.clone());
    }

    let (Some(key), Some(key_id), Some(team)) = (
        key_material(),
        env_nonempty("LOYALTY_APNS_KEY_ID"),
        super::apple::team_id(),
    ) else {
        return Err(AppError::ServiceUnavailable(
            "APNs is not configured".into(),
        ));
    };

    let mut header = Header::new(Algorithm::ES256);
    header.kid = Some(key_id);
    let claims = Claims {
        iss: team,
        iat: chrono::Utc::now().timestamp().max(0) as usize,
    };
    let encoding = EncodingKey::from_ec_pem(&key).map_err(|e| {
        tracing::error!(error = %e, "LOYALTY_APNS_KEY is not a usable EC private key (.p8)");
        AppError::ServiceUnavailable("APNs key is not usable".into())
    })?;
    let token =
        jsonwebtoken::encode(&header, &claims, &encoding).map_err(|_| AppError::Internal)?;

    if let Ok(mut guard) = CACHED.lock() {
        *guard = Some((token.clone(), Instant::now()));
    }
    Ok(token)
}

/// The outcome of one device's push, so the caller can prune what is gone.
pub enum PushOutcome {
    Delivered,
    /// Apple says this token is dead — the pass was deleted or the device wiped.
    /// The registration should go, or we push into the void forever.
    Unregistered,
    Failed(String),
}

/// Notify one device that a pass it holds has changed.
///
/// `apns-topic` is the PASS TYPE ID (not a bundle id) and `apns-push-type` is
/// `background` — both are what Apple requires for passes, and a wrong value
/// here is accepted at the HTTP level and then silently ignored by the device.
pub async fn push(push_token: &str, pass_type_id: &str) -> PushOutcome {
    let token = match provider_token() {
        Ok(t) => t,
        Err(e) => return PushOutcome::Failed(e.to_string()),
    };
    let url = format!("{}/3/device/{push_token}", host());
    let resp = reqwest::Client::new()
        .post(&url)
        .bearer_auth(token)
        .header("apns-topic", pass_type_id)
        .header("apns-push-type", "background")
        .header("apns-priority", "5")
        // Content-free by design: the device calls the pass web service back.
        .body("{}")
        .send()
        .await;

    match resp {
        Ok(r) if r.status().is_success() => PushOutcome::Delivered,
        Ok(r) if r.status().as_u16() == 410 => PushOutcome::Unregistered,
        Ok(r) => {
            let status = r.status();
            let body = r.text().await.unwrap_or_default();
            // 400 with BadDeviceToken means the same thing as 410 in practice:
            // this registration will never work again.
            if status.as_u16() == 400 && body.contains("BadDeviceToken") {
                PushOutcome::Unregistered
            } else {
                PushOutcome::Failed(format!("{status}: {body}"))
            }
        }
        Err(e) => PushOutcome::Failed(e.to_string()),
    }
}
