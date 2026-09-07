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
use crate::orgs::branding::OrgBrand;

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

/// Accepts `LOYALTY_GOOGLE_SA_KEY_FILE` (a path) or `LOYALTY_GOOGLE_SA_KEY`
/// (inline PEM). Prefer the file: see [`super::key_material`].
fn sa_key() -> Option<Vec<u8>> {
    super::key_material("LOYALTY_GOOGLE_SA_KEY")
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
/// Where Madar's own mark is served, for a shop that has not uploaded one.
/// Google REQUIRES a class to carry a logo.
pub const MADAR_LOGO_PATH: &str = "/public/loyalty/brand/logo.png";

/// The brand comes from the ORGANISATION, like Apple's. It used to be read from
/// `loyalty_settings`, whose UI was removed when branding moved to the org — so
/// the class carried no logo and no colour, and every Android card came back in
/// Google's default white.
pub fn loyalty_class(
    issuer: &str,
    org_id: uuid::Uuid,
    brand: &OrgBrand,
    settings: &LoyaltySettings,
) -> serde_json::Value {
    let mut class = json!({
        "id": class_id(issuer, org_id),
        // Whose card this is. Always set, even when nothing else has been
        // configured — falling back to the programme name rather than leaving a
        // wallet entry with no owner on it.
        "issuerName": if brand.name.trim().is_empty() { settings.program_name.as_str() } else { brand.name.as_str() },
        "programName": settings.program_name,
        // `UNDER_REVIEW` is what a class inserted through a save JWT must carry;
        // Google promotes it when the issuer account is approved. `APPROVED`
        // here is rejected outright.
        "reviewStatus": "UNDER_REVIEW",
        // Google FETCHES this, so it must be absolute and publicly reachable —
        // unlike Apple's, which is packed into the archive as bytes.
        "hexBackgroundColor": brand.palette.background,
    });
    // REQUIRED by Google, and the cause of "Something went wrong" on a save
    // that still routed to the app: a loyalty class without a `programLogo` is
    // rejected, and a shop with no logo produced exactly that. Madar's own mark
    // stands in, so a class is always valid.
    //
    // Absolute, because Google fetches this from its own servers — a
    // site-relative path resolves against nothing there.
    let logo = brand
        .logo_url
        .as_deref()
        .filter(|s| s.starts_with("http://") || s.starts_with("https://"))
        .map(|s| s.to_string())
        .or_else(|| super::absolute_api_url(MADAR_LOGO_PATH));
    if let Some(uri) = logo {
        class["programLogo"] = json!({ "sourceUri": { "uri": uri } });
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
    locations: &[super::PassLocation],
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
        },
        // Geofences the card to the shop's branches, so it surfaces on the
        // phone when the customer is there — Google's counterpart to Apple's
        // lock-screen `locations`. Only branches whose coordinates an admin has
        // actually set; a branch without them simply does not surface.
        "locations": locations
            .iter()
            .map(|l| json!({
                "kind": "walletobjects#latLongPoint",
                "latitude": l.latitude,
                "longitude": l.longitude,
            }))
            .collect::<Vec<_>>(),
    })
}

/// The stepper, drawn in text, when the target is small enough to read as one.
///
/// `●─●─●─○─○` rather than `●●●○○`. Loose dots are a paper punch card: they say
/// how many, and nothing about direction. Joining them makes a JOURNEY — the
/// eye reads left to right, sees where it is and how far is left, which is the
/// question a customer actually asks of a stamp card. It is the same object the
/// web card draws with real geometry (`stamp-row.tsx`), reduced to the only
/// thing a Wallet field can hold, which is a line of text.
///
/// The two glyphs are chosen to survive a pass: `●`/`○` (U+25CF/U+25CB) and the
/// box-drawing `─` (U+2500) are in the system fonts on both platforms, and the
/// connector is the one character designed to meet its neighbours with no gap.
/// State is never carried by the drawing alone — [`progress_line`] always puts
/// the figures beside it, so a font that substitutes still reads.
///
/// Above [`MAX_STEPS`] the steps stop being countable and become texture, so a
/// points programme (100, 250…) keeps the plain figure. The cap matches the web
/// card's, so the card in the phone and the card on the page never disagree.
const MAX_STEPS: i32 = 12;

pub fn stepper(balance: i32, threshold: i32) -> Option<String> {
    if threshold <= 0 || threshold > MAX_STEPS {
        return None;
    }
    let filled = balance.clamp(0, threshold);
    let step = |i: i32| if i < filled { "●" } else { "○" };
    let mut out = String::from(step(0));
    for i in 1..threshold {
        out.push('\u{2500}');
        out.push_str(step(i));
    }
    Some(out)
}

/// "30 / 100 to your next reward", or the earned line once it is reached. A
/// small target gets the stepper as well as the arithmetic, never instead of
/// it: the drawing is the glance, the figures are the fact.
pub fn progress_line(balance: i32, threshold: i32) -> String {
    if threshold > 0 && balance >= threshold {
        return "Reward earned — ask at the counter".to_string();
    }
    match stepper(balance, threshold) {
        Some(steps) => format!("{steps}   {balance} / {threshold}"),
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

/// Google's cap on a JWT carried in a save URL.
///
/// Past this the save page fails — "Something went wrong", while still opening
/// the Wallet app, which is a remarkably hard symptom to attribute. A class and
/// an object embedded together measured about 2,150 characters for a shop with
/// six branches, so the link was over the limit from the day it was written and
/// grew with every branch added.
const MAX_SAVE_JWT: usize = 1800;

/// Create the class and the member's object, and return a link that REFERENCES
/// the object rather than carrying it.
///
/// This is the shape Google documents for production, and it is the only shape
/// that fits: a reference-only JWT is a few hundred characters whatever the
/// shop looks like, so branches, a long name and a logo URL can no longer push
/// a customer's save link over the limit.
///
/// The writes are idempotent and happen ONCE per member — the object id is kept
/// on the row, so an existing member's link costs no Google calls at all. A
/// failure here is reported and swallowed by the caller: a wallet that will not
/// provision must not take a signup down with it.
pub async fn save_url(
    pool: &PgPool,
    member: &MemberRow,
    settings: &LoyaltySettings,
    brand: &OrgBrand,
    locations: &[super::PassLocation],
) -> Result<Option<String>, AppError> {
    let (Some(issuer), Some(email), Some(key)) = (issuer_id(), sa_email(), sa_key()) else {
        return Ok(None);
    };
    let object_id = match &member.google_object_id {
        // Already provisioned: nothing to do but sign.
        Some(id) => id.clone(),
        None => {
            let token = access_token().await?;
            ensure_class(&token, &issuer, member.org_id, brand, settings).await?;
            let id = ensure_object(&token, &issuer, member, settings, locations).await?;
            // Remembered so this is a one-time cost, and so `push_balance` has
            // something to PATCH — it reads this column, which nothing used to
            // write, so no Google pass ever saw a balance change.
            sqlx::query("UPDATE loyalty_customers SET google_object_id = $2 WHERE id = $1")
                .bind(member.id)
                .bind(&id)
                .execute(pool)
                .await?;
            id
        }
    };

    let claims = SaveClaims {
        iss: email,
        aud: "google",
        typ: "savetowallet",
        iat: chrono::Utc::now().timestamp().max(0) as usize,
        payload: json!({ "loyaltyObjects": [{ "id": object_id }] }),
    };
    let encoding = EncodingKey::from_rsa_pem(&key).map_err(|e| {
        tracing::error!(error = %e, "LOYALTY_GOOGLE_SA_KEY is not a usable RSA PEM");
        AppError::ServiceUnavailable("Google Wallet key is not usable".into())
    })?;
    let jwt = jsonwebtoken::encode(&Header::new(Algorithm::RS256), &claims, &encoding)
        .map_err(|_| AppError::Internal)?;
    if jwt.len() > MAX_SAVE_JWT {
        tracing::error!(
            len = jwt.len(),
            "loyalty: Google save JWT is over the URL limit; the save page will fail"
        );
    }
    Ok(Some(format!("{SAVE_URL_PREFIX}{jwt}")))
}

/// Create the org's class, or bring an existing one up to date.
///
/// PATCH on conflict rather than leaving it: a class created once and never
/// touched again would keep a shop's first logo and colours forever, and there
/// is no other moment that would notice the branding had changed.
async fn ensure_class(
    token: &str,
    issuer: &str,
    org_id: uuid::Uuid,
    brand: &OrgBrand,
    settings: &LoyaltySettings,
) -> Result<(), AppError> {
    let body = loyalty_class(issuer, org_id, brand, settings);
    let id = class_id(issuer, org_id);
    let http = reqwest::Client::new();
    let resp = http
        .post(format!("{WALLET_API}/loyaltyClass"))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .map_err(|e| AppError::ServiceUnavailable(format!("Google Wallet class: {e}")))?;
    if resp.status().is_success() {
        return Ok(());
    }
    if resp.status() != reqwest::StatusCode::CONFLICT {
        return Err(google_error("creating the loyalty class", resp).await);
    }
    let resp = http
        .patch(format!("{WALLET_API}/loyaltyClass/{id}"))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .map_err(|e| AppError::ServiceUnavailable(format!("Google Wallet class: {e}")))?;
    if resp.status().is_success() {
        return Ok(());
    }
    Err(google_error("updating the loyalty class", resp).await)
}

/// Create the member's object. Returns its id either way.
async fn ensure_object(
    token: &str,
    issuer: &str,
    member: &MemberRow,
    settings: &LoyaltySettings,
    locations: &[super::PassLocation],
) -> Result<String, AppError> {
    let id = object_id(issuer, member);
    let resp = reqwest::Client::new()
        .post(format!("{WALLET_API}/loyaltyObject"))
        .bearer_auth(token)
        .json(&loyalty_object(issuer, member, settings, locations))
        .send()
        .await
        .map_err(|e| AppError::ServiceUnavailable(format!("Google Wallet object: {e}")))?;
    // A member re-opening their card already has one; that is a success.
    if resp.status().is_success() || resp.status() == reqwest::StatusCode::CONFLICT {
        return Ok(id);
    }
    Err(google_error("creating the loyalty object", resp).await)
}

/// Google's own words for why it refused, in the log.
///
/// Worth the round trip: the alternative is a status code, and every failure
/// mode here — an unlinked service account, a wrong issuer id, a class Google
/// will not accept — arrives as the same 400 or 403 with the reason only in the
/// body.
async fn google_error(what: &str, resp: reqwest::Response) -> AppError {
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    tracing::error!(status = %status, body = %body, "loyalty: Google refused {what}");
    AppError::ServiceUnavailable(format!("Google Wallet refused {what} ({status})"))
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
    let encoding = EncodingKey::from_rsa_pem(&key).map_err(|_| AppError::Internal)?;
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

    /// The save link must not grow with the shop.
    ///
    /// It used to carry the whole class AND the whole object. For a shop with
    /// six branches that measured about 2,150 characters against Google's
    /// ~1,800 limit for a JWT in a save URL — so the save page failed with
    /// "Something went wrong" while still opening the Wallet app, and every
    /// branch added made it worse. The link now references an object Google
    /// already holds, so its size is fixed whatever the shop looks like.
    #[test]
    fn the_save_link_references_the_object_rather_than_carrying_it() {
        let s = LoyaltySettings::defaults(uuid::Uuid::nil(), None);
        let brand = OrgBrand {
            name: "RUE Coffee".into(),
            logo_url: Some(
                "https://api.madar-pos.cloud/uploads/logos/3f2a1b8c-7d4e-4a91-bc22-0e5f61a8d904.png"
                    .into(),
            ),
            palette: crate::orgs::branding::Palette::default(),
            logo_is_mark: true,
        };
        let m = super::super::apple::tests::member();
        let locs: Vec<super::super::PassLocation> = (0..6)
            .map(|i| super::super::PassLocation {
                name: format!("RUE Coffee — Branch {i}"),
                latitude: 30.0444 + i as f64 * 0.01,
                longitude: 31.2357 + i as f64 * 0.01,
            })
            .collect();

        // Base64 costs a third on top, and an RS256 signature adds ~350 chars.
        let as_jwt = |v: &serde_json::Value| {
            let raw = serde_json::to_string(v).unwrap();
            40 + raw.len().div_ceil(3) * 4 + 350
        };

        // What we send now: an id, and nothing else.
        let reference =
            json!({ "loyaltyObjects": [{ "id": object_id("3388000000022345678", &m) }] });
        assert!(
            as_jwt(&reference) < MAX_SAVE_JWT,
            "a reference link must fit: {}",
            as_jwt(&reference)
        );

        // What we used to send, on the same shop. Kept as a measurement, so the
        // reason for the REST provisioning is checkable rather than folklore.
        let embedded = json!({
            "loyaltyClasses": [loyalty_class("3388000000022345678", uuid::Uuid::nil(), &brand, &s)],
            "loyaltyObjects": [loyalty_object("3388000000022345678", &m, &s, &locs)],
        });
        assert!(
            as_jwt(&embedded) > MAX_SAVE_JWT,
            "embedding the card was over the limit, which is why it is gone: {}",
            as_jwt(&embedded)
        );
    }

    /// The class Google is asked to hold: whose card it is, and what it looks
    /// like. Sent over REST before the first save, not embedded in the link.
    #[test]
    fn the_class_carries_the_shop_and_a_review_status_google_accepts() {
        let s = LoyaltySettings::defaults(uuid::Uuid::nil(), None);
        let brand = OrgBrand {
            name: "RUE Coffee".into(),
            logo_url: Some("https://api.madar-pos.cloud/api/uploads/logos/rue.png".into()),
            palette: crate::orgs::branding::Palette {
                background: "#7B1E3A".into(),
                foreground: "#EFF3F4".into(),
                accent: "#C8607F".into(),
            },
            logo_is_mark: true,
        };
        let class = loyalty_class("3388000000000000000", uuid::Uuid::nil(), &brand, &s);
        assert_eq!(class["issuerName"], "RUE Coffee");
        assert_eq!(class["reviewStatus"], "UNDER_REVIEW");
        assert_eq!(
            class["id"],
            format!("3388000000000000000.madar-{}", uuid::Uuid::nil())
        );
        // The shop's brand reaches the Android card. These used to be read from
        // `loyalty_settings`, which nothing writes any more, so the class went
        // out with no logo and no colour.
        assert_eq!(class["hexBackgroundColor"], "#7B1E3A");
        assert_eq!(
            class["programLogo"]["sourceUri"]["uri"],
            "https://api.madar-pos.cloud/api/uploads/logos/rue.png"
        );
    }

    #[test]
    fn google_is_never_pointed_at_a_relative_logo() {
        // Google FETCHES the logo from its own servers, so a site-relative path
        // resolves against nothing — and a class with NO logo is rejected
        // outright, which is what "something went wrong" on an otherwise
        // working save link turned out to be. Madar's mark stands in.
        let _guard = super::super::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // SAFETY: the lock makes this the only thread touching the environment.
        unsafe {
            std::env::set_var("PUBLIC_LOYALTY_BASE_URL", "https://loyalty.madar-pos.cloud");
        }
        let s = LoyaltySettings::defaults(uuid::Uuid::nil(), None);
        let brand = OrgBrand {
            name: "RUE".into(),
            logo_url: Some("/api/uploads/logos/rue.png".into()),
            ..OrgBrand::default()
        };
        let class = loyalty_class("338", uuid::Uuid::nil(), &brand, &s);
        assert_eq!(
            class["programLogo"]["sourceUri"]["uri"],
            format!("https://loyalty.madar-pos.cloud/api{MADAR_LOGO_PATH}"),
            "a relative logo falls back to Madar's, never to no logo at all"
        );
        unsafe {
            std::env::remove_var("PUBLIC_LOYALTY_BASE_URL");
        }
        // The colour still lands — it needs no fetching.
        assert_eq!(
            class["hexBackgroundColor"],
            crate::orgs::branding::MADAR_TEAL
        );
    }

    #[test]
    fn a_nameless_org_still_gets_an_issuer_on_the_card() {
        let s = LoyaltySettings::defaults(uuid::Uuid::nil(), None);
        let class = loyalty_class("338", uuid::Uuid::nil(), &OrgBrand::default(), &s);
        assert_eq!(class["issuerName"], s.program_name);
    }

    #[test]
    fn progress_counts_down_then_announces() {
        assert_eq!(progress_line(30, 100), "30 / 100 to your next reward");
        // The stepper never replaces the figures — a font that substitutes a
        // glyph must still leave a readable card.
        assert_eq!(progress_line(3, 5), "●─●─●─○─○   3 / 5");
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
    fn the_stepper_joins_its_steps_and_knows_when_to_stop() {
        // Joined, so it reads as a journey rather than a handful of dots.
        assert_eq!(stepper(3, 5).as_deref(), Some("●─●─●─○─○"));
        assert_eq!(stepper(0, 3).as_deref(), Some("○─○─○"));
        assert_eq!(stepper(3, 3).as_deref(), Some("●─●─●"));
        // A single step has nothing to join to.
        assert_eq!(stepper(0, 1).as_deref(), Some("○"));
        // Clamped both ways: a redemption leaves a remainder and an adjustment
        // can overshoot; neither should draw a broken row.
        assert_eq!(stepper(9, 3).as_deref(), Some("●─●─●"));
        assert_eq!(stepper(-4, 3).as_deref(), Some("○─○─○"));
        // Past the cap the steps stop being countable, so the figures stand alone.
        assert_eq!(stepper(30, 100), None);
        assert_eq!(stepper(1, MAX_STEPS + 1), None);
        assert!(stepper(1, MAX_STEPS).is_some());
        assert_eq!(stepper(1, 0), None);
    }

    #[test]
    fn a_zero_threshold_never_claims_a_reward_is_ready() {
        // Defensive: the column is CHECK (> 0), but a pass that told every
        // customer their reward was ready would be a bad way to find out.
        assert_eq!(progress_line(0, 0), "0 / 0 to your next reward");
    }
}
