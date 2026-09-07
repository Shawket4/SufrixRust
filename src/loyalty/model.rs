//! Members, their balances, and the two writes that move points.
//!
//! Both writes take a transaction rather than a pool: earning happens inside the
//! order's own transaction (so a sale and its points commit together or not at
//! all), and redeeming is a single statement whose guard lives in the database.

use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Postgres, Transaction};
use utoipa::ToSchema;
use uuid::Uuid;

use super::earn::{self, EarnRule, Mode, OrderAmounts};
use super::settings::{RewardItem, load_effective, load_effective_rewards};
use crate::errors::AppError;

/// A member as the teller, the admin and the pass all see them.
///
/// Both balances travel, because an org may switch mode (or run points at one
/// branch and stamps at another) and what a customer earned under the old rules
/// is still theirs. `mode` says which one is LIVE where the question was asked,
/// and `balance` is that one — so a caller never has to pick.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct MemberView {
    pub id: Uuid,
    pub org_id: Uuid,
    pub name: String,
    pub phone: String,
    pub points_balance: i32,
    pub visits_balance: i32,
    pub lifetime_points: i32,
    pub lifetime_visits: i32,
    /// `"points"` or `"visits"` — what the branch that asked collects.
    pub mode: String,
    /// The live balance, in `mode`'s currency.
    pub balance: i32,
    /// The cheapest reward on offer here, in `mode`'s currency — what the
    /// progress line counts towards. Falls back to the scope's default cost
    /// when no rewards have been curated.
    pub next_reward_cost: i32,
    /// `next_reward_cost - balance`, floored at zero.
    pub points_to_next_reward: i32,
    /// The balance affords at least one reward on offer here.
    pub can_redeem: bool,
    pub locale: String,
    pub enrolled_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct MemberRow {
    pub id: Uuid,
    pub org_id: Uuid,
    pub name: String,
    pub phone: String,
    pub member_token: String,
    pub points_balance: i32,
    pub visits_balance: i32,
    pub lifetime_points: i32,
    pub lifetime_visits: i32,
    pub locale: String,
    pub apple_serial: Option<String>,
    pub apple_auth_token: Option<String>,
    pub google_object_id: Option<String>,
    /// When the pass this member holds was last rebuilt — the tag Apple's
    /// devices ask "has anything changed since?" against.
    pub pass_updated_at: Option<chrono::DateTime<chrono::Utc>>,
    pub enrolled_at: chrono::DateTime<chrono::Utc>,
}

pub const MEMBER_COLS: &str = "id, org_id, name, phone, member_token, points_balance, \
    visits_balance, lifetime_points, lifetime_visits, locale, apple_serial, apple_auth_token, \
    google_object_id, pass_updated_at, enrolled_at";

impl MemberRow {
    /// The balance that counts under `mode`.
    pub fn balance_in(&self, mode: Mode) -> i32 {
        match mode {
            Mode::Points => self.points_balance,
            Mode::Visits => self.visits_balance,
        }
    }

    /// Dress the row with the mode and cheapest reward in force where it was
    /// asked for.
    pub fn view(self, mode: Mode, next_reward_cost: i32) -> MemberView {
        let balance = self.balance_in(mode);
        MemberView {
            balance,
            mode: mode.as_str().into(),
            next_reward_cost,
            points_to_next_reward: (next_reward_cost - balance).max(0),
            can_redeem: balance >= next_reward_cost,
            id: self.id,
            org_id: self.org_id,
            name: self.name,
            phone: self.phone,
            points_balance: self.points_balance,
            visits_balance: self.visits_balance,
            lifetime_points: self.lifetime_points,
            lifetime_visits: self.lifetime_visits,
            locale: self.locale,
            enrolled_at: self.enrolled_at,
        }
    }
}

/// The cheapest reward on offer, or `None` when the catalogue is empty.
///
/// This is what the pass counts towards. Aiming at the cheapest is the only
/// target that is always true: a customer who can afford the espresso HAS
/// earned a reward, whatever the cake costs.
///
/// Takes no mode. A catalogue arrives priced in its scope's own currency
/// (`settings::in_mode`), so there is nothing to filter by — and filtering by
/// the row's stored copy is what made this return `None` for a shop with a full
/// catalogue, quietly resetting every customer's target to the program default.
pub fn cheapest_cost(rewards: &[RewardItem]) -> Option<i32> {
    rewards.iter().map(|r| r.cost_amount).min()
}

/// Look a member up by the token their pass barcode carries.
///
/// The token is globally unique and carries no org, because that is all a
/// scanned QR gives us — the caller checks that the member's org matches theirs.
pub async fn find_by_token<'e, E>(exec: E, token: &str) -> Result<Option<MemberRow>, AppError>
where
    E: sqlx::PgExecutor<'e>,
{
    Ok(sqlx::query_as(&format!(
        "SELECT {MEMBER_COLS} FROM loyalty_customers \
         WHERE member_token = $1 AND deleted_at IS NULL"
    ))
    .bind(token)
    .fetch_optional(exec)
    .await?)
}

/// The manual fallback for a customer whose phone is dead.
pub async fn find_by_phone<'e, E>(
    exec: E,
    org_id: Uuid,
    phone: &str,
) -> Result<Option<MemberRow>, AppError>
where
    E: sqlx::PgExecutor<'e>,
{
    Ok(sqlx::query_as(&format!(
        "SELECT {MEMBER_COLS} FROM loyalty_customers \
         WHERE org_id = $1 AND phone = $2 AND deleted_at IS NULL"
    ))
    .bind(org_id)
    .bind(phone)
    .fetch_optional(exec)
    .await?)
}

pub async fn find_by_id<'e, E>(exec: E, id: Uuid) -> Result<Option<MemberRow>, AppError>
where
    E: sqlx::PgExecutor<'e>,
{
    Ok(sqlx::query_as(&format!(
        "SELECT {MEMBER_COLS} FROM loyalty_customers WHERE id = $1 AND deleted_at IS NULL"
    ))
    .bind(id)
    .fetch_optional(exec)
    .await?)
}

/// The member plus the numbers in force at a branch, and what they could claim.
pub async fn member_with_context(
    pool: &PgPool,
    row: MemberRow,
    branch_id: Uuid,
) -> Result<(MemberView, Vec<RewardItem>), AppError> {
    let settings = load_effective(pool, row.org_id, branch_id).await?;
    let (rewards, _) = load_effective_rewards(pool, row.org_id, branch_id).await?;
    let mode = settings.mode();
    let target = cheapest_cost(&rewards).unwrap_or(settings.default_reward_cost);
    Ok((row.view(mode, target), rewards))
}

/// Award the points a settled order is worth.
///
/// Called from inside `create_order_inner`'s transaction, on both the live and
/// the `/sync/replay` path, so an offline till awards on drain with no extra
/// code. Returns the points awarded (0 when the program is off for the branch,
/// the sale was too small, or this order already earned).
///
/// Idempotency is the database's: `loyalty_transactions_earn_order_key` allows
/// one earn per order, so a replayed order cannot award twice however many times
/// it is flushed.
#[allow(clippy::too_many_arguments)]
pub async fn award_for_order(
    tx: &mut Transaction<'_, Postgres>,
    org_id: Uuid,
    branch_id: Uuid,
    customer_id: Uuid,
    order_id: Uuid,
    amounts: OrderAmounts,
    rule: EarnRule,
    enabled: bool,
    created_by: Option<Uuid>,
) -> Result<i32, AppError> {
    if !enabled {
        return Ok(0);
    }
    let points = earn::points_for(amounts, rule);
    if points <= 0 {
        // A sale below one point still attaches the member to the order (the
        // caller sets `orders.loyalty_customer_id`); it just buys nothing.
        return Ok(0);
    }
    let inserted = sqlx::query(
        "INSERT INTO loyalty_transactions \
            (org_id, customer_id, branch_id, kind, currency, points, order_id, basis_piastres, \
             rate_piastres_per_point, created_by) \
         VALUES ($1,$2,$3,'earn',$4,$5,$6,$7,$8,$9) \
         ON CONFLICT DO NOTHING",
    )
    .bind(org_id)
    .bind(customer_id)
    .bind(branch_id)
    .bind(rule.mode.as_str())
    .bind(points)
    .bind(order_id)
    .bind(earn::basis_piastres(amounts, rule))
    .bind(rule.piastres_per_point)
    .bind(created_by)
    .execute(&mut **tx)
    .await?;
    // Zero rows means this order had already earned — a replay, not a failure.
    Ok(if inserted.rows_affected() == 1 {
        points
    } else {
        0
    })
}

/// An admin correction, in either direction.
pub async fn adjust(
    pool: &PgPool,
    member: &MemberRow,
    branch_id: Uuid,
    mode: Mode,
    points: i32,
    note: Option<String>,
    created_by: Option<Uuid>,
) -> Result<MemberView, AppError> {
    if points == 0 {
        return Err(AppError::BadRequest(
            "An adjustment of zero points changes nothing".into(),
        ));
    }
    if member.balance_in(mode) + points < 0 {
        return Err(AppError::BadRequest(format!(
            "{} has {}; that adjustment would go negative",
            member.name,
            member.balance_in(mode)
        )));
    }
    sqlx::query(
        "INSERT INTO loyalty_transactions \
            (org_id, customer_id, branch_id, kind, currency, points, note, created_by) \
         VALUES ($1,$2,$3,'adjust',$4,$5,$6,$7)",
    )
    .bind(member.org_id)
    .bind(member.id)
    .bind(branch_id)
    .bind(mode.as_str())
    .bind(points)
    .bind(note)
    .bind(created_by)
    .execute(pool)
    .await?;
    let fresh = find_by_id(pool, member.id)
        .await?
        .ok_or_else(|| AppError::NotFound("Member not found".into()))?;
    Ok(fresh.view(mode, 0))
}

/// One line of a member's history.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, sqlx::FromRow)]
pub struct LedgerEntry {
    pub id: Uuid,
    pub kind: String,
    /// `"points"` or `"visits"` — which balance this row moved.
    pub currency: String,
    pub points: i32,
    pub branch_id: Uuid,
    pub branch_name: Option<String>,
    pub order_id: Option<Uuid>,
    /// Piastres the rule was applied to (earns only).
    pub basis_piastres: Option<i32>,
    pub reward_name: Option<String>,
    pub note: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

pub async fn ledger(
    pool: &PgPool,
    customer_id: Uuid,
    limit: i64,
) -> Result<Vec<LedgerEntry>, AppError> {
    Ok(sqlx::query_as(
        "SELECT t.id, t.kind::text AS kind, t.currency, t.points, t.branch_id, b.name AS branch_name, \
                t.order_id, t.basis_piastres, m.name AS reward_name, t.note, t.created_at \
           FROM loyalty_transactions t \
           LEFT JOIN branches b ON b.id = t.branch_id \
           LEFT JOIN menu_items m ON m.id = t.reward_menu_item_id \
          WHERE t.customer_id = $1 ORDER BY t.created_at DESC LIMIT $2",
    )
    .bind(customer_id)
    .bind(limit)
    .fetch_all(pool)
    .await?)
}
