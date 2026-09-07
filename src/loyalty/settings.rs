//! Program settings and the reward catalogue.
//!
//! Both are scoped the same way, following `attendance_settings`: a row with
//! `branch_id IS NULL` is the org-wide default, and a branch row **replaces it
//! wholesale** for that branch. Wholesale rather than field-by-field because a
//! half-inherited earn rule is impossible to reason about at a counter — an
//! admin looking at a branch sees exactly the numbers that branch uses.
//!
//! Deleting a branch row puts the branch back on the org default.

use actix_web::{HttpRequest, HttpResponse, web};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::earn::{EarnRule, Mode};
use super::resolve_branch_org;
use crate::delivery::require_branch_access;
use crate::errors::{AppError, AppErrorResponse};
use crate::orgs::handlers::extract_claims;
use crate::permissions::checker::check_permission;

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct LoyaltySettings {
    pub org_id: Uuid,
    /// `null` = the org-wide default. A branch id = that branch's override.
    pub branch_id: Option<Uuid>,
    /// The program switch for this scope.
    pub enabled: bool,
    pub program_name: String,
    pub program_name_ar: Option<String>,

    /// What this scope collects: `"points"` (from money spent) or `"visits"`
    /// (one stamp per sale). One or the other — never both.
    pub mode: String,

    /// One point per this many piastres. 1000 = a point per 10 EGP. The
    /// dashboard shows and accepts EGP; the wire is always piastres.
    pub earn_piastres_per_point: i32,
    /// Earn on what was actually paid rather than the pre-discount subtotal.
    pub earn_on_discounted: bool,
    /// Add tax to the basis. Tips never earn and have no toggle.
    pub earn_include_tax: bool,

    /// The cost offered by default when an admin adds a reward, in whatever this
    /// scope collects. Each reward may override it, so one catalogue holds
    /// "espresso, 5 visits" beside "cake, 10 visits". Also the pass's fallback
    /// target when no rewards have been curated yet.
    pub default_reward_cost: i32,
    /// Verify the signup phone by WhatsApp code, like bookings and ordering.
    pub require_otp: bool,

    pub pass_background_color: Option<String>,
    pub pass_foreground_color: Option<String>,
    pub pass_label_color: Option<String>,
    pub pass_logo_url: Option<String>,
    pub terms: Option<String>,
    pub terms_ar: Option<String>,
}

impl LoyaltySettings {
    pub fn defaults(org_id: Uuid, branch_id: Option<Uuid>) -> Self {
        Self {
            org_id,
            branch_id,
            enabled: false,
            program_name: "Rewards".into(),
            program_name_ar: None,
            mode: Mode::Points.as_str().into(),
            earn_piastres_per_point: 1000,
            earn_on_discounted: true,
            earn_include_tax: false,
            default_reward_cost: 100,
            require_otp: true,
            pass_background_color: None,
            pass_foreground_color: None,
            pass_label_color: None,
            pass_logo_url: None,
            terms: None,
            terms_ar: None,
        }
    }

    /// What this scope collects.
    pub fn mode(&self) -> Mode {
        Mode::parse(&self.mode)
    }

    /// The earn rule these settings describe.
    pub fn rule(&self) -> EarnRule {
        EarnRule {
            mode: self.mode(),
            piastres_per_point: self.earn_piastres_per_point,
            on_discounted: self.earn_on_discounted,
            include_tax: self.earn_include_tax,
        }
    }

    fn validate(&self) -> Result<(), AppError> {
        if self.earn_piastres_per_point <= 0 {
            return Err(AppError::BadRequest(
                "earn_piastres_per_point must be greater than zero".into(),
            ));
        }
        if self.default_reward_cost <= 0 {
            return Err(AppError::BadRequest(
                "default_reward_cost must be greater than zero".into(),
            ));
        }
        if !matches!(self.mode.as_str(), "points" | "visits") {
            return Err(AppError::BadRequest(
                "mode must be 'points' or 'visits'".into(),
            ));
        }
        if self.program_name.trim().is_empty() {
            return Err(AppError::BadRequest("program_name is required".into()));
        }
        for (label, c) in [
            ("pass_background_color", &self.pass_background_color),
            ("pass_foreground_color", &self.pass_foreground_color),
            ("pass_label_color", &self.pass_label_color),
        ] {
            if let Some(c) = c
                && !is_hex_color(c)
            {
                return Err(AppError::BadRequest(format!("{label} must be #RRGGBB")));
            }
        }
        Ok(())
    }
}

fn is_hex_color(s: &str) -> bool {
    s.len() == 7 && s.starts_with('#') && s[1..].chars().all(|c| c.is_ascii_hexdigit())
}

#[derive(sqlx::FromRow)]
struct Row {
    org_id: Uuid,
    branch_id: Option<Uuid>,
    enabled: bool,
    program_name: String,
    program_name_ar: Option<String>,
    mode: String,
    earn_piastres_per_point: i32,
    earn_on_discounted: bool,
    earn_include_tax: bool,
    default_reward_cost: i32,
    require_otp: bool,
    pass_background_color: Option<String>,
    pass_foreground_color: Option<String>,
    pass_label_color: Option<String>,
    pass_logo_url: Option<String>,
    terms: Option<String>,
    terms_ar: Option<String>,
}

const COLS: &str = "org_id, branch_id, enabled, program_name, program_name_ar, mode, \
    earn_piastres_per_point, earn_on_discounted, earn_include_tax, \
    default_reward_cost, require_otp, pass_background_color, \
    pass_foreground_color, pass_label_color, pass_logo_url, terms, terms_ar";

impl From<Row> for LoyaltySettings {
    fn from(r: Row) -> Self {
        Self {
            org_id: r.org_id,
            branch_id: r.branch_id,
            enabled: r.enabled,
            program_name: r.program_name,
            program_name_ar: r.program_name_ar,
            mode: r.mode,
            earn_piastres_per_point: r.earn_piastres_per_point,
            earn_on_discounted: r.earn_on_discounted,
            earn_include_tax: r.earn_include_tax,
            default_reward_cost: r.default_reward_cost,
            require_otp: r.require_otp,
            pass_background_color: r.pass_background_color,
            pass_foreground_color: r.pass_foreground_color,
            pass_label_color: r.pass_label_color,
            pass_logo_url: r.pass_logo_url,
            terms: r.terms,
            terms_ar: r.terms_ar,
        }
    }
}

/// The row saved for exactly this scope, if any. Does **not** fall back.
pub async fn load_scope<'e, E>(
    exec: E,
    org_id: Uuid,
    branch_id: Option<Uuid>,
) -> Result<Option<LoyaltySettings>, AppError>
where
    E: sqlx::PgExecutor<'e>,
{
    let row: Option<Row> = sqlx::query_as(&format!(
        "SELECT {COLS} FROM loyalty_settings WHERE org_id = $1 \
         AND COALESCE(branch_id, '00000000-0000-0000-0000-000000000000'::uuid) \
           = COALESCE($2::uuid, '00000000-0000-0000-0000-000000000000'::uuid)"
    ))
    .bind(org_id)
    .bind(branch_id)
    .fetch_optional(exec)
    .await?;
    Ok(row.map(LoyaltySettings::from))
}

/// What a branch actually runs on: its own override, else the org default, else
/// the built-in defaults (program off). This is the only reader the earn and
/// redeem paths use.
pub async fn load_effective(
    pool: &PgPool,
    org_id: Uuid,
    branch_id: Uuid,
) -> Result<LoyaltySettings, AppError> {
    if let Some(s) = load_scope(pool, org_id, Some(branch_id)).await? {
        return Ok(s);
    }
    if let Some(s) = load_scope(pool, org_id, None).await? {
        // Report it under the branch that asked, so a caller never has to know
        // which scope answered.
        return Ok(LoyaltySettings {
            branch_id: Some(branch_id),
            ..s
        });
    }
    Ok(LoyaltySettings::defaults(org_id, Some(branch_id)))
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ScopeQuery {
    /// Omit for the org-wide default; supply a branch for its override.
    pub branch_id: Option<Uuid>,
}

/// The org of the caller, or of the branch they named. Super admins act on the
/// branch's org; everyone else is pinned to their own.
async fn scope_org(
    pool: &PgPool,
    req: &HttpRequest,
    branch_id: Option<Uuid>,
) -> Result<(Uuid, crate::auth::jwt::Claims), AppError> {
    let claims = extract_claims(req)?;
    let org_id = match (claims.org_id(), branch_id) {
        (Some(o), _) => o,
        (None, Some(b)) => resolve_branch_org(pool, b).await?,
        (None, None) => {
            return Err(AppError::BadRequest(
                "branch_id is required for a super admin".into(),
            ));
        }
    };
    Ok((org_id, claims))
}

#[utoipa::path(get, path = "/loyalty/settings", tag = "loyalty", operation_id = "get_loyalty_settings",
    params(ScopeQuery), responses((status = 200, body = LoyaltySettings), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn get_settings(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    query: web::Query<ScopeQuery>,
) -> Result<HttpResponse, AppError> {
    let (org_id, claims) = scope_org(pool.get_ref(), &req, query.branch_id).await?;
    check_permission(pool.get_ref(), &claims, "loyalty", "read").await?;
    if let Some(b) = query.branch_id {
        require_branch_access(pool.get_ref(), &claims, b).await?;
    }
    // A branch scope reports what that branch RUNS ON (inherited or its own), so
    // the dashboard shows the numbers in force rather than an empty form.
    let settings = match query.branch_id {
        Some(b) => load_effective(pool.get_ref(), org_id, b).await?,
        None => load_scope(pool.get_ref(), org_id, None)
            .await?
            .unwrap_or_else(|| LoyaltySettings::defaults(org_id, None)),
    };
    Ok(HttpResponse::Ok().json(settings))
}

#[utoipa::path(put, path = "/loyalty/settings", tag = "loyalty", operation_id = "put_loyalty_settings",
    request_body = LoyaltySettings, responses((status = 200, body = LoyaltySettings), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn put_settings(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    body: web::Json<LoyaltySettings>,
) -> Result<HttpResponse, AppError> {
    let incoming = body.into_inner();
    let (org_id, claims) = scope_org(pool.get_ref(), &req, incoming.branch_id).await?;
    check_permission(pool.get_ref(), &claims, "loyalty", "update").await?;
    if let Some(b) = incoming.branch_id {
        require_branch_access(pool.get_ref(), &claims, b).await?;
        // The branch must belong to the org the row is filed under, or the
        // override would be invisible to the org that owns the branch.
        if resolve_branch_org(pool.get_ref(), b).await? != org_id {
            return Err(AppError::Forbidden(
                "Branch belongs to a different org".into(),
            ));
        }
    }
    incoming.validate()?;

    let row: Row = sqlx::query_as(&format!(
        "INSERT INTO loyalty_settings (org_id, branch_id, enabled, program_name, program_name_ar, \
            mode, earn_piastres_per_point, earn_on_discounted, earn_include_tax, default_reward_cost, \
            require_otp, pass_background_color, pass_foreground_color, pass_label_color, \
            pass_logo_url, terms, terms_ar) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17) \
         ON CONFLICT (org_id, COALESCE(branch_id, '00000000-0000-0000-0000-000000000000'::uuid)) \
         DO UPDATE SET enabled = EXCLUDED.enabled, program_name = EXCLUDED.program_name, \
            program_name_ar = EXCLUDED.program_name_ar, mode = EXCLUDED.mode, \
            earn_piastres_per_point = EXCLUDED.earn_piastres_per_point, \
            earn_on_discounted = EXCLUDED.earn_on_discounted, \
            earn_include_tax = EXCLUDED.earn_include_tax, \
            default_reward_cost = EXCLUDED.default_reward_cost, \
            require_otp = EXCLUDED.require_otp, \
            pass_background_color = EXCLUDED.pass_background_color, \
            pass_foreground_color = EXCLUDED.pass_foreground_color, \
            pass_label_color = EXCLUDED.pass_label_color, \
            pass_logo_url = EXCLUDED.pass_logo_url, terms = EXCLUDED.terms, \
            terms_ar = EXCLUDED.terms_ar, updated_at = now() \
         RETURNING {COLS}"
    ))
    .bind(org_id)
    .bind(incoming.branch_id)
    .bind(incoming.enabled)
    .bind(incoming.program_name.trim())
    .bind(&incoming.program_name_ar)
    .bind(incoming.mode.as_str())
    .bind(incoming.earn_piastres_per_point)
    .bind(incoming.earn_on_discounted)
    .bind(incoming.earn_include_tax)
    .bind(incoming.default_reward_cost)
    .bind(incoming.require_otp)
    .bind(&incoming.pass_background_color)
    .bind(&incoming.pass_foreground_color)
    .bind(&incoming.pass_label_color)
    .bind(&incoming.pass_logo_url)
    .bind(&incoming.terms)
    .bind(&incoming.terms_ar)
    .fetch_one(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(LoyaltySettings::from(row)))
}

#[utoipa::path(delete, path = "/loyalty/settings", tag = "loyalty", operation_id = "delete_loyalty_settings",
    params(ScopeQuery), responses((status = 204), AppErrorResponse), security(("bearer_jwt" = [])))]
pub async fn delete_settings(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    query: web::Query<ScopeQuery>,
) -> Result<HttpResponse, AppError> {
    let branch_id = query.branch_id.ok_or_else(|| {
        // Deleting the org default would leave every branch on the built-in
        // defaults with the program silently off — almost never what was meant.
        AppError::BadRequest("branch_id is required: only a branch override may be removed".into())
    })?;
    let (org_id, claims) = scope_org(pool.get_ref(), &req, Some(branch_id)).await?;
    check_permission(pool.get_ref(), &claims, "loyalty", "update").await?;
    require_branch_access(pool.get_ref(), &claims, branch_id).await?;
    sqlx::query("DELETE FROM loyalty_settings WHERE org_id = $1 AND branch_id = $2")
        .bind(org_id)
        .bind(branch_id)
        .execute(pool.get_ref())
        .await?;
    Ok(HttpResponse::NoContent().finish())
}

// ── The reward catalogue ─────────────────────────────────────────────────────
// Which menu items a member may claim at the threshold. Scoped like the
// settings above: a branch with no rows of its own inherits the org's list, so
// an org curates one catalogue and a branch departs from it only when it means
// to. Item selection governs REDEMPTION only — earning reads order totals, so
// the till never needs to know which lines were eligible.

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RewardItem {
    pub menu_item_id: Uuid,
    /// Denormalised for display so the teller and the pass need no menu join.
    pub name: String,
    pub image_url: Option<String>,
    /// Menu price in piastres — what the reward is worth, for the admin's sake.
    pub base_price: i32,
    /// `"points"` or `"visits"` — what this reward is bought with.
    pub cost_currency: String,
    /// How much of that currency it costs. Per item, so one catalogue holds
    /// "espresso, 5 visits" beside "cake, 10 visits".
    pub cost_amount: i32,
    pub sort_order: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RewardCatalogue {
    pub org_id: Uuid,
    pub branch_id: Option<Uuid>,
    /// True when these rows are the org default rather than this branch's own.
    pub inherited: bool,
    pub items: Vec<RewardItem>,
}

async fn load_reward_rows<'e, E>(
    exec: E,
    org_id: Uuid,
    branch_id: Option<Uuid>,
) -> Result<Vec<RewardItem>, AppError>
where
    E: sqlx::PgExecutor<'e>,
{
    let rows: Vec<(Uuid, String, Option<String>, i32, String, i32, i32)> = sqlx::query_as(
        "SELECT m.id, m.name, m.image_url, m.base_price, r.cost_currency, r.cost_amount, \
                r.sort_order \
           FROM loyalty_reward_items r JOIN menu_items m ON m.id = r.menu_item_id \
          WHERE r.org_id = $1 \
            AND COALESCE(r.branch_id, '00000000-0000-0000-0000-000000000000'::uuid) \
              = COALESCE($2::uuid, '00000000-0000-0000-0000-000000000000'::uuid) \
            AND m.deleted_at IS NULL \
          ORDER BY r.sort_order, r.cost_amount, m.name",
    )
    .bind(org_id)
    .bind(branch_id)
    .fetch_all(exec)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(
                menu_item_id,
                name,
                image_url,
                base_price,
                cost_currency,
                cost_amount,
                sort_order,
            )| {
                RewardItem {
                    menu_item_id,
                    name,
                    image_url,
                    base_price,
                    cost_currency,
                    cost_amount,
                    sort_order,
                }
            },
        )
        .collect())
}

/// The catalogue a branch actually offers: its own list when it has one, else
/// the org's. An empty branch list means "inherit", not "no rewards" — a branch
/// that wants no rewards turns the program off for itself.
pub async fn load_effective_rewards(
    pool: &PgPool,
    org_id: Uuid,
    branch_id: Uuid,
) -> Result<(Vec<RewardItem>, bool), AppError> {
    let own = load_reward_rows(pool, org_id, Some(branch_id)).await?;
    if !own.is_empty() {
        return Ok((own, false));
    }
    Ok((load_reward_rows(pool, org_id, None).await?, true))
}

#[utoipa::path(get, path = "/loyalty/reward-items", tag = "loyalty", operation_id = "get_loyalty_reward_items",
    params(ScopeQuery), responses((status = 200, body = RewardCatalogue), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn get_reward_items(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    query: web::Query<ScopeQuery>,
) -> Result<HttpResponse, AppError> {
    let (org_id, claims) = scope_org(pool.get_ref(), &req, query.branch_id).await?;
    check_permission(pool.get_ref(), &claims, "loyalty", "read").await?;
    let (items, inherited) = match query.branch_id {
        Some(b) => {
            require_branch_access(pool.get_ref(), &claims, b).await?;
            load_effective_rewards(pool.get_ref(), org_id, b).await?
        }
        None => (load_reward_rows(pool.get_ref(), org_id, None).await?, false),
    };
    Ok(HttpResponse::Ok().json(RewardCatalogue {
        org_id,
        branch_id: query.branch_id,
        inherited,
        items,
    }))
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct RewardItemInput {
    pub menu_item_id: Uuid,
    /// `"points"` or `"visits"`. Omitted follows the scope's mode.
    #[serde(default)]
    pub cost_currency: Option<String>,
    /// Omitted follows the scope's `default_reward_cost`.
    #[serde(default)]
    pub cost_amount: Option<i32>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct PutRewardItems {
    pub branch_id: Option<Uuid>,
    /// The complete list for this scope, in order. An empty list clears the
    /// scope — for a branch that means going back to inheriting the org's.
    pub items: Vec<RewardItemInput>,
}

#[utoipa::path(put, path = "/loyalty/reward-items", tag = "loyalty", operation_id = "put_loyalty_reward_items",
    request_body = PutRewardItems, responses((status = 200, body = RewardCatalogue), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn put_reward_items(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    body: web::Json<PutRewardItems>,
) -> Result<HttpResponse, AppError> {
    let incoming = body.into_inner();
    let (org_id, claims) = scope_org(pool.get_ref(), &req, incoming.branch_id).await?;
    check_permission(pool.get_ref(), &claims, "loyalty", "update").await?;
    if let Some(b) = incoming.branch_id {
        require_branch_access(pool.get_ref(), &claims, b).await?;
    }

    // Defaults for anything the caller left out come from the scope in force —
    // so "add this item as a reward" means the obvious thing without the client
    // having to restate the program's own settings.
    let scope = match incoming.branch_id {
        Some(b) => load_effective(pool.get_ref(), org_id, b).await?,
        None => load_scope(pool.get_ref(), org_id, None)
            .await?
            .unwrap_or_else(|| LoyaltySettings::defaults(org_id, None)),
    };

    let ids: Vec<Uuid> = incoming.items.iter().map(|i| i.menu_item_id).collect();
    // Every item must belong to this org. Without this an admin could name
    // another tenant's item id and leak its name and price back through the
    // catalogue read.
    if !ids.is_empty() {
        let valid: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM menu_items \
              WHERE id = ANY($1) AND org_id = $2 AND deleted_at IS NULL",
        )
        .bind(&ids)
        .bind(org_id)
        .fetch_one(pool.get_ref())
        .await?;
        let distinct = ids.iter().collect::<std::collections::HashSet<_>>().len() as i64;
        if valid != distinct {
            return Err(AppError::BadRequest(
                "reward items must be active menu items of this org".into(),
            ));
        }
    }
    for item in &incoming.items {
        if let Some(c) = &item.cost_currency
            && !matches!(c.as_str(), "points" | "visits")
        {
            return Err(AppError::BadRequest(
                "cost_currency must be 'points' or 'visits'".into(),
            ));
        }
        if let Some(a) = item.cost_amount
            && a <= 0
        {
            return Err(AppError::BadRequest(
                "a reward must cost more than nothing".into(),
            ));
        }
    }

    // Replace the scope's list in one transaction so a failure never leaves a
    // half-written catalogue an admin would have to notice and repair.
    let mut tx = pool.begin().await?;
    sqlx::query(
        "DELETE FROM loyalty_reward_items WHERE org_id = $1 \
           AND COALESCE(branch_id, '00000000-0000-0000-0000-000000000000'::uuid) \
             = COALESCE($2::uuid, '00000000-0000-0000-0000-000000000000'::uuid)",
    )
    .bind(org_id)
    .bind(incoming.branch_id)
    .execute(&mut *tx)
    .await?;
    for (i, item) in incoming.items.iter().enumerate() {
        sqlx::query(
            "INSERT INTO loyalty_reward_items \
                (org_id, branch_id, menu_item_id, cost_currency, cost_amount, sort_order) \
             VALUES ($1,$2,$3,$4,$5,$6) ON CONFLICT DO NOTHING",
        )
        .bind(org_id)
        .bind(incoming.branch_id)
        .bind(item.menu_item_id)
        .bind(item.cost_currency.as_deref().unwrap_or(&scope.mode))
        .bind(item.cost_amount.unwrap_or(scope.default_reward_cost))
        .bind(i as i32)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;

    let items = load_reward_rows(pool.get_ref(), org_id, incoming.branch_id).await?;
    Ok(HttpResponse::Ok().json(RewardCatalogue {
        org_id,
        branch_id: incoming.branch_id,
        inherited: false,
        items,
    }))
}

/// The org's own catalogue, with no branch in the picture — what a customer
/// reading their card at home is shown, and what goes on the back of the pass.
pub async fn load_effective_rewards_org(
    pool: &PgPool,
    org_id: Uuid,
) -> Result<Vec<RewardItem>, AppError> {
    load_reward_rows(pool, org_id, None).await
}
