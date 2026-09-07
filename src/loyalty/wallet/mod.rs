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

/// What signup hands the customer. Either side may be absent: a tenant with only
/// Google credentials configured shows one button, not a broken one.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PassLinks {
    /// Downloads the signed `.pkpass`.
    pub apple_url: Option<String>,
    /// `https://pay.google.com/gp/v/save/<jwt>`.
    pub google_url: Option<String>,
    /// False when neither wallet is configured — the site shows the member's
    /// QR on the page instead of dead buttons.
    pub any: bool,
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

/// Build both "add to wallet" links for a member.
pub fn links_for(member: &MemberRow, settings: &LoyaltySettings) -> PassLinks {
    let apple_url = apple::is_configured()
        .then(|| loyalty_base().map(|b| format!("{b}/pass/{}/apple.pkpass", member.member_token)))
        .flatten();
    let google_url = google::save_url(member, settings).unwrap_or_else(|e| {
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
