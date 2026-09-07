//! The public signup site (`loyalty.madar-pos.cloud`).
//!
//! A customer scans the counter's join QR, lands on a plain webform — no app, no
//! install — gives a name and phone, and walks away with a Wallet pass. These
//! endpoints are unauthenticated and rate-limited exactly like the ordering and
//! booking public endpoints they sit beside.
//!
//! OTP is not reimplemented here. The existing `/public/otp/request` and
//! `/public/otp/verify` already prove a phone and mint the 90-day device-trust
//! token; this module simply requires that token when the branch asks for it,
//! the same way delivery intake does.

use actix_web::{HttpResponse, web};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::model::{self, MemberRow};
use super::settings::{load_effective, load_effective_rewards};
use super::wallet::{self, PassLinks};
use super::{mint_member_token, resolve_branch_org};
use crate::auth::jwt::JwtSecret;
use crate::delivery::{normalize_phone, whatsapp};
use crate::errors::{AppError, AppErrorResponse};

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct BranchQuery {
    pub branch_id: Uuid,
}

/// What the signup page needs to render itself before anyone types anything.
#[derive(Serialize, ToSchema)]
pub struct JoinInfo {
    pub branch_id: Uuid,
    pub branch_name: String,
    pub org_name: String,
    pub program_name: String,
    pub program_name_ar: Option<String>,
    /// False when the program is off here — the page says so instead of taking
    /// a signup that would go nowhere.
    pub enabled: bool,
    /// The page collects an OTP only when the branch asks for one.
    pub require_otp: bool,
    /// `"points"` (earned on spend) or `"visits"` (a stamp per order) — which
    /// sentence the page writes.
    pub mode: String,
    /// The cheapest reward on offer, in `mode`'s currency.
    pub next_reward_cost: i32,
    /// EGP that earns one point — the page's "a point for every N EGP" line.
    /// Piastres on the wire, as everywhere; the page divides by 100. Only
    /// meaningful when `mode` is `"points"`.
    pub earn_piastres_per_point: i32,
    /// The rewards on offer, each with what it costs.
    pub rewards: Vec<PublicReward>,
    pub terms: Option<String>,
    pub terms_ar: Option<String>,
}

/// A reward as the signup page lists it: what it is, and what it costs.
#[derive(Serialize, ToSchema)]
pub struct PublicReward {
    pub name: String,
    pub cost_currency: String,
    pub cost_amount: i32,
}

#[utoipa::path(get, path = "/public/loyalty/join-info", tag = "loyalty-public", operation_id = "loyalty_join_info", params(BranchQuery),
    responses((status = 200, body = JoinInfo), AppErrorResponse))]
pub async fn join_info(
    pool: web::Data<PgPool>,
    query: web::Query<BranchQuery>,
) -> Result<HttpResponse, AppError> {
    let org_id = resolve_branch_org(pool.get_ref(), query.branch_id).await?;
    let names: Option<(String, String)> = sqlx::query_as(
        "SELECT b.name, o.name FROM branches b JOIN organizations o ON o.id = b.org_id \
         WHERE b.id = $1 AND b.is_active AND b.deleted_at IS NULL",
    )
    .bind(query.branch_id)
    .fetch_optional(pool.get_ref())
    .await?;
    let (branch_name, org_name) =
        names.ok_or_else(|| AppError::NotFound("Branch not found".into()))?;

    let settings = load_effective(pool.get_ref(), org_id, query.branch_id).await?;
    let (rewards, _) = load_effective_rewards(pool.get_ref(), org_id, query.branch_id).await?;

    Ok(HttpResponse::Ok().json(JoinInfo {
        branch_id: query.branch_id,
        branch_name,
        org_name,
        program_name: settings.program_name.clone(),
        program_name_ar: settings.program_name_ar.clone(),
        enabled: settings.enabled,
        require_otp: settings.require_otp,
        mode: settings.mode.clone(),
        next_reward_cost: model::cheapest_cost(&rewards, settings.mode())
            .unwrap_or(settings.default_reward_cost),
        earn_piastres_per_point: settings.earn_piastres_per_point,
        rewards: rewards
            .into_iter()
            .map(|r| PublicReward {
                name: r.name,
                cost_currency: r.cost_currency,
                cost_amount: r.cost_amount,
            })
            .collect(),
        terms: settings.terms.clone(),
        terms_ar: settings.terms_ar.clone(),
    }))
}

#[derive(Deserialize, ToSchema)]
pub struct JoinInput {
    pub branch_id: Uuid,
    pub name: String,
    pub phone: String,
    /// Device-trust token from `/public/otp/verify`. Required only when the
    /// branch's `require_otp` is on.
    #[serde(default)]
    pub device_token: Option<String>,
    /// 'en' or 'ar' — the language the pass is written in.
    #[serde(default)]
    pub locale: Option<String>,
}

/// What the customer sees after signing up: their card, and the buttons.
#[derive(Serialize, ToSchema)]
pub struct JoinResult {
    pub member_token: String,
    pub name: String,
    /// The live balance, in `mode`'s currency. Zero for a fresh member.
    pub balance: i32,
    pub mode: String,
    pub next_reward_cost: i32,
    pub program_name: String,
    pub passes: PassLinks,
    /// True when this phone was already a member — the page says "welcome back"
    /// and shows the existing card rather than pretending to have made a new one.
    pub already_member: bool,
}

#[utoipa::path(post, path = "/public/loyalty/join", tag = "loyalty-public", operation_id = "loyalty_join", request_body = JoinInput,
    responses((status = 200, body = JoinResult), AppErrorResponse))]
pub async fn join(
    pool: web::Data<PgPool>,
    secret: web::Data<JwtSecret>,
    body: web::Json<JoinInput>,
) -> Result<HttpResponse, AppError> {
    let org_id = resolve_branch_org(pool.get_ref(), body.branch_id).await?;
    let settings = load_effective(pool.get_ref(), org_id, body.branch_id).await?;
    if !settings.enabled {
        return Err(AppError::Conflict(
            "This branch is not running a loyalty program".into(),
        ));
    }

    let name = body.name.trim();
    if name.is_empty() {
        return Err(AppError::BadRequest("Please enter your name".into()));
    }
    if name.chars().count() > 80 {
        return Err(AppError::BadRequest("That name is too long".into()));
    }
    let phone = normalize_phone(&body.phone)?;

    // The same proof of phone the delivery intake requires, and only when the
    // branch asks for it — an admin turns OTP off per tenant exactly as they do
    // for ordering and bookings.
    if settings.require_otp {
        let token = body.device_token.as_deref().unwrap_or_default();
        if !whatsapp::verify_device_token(&secret.0, &phone, token) {
            return Err(AppError::Unauthorized(
                "Verify your phone number first".into(),
            ));
        }
    }

    let locale = match body.locale.as_deref() {
        Some("ar") => "ar",
        _ => "en",
    };

    // Joining twice from the same phone is a normal thing to do — a customer who
    // lost their pass rescans the counter QR. Return the existing card rather
    // than a duplicate member or an error.
    let (member, already_member) = match model::find_by_phone(pool.get_ref(), org_id, &phone)
        .await?
    {
        Some(existing) => (existing, true),
        None => {
            let row: MemberRow = sqlx::query_as(&format!(
                    "INSERT INTO loyalty_customers \
                        (org_id, phone, name, member_token, joined_branch_id, locale, apple_auth_token) \
                     VALUES ($1,$2,$3,$4,$5,$6,$7) RETURNING {}",
                    model::MEMBER_COLS
                ))
                .bind(org_id)
                .bind(&phone)
                .bind(name)
                .bind(mint_member_token())
                .bind(body.branch_id)
                .bind(locale)
                // Apple authenticates pass updates with this; minted now so a
                // pass issued later needs no second write.
                .bind(mint_member_token())
                .fetch_one(pool.get_ref())
                .await?;
            (row, false)
        }
    };

    let (rewards, _) = load_effective_rewards(pool.get_ref(), org_id, body.branch_id).await?;
    let mode = settings.mode();
    let passes = wallet::links_for(&member, &settings);
    Ok(HttpResponse::Ok().json(JoinResult {
        member_token: member.member_token.clone(),
        name: member.name.clone(),
        balance: member.balance_in(mode),
        mode: settings.mode.clone(),
        next_reward_cost: model::cheapest_cost(&rewards, mode)
            .unwrap_or(settings.default_reward_cost),
        program_name: settings.program_name.clone(),
        passes,
        already_member,
    }))
}

/// The member's own card page — what they see when they open the link again.
///
/// The token in the path is the member's secret, which is why this returns only
/// what the pass already shows and never the phone number in full.
#[derive(Serialize, ToSchema)]
pub struct CardView {
    pub name: String,
    /// The live balance, in `mode`'s currency.
    pub balance: i32,
    pub mode: String,
    pub next_reward_cost: i32,
    pub points_to_next_reward: i32,
    pub can_redeem: bool,
    pub program_name: String,
    pub member_token: String,
    pub rewards: Vec<PublicReward>,
    pub passes: PassLinks,
}

#[utoipa::path(get, path = "/public/loyalty/card/{token}", tag = "loyalty-public", operation_id = "loyalty_card",
    params(("token" = String, Path, description = "Member token from the pass barcode")),
    responses((status = 200, body = CardView), AppErrorResponse))]
pub async fn card(
    pool: web::Data<PgPool>,
    token: web::Path<String>,
) -> Result<HttpResponse, AppError> {
    let member = model::find_by_token(pool.get_ref(), token.as_str())
        .await?
        .ok_or_else(|| AppError::NotFound("Card not found".into()))?;
    // The org default is the right scope here: a customer opening their card at
    // home is not standing in any particular branch.
    let settings = super::settings::load_scope(pool.get_ref(), member.org_id, None)
        .await?
        .unwrap_or_else(|| super::settings::LoyaltySettings::defaults(member.org_id, None));
    let catalogue =
        super::settings::load_effective_rewards_org(pool.get_ref(), member.org_id).await?;
    let mode = settings.mode();
    let target = model::cheapest_cost(&catalogue, mode).unwrap_or(settings.default_reward_cost);
    let passes = wallet::links_for(&member, &settings);
    let view = member.view(mode, target);
    Ok(HttpResponse::Ok().json(CardView {
        name: view.name,
        balance: view.balance,
        mode: view.mode,
        next_reward_cost: view.next_reward_cost,
        points_to_next_reward: view.points_to_next_reward,
        can_redeem: view.can_redeem,
        program_name: settings.program_name,
        member_token: token.into_inner(),
        rewards: catalogue
            .into_iter()
            .map(|r| PublicReward {
                name: r.name,
                cost_currency: r.cost_currency,
                cost_amount: r.cost_amount,
            })
            .collect(),
        passes,
    }))
}

/// Download the signed `.pkpass`.
///
/// 503s until Apple credentials are configured — see
/// `wallet::apple::sign_manifest`. The button that leads here is only rendered
/// when `apple::is_configured()`, so a customer does not meet this by accident.
#[utoipa::path(get, path = "/public/loyalty/pass/{token}/apple.pkpass", tag = "loyalty-public", operation_id = "loyalty_apple_pass",
    params(("token" = String, Path, description = "Member token")),
    responses((status = 200, description = "Apple Wallet pass"), AppErrorResponse))]
pub async fn apple_pass(
    pool: web::Data<PgPool>,
    token: web::Path<String>,
) -> Result<HttpResponse, AppError> {
    let member = model::find_by_token(pool.get_ref(), token.as_str())
        .await?
        .ok_or_else(|| AppError::NotFound("Card not found".into()))?;
    // The same builder the device's own refetch uses, so the pass a customer
    // downloads and the pass their phone later pulls are the same shape.
    let bytes = wallet::apple::build_pass_for(pool.get_ref(), &member).await?;

    // Record the serial so pass updates can find this member later.
    sqlx::query(
        "UPDATE loyalty_customers SET apple_serial = $2, pass_updated_at = now() \
         WHERE id = $1 AND apple_serial IS NULL",
    )
    .bind(member.id)
    .bind(member.id.to_string())
    .execute(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok()
        .content_type("application/vnd.apple.pkpass")
        .append_header((
            "Content-Disposition",
            "attachment; filename=\"madar.pkpass\"",
        ))
        .body(bytes))
}

/// The member's QR as a PNG.
///
/// Rendered server-side with the same renderer the printed cards use, rather
/// than shipping a QR library to the browser — and rendered from the token
/// DIRECTLY, never through a Shlink short link: a short URL is a public
/// redirect, and the member token is the one value here that has to stay
/// between the customer and the till.
///
/// This is the fallback that makes the program usable before either wallet is
/// configured — and the answer for a customer whose phone has no wallet app.
#[utoipa::path(get, path = "/public/loyalty/card/{token}/qr.png", tag = "loyalty-public",
    operation_id = "loyalty_card_qr",
    params(("token" = String, Path, description = "Member token")),
    responses((status = 200, description = "Member QR as a PNG"), AppErrorResponse))]
pub async fn card_qr(
    pool: web::Data<PgPool>,
    token: web::Path<String>,
) -> Result<HttpResponse, AppError> {
    // Resolve first: rendering a QR of an arbitrary string a caller supplied
    // would turn this into an open QR generator for anyone who found the URL.
    let member = model::find_by_token(pool.get_ref(), token.as_str())
        .await?
        .ok_or_else(|| AppError::NotFound("Card not found".into()))?;
    let png = crate::qr_card::render_qr_receipt_png(&member.member_token, 8)
        .map_err(|_| AppError::Internal)?;
    Ok(HttpResponse::Ok()
        .content_type("image/png")
        // The token never changes, but the pass and page around it might, and a
        // stale cached QR is indistinguishable from a broken card.
        .append_header(("Cache-Control", "private, max-age=3600"))
        .body(png))
}
