//! Google Wallet loyalty objects.
//!
//! Two things happen here:
//!   * **Save link** — a JWT signed with the issuer's service-account key,
//!     handed to the customer as `https://pay.google.com/gp/v/save/<jwt>`. It
//!     carries the whole loyalty object, so a member can be saved without the
//!     object having been created through the API first.
//!   * **Balance push** — a PATCH to the Wallet Objects API when points move.
//!     Google needs no device registry (that is Apple's model); the object is
//!     the record and every device holding it follows.
//!
//! Configured by `LOYALTY_GOOGLE_ISSUER_ID` and `LOYALTY_GOOGLE_SA_KEY` (the
//! service account's PEM private key) plus `LOYALTY_GOOGLE_SA_EMAIL`. Unset
//! means no Google button and no push — never an error at signup.

use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde::Serialize;
use serde_json::json;
use sqlx::PgPool;

use crate::errors::AppError;
use crate::loyalty::model::MemberRow;
use crate::loyalty::settings::LoyaltySettings;

const SAVE_URL_PREFIX: &str = "https://pay.google.com/gp/v/save/";
const WALLET_API: &str = "https://walletobjects.googleapis.com/walletobjects/v1";

fn issuer_id() -> Option<String> {
    std::env::var("LOYALTY_GOOGLE_ISSUER_ID")
        .ok()
        .filter(|s| !s.trim().is_empty())
}

fn sa_email() -> Option<String> {
    std::env::var("LOYALTY_GOOGLE_SA_EMAIL")
        .ok()
        .filter(|s| !s.trim().is_empty())
}

fn sa_key() -> Option<String> {
    std::env::var("LOYALTY_GOOGLE_SA_KEY")
        .ok()
        .filter(|s| !s.trim().is_empty())
        // A PEM in an env var almost always arrives with escaped newlines.
        .map(|s| s.replace("\\n", "\n"))
}

pub fn is_configured() -> bool {
    issuer_id().is_some() && sa_email().is_some() && sa_key().is_some()
}

/// The Wallet object id for a member. Google requires `<issuer>.<suffix>` with
/// the suffix restricted to alphanumerics, `.`, `_` and `-`; a UUID qualifies.
pub fn object_id(issuer: &str, member: &MemberRow) -> String {
    format!("{issuer}.{}", member.id)
}

/// The class every member of one org shares — this is what carries the tenant's
/// branding, so a class exists per org rather than per program.
pub fn class_id(issuer: &str, org_id: uuid::Uuid) -> String {
    format!("{issuer}.madar-{org_id}")
}

#[derive(Serialize)]
struct SaveClaims {
    iss: String,
    aud: &'static str,
    typ: &'static str,
    iat: usize,
    payload: serde_json::Value,
}

/// The loyalty CLASS: the programme itself, as opposed to one member's card.
///
/// Google will not accept an object whose class does not exist, so the class
/// rides in the same "save" JWT and is created on first use. That is the whole
/// reason this exists — without it every save fails with a class-not-found that
/// the customer sees only as a dead link.
///
/// One class per ORG, not per programme: it carries the tenant's identity, and
/// a customer looking at their wallet should see the shop's name on the card.
pub fn loyalty_class(
    issuer: &str,
    org_id: uuid::Uuid,
    org_name: &str,
    settings: &LoyaltySettings,
) -> serde_json::Value {
    let mut class = json!({
        "id": class_id(issuer, org_id),
        // Whose card this is. Always set, even when nothing else has been
        // configured — falling back to the programme name rather than leaving a
        // wallet entry with no owner on it.
        "issuerName": if org_name.trim().is_empty() { settings.program_name.as_str() } else { org_name },
        "programName": settings.program_name,
        // `UNDER_REVIEW` is what a class inserted through a save JWT must carry;
        // Google promotes it when the issuer account is approved. `APPROVED`
        // here is rejected outright.
        "reviewStatus": "UNDER_REVIEW",
    });
    if let Some(logo) = settings.pass_logo_url.as_deref().filter(|s| !s.trim().is_empty()) {
        class["programLogo"] = json!({ "sourceUri": { "uri": logo } });
    }
    if let Some(bg) = settings.pass_background_color.as_deref() {
        class["hexBackgroundColor"] = json!(bg);
    }
    class
}

/// The loyalty object as Google models it.
///
/// `loyaltyPoints.balance.int` is the number the customer sees on the pass, and
/// `accountId` is the member token — so the barcode and the account agree, and
/// a scan resolves the same member whichever wallet produced it.
pub fn loyalty_object(
    issuer: &str,
    member: &MemberRow,
    settings: &LoyaltySettings,
) -> serde_json::Value {
    let program = settings.program_name.clone();
    let mode = settings.mode();
    let balance = member.balance_in(mode);
    json!({
        "id": object_id(issuer, member),
        "classId": class_id(issuer, member.org_id),
        "state": "ACTIVE",
        "accountId": member.member_token,
        "accountName": member.name,
        "loyaltyPoints": {
            "label": balance_label(mode),
            "balance": { "int": balance }
        },
        // The progress line, mirroring the Apple pass's secondary field so the
        // two wallets say the same thing.
        "textModulesData": [{
            "header": program,
            "body": progress_line(balance, settings.default_reward_cost),
            "id": "progress"
        }],
        "barcode": {
            "type": "QR_CODE",
            "value": member.member_token,
            "alternateText": member.name
        }
    })
}

/// A stamp card drawn in text, when the target is small enough to read as one.
///
/// "●●●○○" says "three of five orders" at a glance in a way "3 / 5" does not —
/// it is the paper punch card everyone already understands. Above
/// [`MAX_STAMPS`] the dots stop being countable and become noise, so a points
/// programme (100, 250…) keeps the plain figure.
const MAX_STAMPS: i32 = 12;

pub fn stamps(balance: i32, threshold: i32) -> Option<String> {
    if threshold <= 0 || threshold > MAX_STAMPS {
        return None;
    }
    let filled = balance.clamp(0, threshold);
    Some(
        "●".repeat(filled as usize) + &"○".repeat((threshold - filled).max(0) as usize),
    )
}

/// "30 / 100 to your next reward", or the earned line once it is reached. A
/// small target gets the punch card instead of the arithmetic.
pub fn progress_line(balance: i32, threshold: i32) -> String {
    if threshold > 0 && balance >= threshold {
        return "Reward earned — ask at the counter".to_string();
    }
    match stamps(balance, threshold) {
        Some(dots) => format!("{dots}   {balance} / {threshold}"),
        None => format!("{balance} / {threshold} to your next reward"),
    }
}

/// What this program calls what it collects, for the pass's field label.
pub fn balance_label(mode: crate::loyalty::earn::Mode) -> &'static str {
    match mode {
        crate::loyalty::earn::Mode::Points => "Points",
        // "Orders", not "Visits": the customer counts the things they bought,
        // and that is the word the counter uses back to them.
        crate::loyalty::earn::Mode::Visits => "Orders",
    }
}

/// The "Save to Google Wallet" URL, or `None` when Google is not configured.
pub fn save_url(
    member: &MemberRow,
    settings: &LoyaltySettings,
    org_name: &str,
) -> Result<Option<String>, AppError> {
    let (Some(issuer), Some(email), Some(key)) = (issuer_id(), sa_email(), sa_key()) else {
        return Ok(None);
    };
    let claims = SaveClaims {
        iss: email,
        aud: "google",
        typ: "savetowallet",
        iat: chrono::Utc::now().timestamp().max(0) as usize,
        // BOTH, deliberately: the class must exist before the object that
        // points at it, and sending them together lets Google create it on the
        // first save rather than needing a separate provisioning step.
        payload: json!({
            "loyaltyClasses": [loyalty_class(&issuer, member.org_id, org_name, settings)],
            "loyaltyObjects": [loyalty_object(&issuer, member, settings)],
        }),
    };
    let encoding = EncodingKey::from_rsa_pem(key.as_bytes()).map_err(|e| {
        tracing::error!(error = %e, "LOYALTY_GOOGLE_SA_KEY is not a usable RSA PEM");
        AppError::ServiceUnavailable("Google Wallet key is not usable".into())
    })?;
    let jwt = jsonwebtoken::encode(&Header::new(Algorithm::RS256), &claims, &encoding)
        .map_err(|_| AppError::Internal)?;
    Ok(Some(format!("{SAVE_URL_PREFIX}{jwt}")))
}

/// Exchange the service-account key for an access token (the JWT bearer grant).
async fn access_token() -> Result<String, AppError> {
    let (Some(email), Some(key)) = (sa_email(), sa_key()) else {
        return Err(AppError::ServiceUnavailable(
            "Google Wallet is not configured".into(),
        ));
    };
    let now = chrono::Utc::now().timestamp().max(0) as usize;
    let claims = json!({
        "iss": email,
        "scope": "https://www.googleapis.com/auth/wallet_object.issuer",
        "aud": "https://oauth2.googleapis.com/token",
        "iat": now,
        "exp": now + 3600,
    });
    let encoding = EncodingKey::from_rsa_pem(key.as_bytes()).map_err(|_| AppError::Internal)?;
    let assertion = jsonwebtoken::encode(&Header::new(Algorithm::RS256), &claims, &encoding)
        .map_err(|_| AppError::Internal)?;

    // Built by hand rather than with `.form()`: reqwest is pulled in with only
    // the `json` feature and the form encoder is not compiled in. The two values
    // are a fixed grant name and a JWT (base64url + dots), neither of which
    // needs percent-encoding, so the body is exact.
    let body =
        format!("grant_type=urn:ietf:params:oauth:grant-type:jwt-bearer&assertion={assertion}");
    let resp = reqwest::Client::new()
        .post("https://oauth2.googleapis.com/token")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .map_err(|e| AppError::ServiceUnavailable(format!("Google token endpoint: {e}")))?;
    if !resp.status().is_success() {
        return Err(AppError::ServiceUnavailable(format!(
            "Google token endpoint returned {}",
            resp.status()
        )));
    }
    let body: serde_json::Value = resp.json().await.map_err(|_| AppError::Internal)?;
    body["access_token"]
        .as_str()
        .map(str::to_string)
        .ok_or(AppError::Internal)
}

/// PATCH the member's balance onto their Wallet object.
pub async fn push_balance(pool: &PgPool, member: &MemberRow) -> Result<(), AppError> {
    let Some(issuer) = issuer_id() else {
        return Ok(());
    };
    let Some(object_id) = member.google_object_id.clone() else {
        return Ok(());
    };
    let settings = crate::loyalty::settings::load_scope(pool, member.org_id, None)
        .await?
        .unwrap_or_else(|| LoyaltySettings::defaults(member.org_id, None));

    let token = access_token().await?;
    let mode = settings.mode();
    let balance = member.balance_in(mode);
    let body = json!({
        "loyaltyPoints": {
            "label": balance_label(mode),
            "balance": { "int": balance }
        },
        "textModulesData": [{
            "header": settings.program_name,
            "body": progress_line(balance, settings.default_reward_cost),
            "id": "progress"
        }]
    });
    let resp = reqwest::Client::new()
        .patch(format!("{WALLET_API}/loyaltyObject/{object_id}"))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .map_err(|e| AppError::ServiceUnavailable(format!("Google Wallet PATCH: {e}")))?;
    if !resp.status().is_success() {
        return Err(AppError::ServiceUnavailable(format!(
            "Google Wallet PATCH returned {}",
            resp.status()
        )));
    }
    let _ = issuer;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_save_payload_carries_the_class_as_well_as_the_object() {
        // Google refuses an object whose class does not exist, and the customer
        // sees only a dead link. The class must ride along.
        let s = LoyaltySettings::defaults(uuid::Uuid::nil(), None);
        let class = loyalty_class("3388000000000000000", uuid::Uuid::nil(), "RUE Coffee", &s);
        assert_eq!(class["issuerName"], "RUE Coffee");
        assert_eq!(class["reviewStatus"], "UNDER_REVIEW");
        assert_eq!(
            class["id"],
            format!("3388000000000000000.madar-{}", uuid::Uuid::nil())
        );
    }

    #[test]
    fn a_nameless_org_still_gets_an_issuer_on_the_card() {
        let s = LoyaltySettings::defaults(uuid::Uuid::nil(), None);
        let class = loyalty_class("338", uuid::Uuid::nil(), "  ", &s);
        assert_eq!(class["issuerName"], s.program_name);
    }

    #[test]
    fn progress_counts_down_then_announces() {
        assert_eq!(progress_line(30, 100), "30 / 100 to your next reward");
        assert_eq!(
            progress_line(100, 100),
            "Reward earned — ask at the counter"
        );
        assert_eq!(
            progress_line(130, 100),
            "Reward earned — ask at the counter"
        );
    }

    #[test]
    fn a_zero_threshold_never_claims_a_reward_is_ready() {
        // Defensive: the column is CHECK (> 0), but a pass that told every
        // customer their reward was ready would be a bad way to find out.
        assert_eq!(progress_line(0, 0), "0 / 0 to your next reward");
    }
}
