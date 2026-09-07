//! Staff-facing surfaces: the teller's scan and redeem, and the admin's member
//! list, history and corrections.
//!
//! The teller never types a number of points. Earning is the server's business
//! (see [`super::model::award_for_order`], called from `create_order_inner`);
//! what a teller does here is identify the member in front of them and, when the
//! balance allows, hand over a reward.

use actix_web::{HttpRequest, HttpResponse, web};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::model::{self, LedgerEntry, MemberRow, MemberView};
use super::settings::{RewardItem, load_effective, load_effective_rewards};
use super::{resolve_branch_org, wallet};
use crate::delivery::{normalize_phone, require_branch_access};
use crate::errors::{AppError, AppErrorResponse};
use crate::models::UserRole;
use crate::orgs::handlers::extract_claims;
use crate::permissions::checker::check_permission;

/// What the teller's scan screen shows.
#[derive(Debug, Serialize, ToSchema)]
pub struct ScanResult {
    pub member: MemberView,
    /// What this member could claim at this branch right now. Empty until the
    /// balance reaches the threshold, so the screen cannot tempt a teller into
    /// handing over a reward that has not been earned.
    pub rewards: Vec<RewardItem>,
    /// Recent history, so a teller can answer "where did my points go?".
    pub recent: Vec<LedgerEntry>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct LookupRequest {
    pub branch_id: Uuid,
    /// The token from the scanned pass barcode. Preferred.
    pub token: Option<String>,
    /// Manual fallback for a customer whose phone is dead.
    pub phone: Option<String>,
}

/// Identify the member in front of the till.
///
/// A POST rather than a GET because the member token is a bearer-ish secret: in
/// a query string it would land in access logs, browser history and any proxy
/// in between.
#[utoipa::path(post, path = "/loyalty/lookup", tag = "loyalty", operation_id = "loyalty_lookup",
    request_body = LookupRequest, responses((status = 200, body = ScanResult), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn lookup(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    body: web::Json<LookupRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "loyalty", "read").await?;
    require_branch_access(pool.get_ref(), &claims, body.branch_id).await?;
    let org_id = resolve_branch_org(pool.get_ref(), body.branch_id).await?;

    let member = resolve_member(pool.get_ref(), org_id, &body).await?;
    let settings = load_effective(pool.get_ref(), org_id, body.branch_id).await?;
    let (all_rewards, _) = load_effective_rewards(pool.get_ref(), org_id, body.branch_id).await?;
    let recent = model::ledger(pool.get_ref(), member.id, 10).await?;
    let mode = settings.mode();
    let target = model::cheapest_cost(&all_rewards, mode).unwrap_or(settings.default_reward_cost);
    let view = member.clone().view(mode, target);
    // Only what this balance can actually buy, priced in the live currency. A
    // screen that lists a reward the customer cannot afford is a screen that
    // tempts a teller into handing it over anyway.
    let balance = member.balance_in(mode);
    let rewards: Vec<_> = all_rewards
        .into_iter()
        .filter(|r| r.cost_currency == mode.as_str() && r.cost_amount <= balance)
        .collect();
    Ok(HttpResponse::Ok().json(ScanResult {
        member: view,
        rewards,
        recent,
    }))
}

/// The member a lookup names, checked against the branch's org.
///
/// A token is globally unique and carries no org of its own, so a member from
/// another tenant would otherwise resolve here. Reporting that as "not found"
/// rather than "wrong org" keeps one tenant from probing another's membership.
async fn resolve_member(
    pool: &PgPool,
    org_id: Uuid,
    body: &LookupRequest,
) -> Result<MemberRow, AppError> {
    let found = match (&body.token, &body.phone) {
        (Some(token), _) if !token.trim().is_empty() => {
            model::find_by_token(pool, token.trim()).await?
        }
        (_, Some(phone)) if !phone.trim().is_empty() => {
            model::find_by_phone(pool, org_id, &normalize_phone(phone)?).await?
        }
        _ => {
            return Err(AppError::BadRequest(
                "Supply the scanned token or a phone number".into(),
            ));
        }
    };
    match found {
        Some(m) if m.org_id == org_id => Ok(m),
        _ => Err(AppError::NotFound("No member for that card".into())),
    }
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct AdjustRequest {
    pub branch_id: Uuid,
    pub customer_id: Uuid,
    /// Signed. Negative takes points away.
    pub points: i32,
    pub note: Option<String>,
}

#[utoipa::path(post, path = "/loyalty/adjust", tag = "loyalty", operation_id = "loyalty_adjust",
    request_body = AdjustRequest, responses((status = 200, body = MemberView), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn adjust(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    body: web::Json<AdjustRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    // Handing out points by hand is the one action here that creates value from
    // nothing, so it sits above the till: the permission table is consulted as
    // usual AND the actor must be an admin. `permission_action` has no rung
    // above `update`, so the role check is what separates this from a redeem.
    check_permission(pool.get_ref(), &claims, "loyalty", "update").await?;
    if !matches!(claims.role, UserRole::OrgAdmin | UserRole::SuperAdmin) {
        return Err(AppError::Forbidden(
            "Only an admin may adjust a member's points by hand".into(),
        ));
    }
    require_branch_access(pool.get_ref(), &claims, body.branch_id).await?;
    let org_id = resolve_branch_org(pool.get_ref(), body.branch_id).await?;

    let member = model::find_by_id(pool.get_ref(), body.customer_id)
        .await?
        .filter(|m| m.org_id == org_id)
        .ok_or_else(|| AppError::NotFound("No member for that card".into()))?;
    let settings = load_effective(pool.get_ref(), org_id, body.branch_id).await?;
    let (rewards, _) = load_effective_rewards(pool.get_ref(), org_id, body.branch_id).await?;
    let mode = settings.mode();
    let target = model::cheapest_cost(&rewards, mode).unwrap_or(settings.default_reward_cost);

    let view = model::adjust(
        pool.get_ref(),
        &member,
        body.branch_id,
        mode,
        body.points,
        body.note.clone(),
        claims.user_id_safe().ok(),
    )
    .await?;
    wallet::push_update(pool.get_ref(), member.id);
    Ok(HttpResponse::Ok().json(MemberView {
        next_reward_cost: target,
        points_to_next_reward: (target - view.balance).max(0),
        can_redeem: view.balance >= target,
        ..view
    }))
}

// ── Admin: the member list and one member's history ──────────────────────────

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct MembersQuery {
    /// Scopes the thresholds shown. Omit to use the org default.
    pub branch_id: Option<Uuid>,
    /// Name or phone fragment.
    pub q: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct MembersPage {
    pub members: Vec<MemberView>,
    pub total: i64,
}

#[utoipa::path(get, path = "/loyalty/members", tag = "loyalty", operation_id = "list_loyalty_members",
    params(MembersQuery), responses((status = 200, body = MembersPage), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn list_members(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    query: web::Query<MembersQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "loyalty", "read").await?;
    let org_id = match (claims.org_id(), query.branch_id) {
        (Some(o), _) => o,
        (None, Some(b)) => resolve_branch_org(pool.get_ref(), b).await?,
        (None, None) => {
            return Err(AppError::BadRequest(
                "branch_id is required for a super admin".into(),
            ));
        }
    };

    let scope = match query.branch_id {
        Some(b) => {
            require_branch_access(pool.get_ref(), &claims, b).await?;
            load_effective(pool.get_ref(), org_id, b).await?
        }
        None => super::settings::load_scope(pool.get_ref(), org_id, None)
            .await?
            .unwrap_or_else(|| super::settings::LoyaltySettings::defaults(org_id, None)),
    };
    let rewards = match query.branch_id {
        Some(b) => load_effective_rewards(pool.get_ref(), org_id, b).await?.0,
        None => super::settings::load_effective_rewards_org(pool.get_ref(), org_id).await?,
    };
    let mode = scope.mode();
    let target = model::cheapest_cost(&rewards, mode).unwrap_or(scope.default_reward_cost);

    let limit = query.limit.unwrap_or(50).clamp(1, 200);
    let offset = query.offset.unwrap_or(0).max(0);
    // `%` and `_` in a search box are literal characters to the person typing
    // them, not wildcards that quietly match everything.
    let needle = query
        .q
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            format!(
                "%{}%",
                s.replace('\\', "\\\\")
                    .replace('%', "\\%")
                    .replace('_', "\\_")
            )
        });

    let rows: Vec<MemberRow> = sqlx::query_as(&format!(
        "SELECT {} FROM loyalty_customers \
          WHERE org_id = $1 AND deleted_at IS NULL \
            AND ($2::text IS NULL OR name ILIKE $2 OR phone ILIKE $2) \
          ORDER BY enrolled_at DESC LIMIT $3 OFFSET $4",
        model::MEMBER_COLS
    ))
    .bind(org_id)
    .bind(&needle)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool.get_ref())
    .await?;

    let total: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM loyalty_customers \
          WHERE org_id = $1 AND deleted_at IS NULL \
            AND ($2::text IS NULL OR name ILIKE $2 OR phone ILIKE $2)",
    )
    .bind(org_id)
    .bind(&needle)
    .fetch_one(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(MembersPage {
        members: rows.into_iter().map(|r| r.view(mode, target)).collect(),
        total,
    }))
}

#[derive(Debug, Serialize, ToSchema)]
pub struct MemberDetail {
    pub member: MemberView,
    pub ledger: Vec<LedgerEntry>,
}

#[utoipa::path(get, path = "/loyalty/members/{id}", tag = "loyalty", operation_id = "get_loyalty_member",
    params(("id" = Uuid, Path, description = "Member ID"), MembersQuery),
    responses((status = 200, body = MemberDetail), AppErrorResponse), security(("bearer_jwt" = [])))]
pub async fn get_member(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    id: web::Path<Uuid>,
    query: web::Query<MembersQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "loyalty", "read").await?;
    let member = model::find_by_id(pool.get_ref(), *id)
        .await?
        .ok_or_else(|| AppError::NotFound("Member not found".into()))?;
    if let Some(org) = claims.org_id()
        && member.org_id != org
    {
        return Err(AppError::NotFound("Member not found".into()));
    }
    let scope = match query.branch_id {
        Some(b) => load_effective(pool.get_ref(), member.org_id, b).await?,
        None => super::settings::load_scope(pool.get_ref(), member.org_id, None)
            .await?
            .unwrap_or_else(|| super::settings::LoyaltySettings::defaults(member.org_id, None)),
    };
    let rewards =
        super::settings::load_effective_rewards_org(pool.get_ref(), member.org_id).await?;
    let mode = scope.mode();
    let target = model::cheapest_cost(&rewards, mode).unwrap_or(scope.default_reward_cost);
    let ledger = model::ledger(pool.get_ref(), member.id, 200).await?;
    Ok(HttpResponse::Ok().json(MemberDetail {
        member: member.view(mode, target),
        ledger,
    }))
}
