//! Apple Wallet / Google Wallet passes.
//!
//! **Degrade-safe behind config, exactly like `WHATSAPP_SERVICE_URL`.** Madar
//! does not hold the credentials yet: with the env vars unset, issuing a pass is
//! skipped and logged, signup still succeeds, and the customer simply gets no
//! "add to wallet" button. Nothing else in the program depends on a pass
//! existing — the member's token is minted at signup either way, so the barcode
//! can be printed on a receipt or read from the site in the meantime.
//!
//! One Madar-owned issuer serves every tenant (one Apple Pass Type ID, one
//! Google issuer), with the tenant's identity coming from `loyalty_settings`
//! branding. The credential lookup is per-org from the start, so per-tenant
//! certificates can be dropped in later without a schema change.

pub mod apns;
pub mod apple;
pub mod google;
pub mod web_service;

use serde::Serialize;
use sqlx::PgPool;
use utoipa::ToSchema;
use uuid::Uuid;

use super::model::MemberRow;
use super::settings::LoyaltySettings;
use crate::errors::AppError;

/// Both wallets cap the locations they will act on at ten; more are ignored, so
/// sending more only costs bytes on every device holding the pass.
const MAX_LOCATIONS: usize = 10;

/// A branch as a pass surfaces it: Apple puts it on the lock screen when the
/// customer is nearby, Google geofences the object the same way.
///
/// Shared by both wallets, which is why it lives here rather than in either.
#[derive(Debug, Clone)]
pub struct PassLocation {
    pub latitude: f64,
    pub longitude: f64,
    pub name: String,
}

/// Every branch of the org that has coordinates.
///
/// These are the columns the staff-geofencing work already added and the branch
/// dialog already edits — the program needed no new location UI, only a reason
/// to read them.
pub async fn locations_for_org(pool: &PgPool, org_id: Uuid) -> Result<Vec<PassLocation>, AppError> {
    let rows: Vec<(f64, f64, String)> = sqlx::query_as(
        "SELECT latitude, longitude, name FROM branches \
          WHERE org_id = $1 AND is_active AND deleted_at IS NULL \
            AND latitude IS NOT NULL AND longitude IS NOT NULL \
          ORDER BY name LIMIT $2",
    )
    .bind(org_id)
    .bind(MAX_LOCATIONS as i64)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(latitude, longitude, name)| PassLocation {
            latitude,
            longitude,
            name,
        })
        .collect())
}

/// What signup hands the customer. Either side may be absent: a tenant with only
/// Google credentials configured shows one button, not a broken one.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PassLinks {
    /// Downloads the signed `.pkpass`. Site-relative, because the signup page
    /// is served from the same origin as the API — so a pass needs a
    /// CERTIFICATE, not a configured base URL.
    pub apple_url: Option<String>,
    /// `https://pay.google.com/gp/v/save/<jwt>`.
    pub google_url: Option<String>,
    /// False when neither wallet is configured — the site shows the member's
    /// QR on the page instead of dead buttons.
    pub any: bool,
}

/// Read PEM/key material from `KEY_FILE` (a path) or `KEY` (inline).
///
/// Shared by both wallets on purpose. Apple had the file form and Google did
/// not, which meant a documented `LOYALTY_GOOGLE_SA_KEY_FILE` was silently
/// ignored and the wallet read as unconfigured — no button, no error, nothing
/// in a log. One helper, so the two cannot drift again.
///
/// The file form is what production should use: a private key in an env var has
/// to have its newlines escaped, shows up in `docker inspect`, and lands in any
/// process listing that dumps the environment. The inline form stays for local
/// development and tests.
pub(crate) fn key_material(key: &str) -> Option<Vec<u8>> {
    if let Some(path) = std::env::var(format!("{key}_FILE"))
        .ok()
        .filter(|s| !s.trim().is_empty())
    {
        return match std::fs::read(&path) {
            Ok(bytes) => Some(bytes),
            Err(e) => {
                // Configured but unreadable is an operator error worth shouting
                // about: silently falling back to "not configured" looks
                // identical to never having set it up.
                tracing::error!(path = %path, error = %e, "cannot read {}_FILE", key);
                None
            }
        };
    }
    std::env::var(key)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.replace("\\n", "\n").into_bytes())
}

/// The customer-facing base URL (`loyalty.madar-pos.cloud`), trailing slash
/// trimmed. Unset degrades softly: the Apple link is omitted rather than the
/// signup failing.
pub fn loyalty_base() -> Option<String> {
    std::env::var("PUBLIC_LOYALTY_BASE_URL")
        .ok()
        .map(|s| s.trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
}

/// The scheme and host of a URL, with no path. `None` if it is not absolute.
///
/// Pure so the parsing is testable: getting this wrong points every device on
/// the estate at an address that does not answer.
pub fn origin_of(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let host = rest.split('/').next().filter(|h| !h.is_empty())?;
    let scheme = if url.starts_with("https://") {
        "https"
    } else {
        "http"
    };
    Some(format!("{scheme}://{host}"))
}

/// Where Apple's devices should call to refresh a pass.
///
/// Absolute by necessity — it is baked into a file on someone's phone, so it
/// cannot be relative like the download link is.
///
/// Falls back to the API's own origin, because that is where these endpoints
/// actually live and because tying pass UPDATES to `PUBLIC_LOYALTY_BASE_URL`
/// made one unset variable mean "no pass ever updates again" — silently, and
/// unfixably for every pass already issued under it.
pub fn web_service_url() -> Option<String> {
    if let Some(base) = loyalty_base() {
        return Some(format!("{base}/api/wallet"));
    }
    // `UPLOADS_BASE_URL` is already set wherever images work, and it points at
    // the API.
    let uploads = std::env::var("UPLOADS_BASE_URL").ok()?;
    Some(format!("{}/wallet", origin_of(&uploads)?))
}

/// Build both "add to wallet" links for a member.
pub fn links_for(
    member: &MemberRow,
    settings: &LoyaltySettings,
    brand: &crate::orgs::branding::OrgBrand,
    locations: &[PassLocation],
) -> PassLinks {
    // Relative to the site root, and under `/api/` — which is what nginx proxies
    // to the backend. Two things this deliberately is NOT:
    //
    //   * not a PAGE path (`/pass/…`), which falls through to the SPA fallback
    //     and hands the customer an HTML document iOS refuses without a word;
    //   * not absolute, so it does NOT depend on `PUBLIC_LOYALTY_BASE_URL`. The
    //     page is served from this same origin, so signing a pass is all it
    //     takes to offer one. The base URL is for the COUNTER QR's target and
    //     the pass's self-update address — not for handing over the file.
    let apple_url = apple::is_configured().then(|| {
        format!(
            "/api/public/loyalty/pass/{}/apple.pkpass",
            member.member_token
        )
    });
    let google_url = google::save_url(member, settings, brand, locations).unwrap_or_else(|e| {
        // A misconfigured issuer must not take the signup down with it.
        tracing::warn!(error = %e, "loyalty: could not build the Google Wallet save link");
        None
    });
    PassLinks {
        any: apple_url.is_some() || google_url.is_some(),
        apple_url,
        google_url,
    }
}

/// Push the member's new balance to whichever wallets hold their pass.
///
/// Fire-and-forget, copying `delivery::whatsapp::send_message`: the till is
/// never blocked on Apple's or Google's servers, and a failure is reported
/// rather than surfaced. The balance in the database is the truth; the pass is a
/// cache of it that catches up.
pub fn push_update(pool: &PgPool, customer_id: Uuid) {
    if !apple::is_configured() && !google::is_configured() {
        tracing::debug!(
            customer_id = %customer_id,
            "loyalty: no wallet configured — skipping pass update"
        );
        return;
    }
    let pool = pool.clone();
    tokio::spawn(async move {
        if let Err(e) = push_update_inner(&pool, customer_id).await {
            use crate::observability::report::{Failure, report};
            report(Failure::new("loyalty", "push_pass_update"), &e);
        }
    });
}

async fn push_update_inner(pool: &PgPool, customer_id: Uuid) -> Result<(), AppError> {
    let Some(member) = super::model::find_by_id(pool, customer_id).await? else {
        return Ok(());
    };
    if member.google_object_id.is_some() {
        google::push_balance(pool, &member).await?;
    }
    if member.apple_serial.is_some() {
        apple::notify_devices(pool, &member).await?;
    }
    sqlx::query("UPDATE loyalty_customers SET pass_updated_at = now() WHERE id = $1")
        .bind(customer_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Serialises the tests that read and write the wallet environment.
///
/// Env is process-global: two tests toggling `LOYALTY_APPLE_*` race, and the
/// failure looks like a signing bug rather than a test-harness one. Every test
/// in this module tree that touches those variables takes this first.
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loyalty::model::MemberRow;
    use uuid::Uuid;

    fn member() -> MemberRow {
        MemberRow {
            id: Uuid::nil(),
            org_id: Uuid::nil(),
            name: "Ali".into(),
            phone: "201000000001".into(),
            member_token: "Mabcdefghijklmnopqrstuv".into(),
            points_balance: 0,
            visits_balance: 0,
            lifetime_points: 0,
            lifetime_visits: 0,
            locale: "en".into(),
            apple_serial: None,
            apple_auth_token: None,
            google_object_id: None,
            pass_updated_at: None,
            enrolled_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn the_update_address_survives_an_unset_loyalty_base() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: the lock makes this the only thread touching the environment.
        unsafe {
            std::env::remove_var("PUBLIC_LOYALTY_BASE_URL");
            std::env::set_var(
                "UPLOADS_BASE_URL",
                "https://api.madar-pos.cloud/api/uploads",
            );
        }
        // Falls back to the API origin, where the endpoints actually are.
        assert_eq!(
            web_service_url().as_deref(),
            Some("https://api.madar-pos.cloud/wallet")
        );

        unsafe {
            std::env::set_var("PUBLIC_LOYALTY_BASE_URL", "https://loyalty.madar-pos.cloud");
        }
        assert_eq!(
            web_service_url().as_deref(),
            Some("https://loyalty.madar-pos.cloud/api/wallet")
        );
        unsafe {
            std::env::remove_var("PUBLIC_LOYALTY_BASE_URL");
            std::env::remove_var("UPLOADS_BASE_URL");
        }
        // Nothing configured: the pass still issues, it simply never self-updates.
        assert!(web_service_url().is_none());
    }

    #[test]
    fn origin_parsing_keeps_the_scheme_and_drops_the_path() {
        assert_eq!(
            origin_of("https://api.madar-pos.cloud/api/uploads").as_deref(),
            Some("https://api.madar-pos.cloud")
        );
        assert_eq!(
            origin_of("http://localhost:8081/x").as_deref(),
            Some("http://localhost:8081")
        );
        assert_eq!(
            origin_of("https://api.example.com").as_deref(),
            Some("https://api.example.com")
        );
        // Not absolute, so there is no origin to take.
        assert_eq!(origin_of("/api/uploads"), None);
        assert_eq!(origin_of("https://"), None);
    }

    /// The Apple link is the API path, and a CERTIFICATE is all it takes.
    ///
    /// Two things this pins. It must not be a page path — that falls through to
    /// the SPA fallback and hands the customer an HTML document iOS refuses
    /// with no message, which looks exactly like a certificate problem. And it
    /// must not depend on `PUBLIC_LOYALTY_BASE_URL`: the page is same-origin,
    /// so requiring an absolute URL made a working set of certificates look
    /// broken for no reason.
    #[test]
    fn the_apple_link_points_at_the_api_and_needs_no_base_url() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: the lock above makes this the only thread touching the wallet
        // environment for the duration.
        unsafe {
            // Deliberately NOT set: a pass needs a certificate, not this.
            std::env::remove_var("PUBLIC_LOYALTY_BASE_URL");
            std::env::set_var("LOYALTY_APPLE_PASS_TYPE_ID", "pass.example");
            std::env::set_var("LOYALTY_APPLE_TEAM_ID", "TEAM123456");
            std::env::set_var("LOYALTY_APPLE_CERT_PEM", "x");
            std::env::set_var("LOYALTY_APPLE_KEY_PEM", "x");
            std::env::set_var("LOYALTY_APPLE_WWDR_PEM", "x");
        }
        let s = LoyaltySettings::defaults(Uuid::nil(), None);
        let links = links_for(
            &member(),
            &s,
            &crate::orgs::branding::OrgBrand::default(),
            &[],
        );
        assert_eq!(
            links.apple_url.as_deref(),
            Some("/api/public/loyalty/pass/Mabcdefghijklmnopqrstuv/apple.pkpass"),
            "certificates alone must be enough to offer the pass"
        );
        assert!(links.any);

        // With nothing configured at all, no buttons: the page shows the
        // member's QR instead, which still works at the till.
        unsafe {
            std::env::remove_var("LOYALTY_APPLE_PASS_TYPE_ID");
            std::env::remove_var("LOYALTY_APPLE_TEAM_ID");
            std::env::remove_var("LOYALTY_APPLE_CERT_PEM");
            std::env::remove_var("LOYALTY_APPLE_KEY_PEM");
            std::env::remove_var("LOYALTY_APPLE_WWDR_PEM");
        }
        let bare = links_for(
            &member(),
            &s,
            &crate::orgs::branding::OrgBrand::default(),
            &[],
        );
        assert!(bare.apple_url.is_none());
        assert!(
            !bare.any,
            "no wallet configured means the QR fallback, not a dead button"
        );
    }

    // Deliberately ONE test, not two: these read process-global environment,
    // and two tests mutating it race in the same process.
}
