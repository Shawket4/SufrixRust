//! Awarding a sale's points — an explicit act, not a side effect of checkout.
//!
//! A teller presses "add points" on the receipt, or on a past order in the
//! history, and this decides what that sale was worth. Splitting it from the
//! sale is what makes the 24-hour window possible at all: the sale is finished
//! and the customer can produce their card afterwards, at the counter or when
//! they come back tomorrow.
//!
//! ## What the server enforces, regardless of what any client believes
//! 1. **The window.** The button was pressed within [`AWARD_WINDOW_HOURS`] of the
//!    sale. The client hides the button after that; this is what makes it true.
//! 2. **The timestamp is bounded.** An offline till queues the moment the teller
//!    pressed, so a long drain does not lose a legitimate award — but that claim
//!    cannot be before the sale, and cannot be in the future. A forged
//!    `requested_at` can therefore only ever land *inside* the window it is
//!    claiming to be inside, which is the rule anyway.
//! 3. **Once.** `loyalty_transactions_earn_order_key` allows one earn per order,
//!    so a double tap, a retried request and a replayed offline op all converge
//!    on the same single award.
//! 4. **The amount.** Points come from the ORDER's own stored totals and the
//!    settings of the ORDER's branch. A till sends who, never how many.
//!
//! Live route / `*_inner` split, like every other POS-facing mutation, so
//! `/sync/replay` flushes a queued award through exactly this code.

use actix_web::{HttpRequest, HttpResponse, web};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::ToSchema;
use uuid::Uuid;

use super::model::{self, MemberRow, MemberView};
use super::settings::load_effective;
use super::wallet;
use crate::delivery::{normalize_phone, require_branch_access};
use crate::errors::{AppError, AppErrorResponse};
use crate::orgs::handlers::extract_claims;
use crate::permissions::checker::check_permission;

/// How long after a sale its points may still be claimed.
///
/// One day, so "I forgot my card" is answerable on the customer's next visit
/// without leaving a sale open to being mined for points indefinitely.
pub const AWARD_WINDOW_HOURS: i64 = 24;

/// Tolerance for a till whose clock runs fast. Generous enough for ordinary
/// drift, far too small to reach past the window.
const FUTURE_SKEW_MINUTES: i64 = 5;

#[derive(Debug, Deserialize, ToSchema)]
pub struct AwardRequest {
    pub branch_id: Uuid,
    /// The server's order id — the history path, where the order is synced.
    #[serde(default)]
    pub order_id: Option<Uuid>,
    /// The client-minted idempotency key — the just-checked-out path, where the
    /// order may not have reached the server yet. Resolved to the same order.
    #[serde(default)]
    pub order_key: Option<Uuid>,

    /// Who. A scanned pass token, a typed phone, or an already-resolved member.
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub phone: Option<String>,
    #[serde(default)]
    pub customer_id: Option<Uuid>,

    /// When the teller pressed the button. Absent = now.
    ///
    /// An offline till stamps the press and queues it, so a drain days later
    /// still credits an award that was made in time. Bounded on arrival (see
    /// the module docs) so it cannot be used to reach outside the window.
    #[serde(default)]
    pub requested_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AwardResult {
    pub member: MemberView,
    /// Points this sale earned. 0 when it was too small to reach one point.
    pub points_awarded: i32,
    /// True when this order had already earned — a double tap, a retry, or a
    /// replayed offline op. Not an error: the outcome is the one that was asked
    /// for, and the response carries the balance that resulted.
    pub already_awarded: bool,
    pub order_id: Uuid,
}

/// The order an award names, with the amounts the points come from.
struct OrderFacts {
    id: Uuid,
    branch_id: Uuid,
    org_id: Uuid,
    created_at: DateTime<Utc>,
    voided: bool,
    subtotal: i32,
    discount_amount: i32,
    tax_amount: i32,
}

async fn load_order(
    pool: &PgPool,
    order_id: Option<Uuid>,
    order_key: Option<Uuid>,
) -> Result<OrderFacts, AppError> {
    if order_id.is_none() && order_key.is_none() {
        return Err(AppError::BadRequest(
            "Name the order by order_id or order_key".into(),
        ));
    }
    let row: Option<(Uuid, Uuid, Uuid, DateTime<Utc>, bool, i32, i32, i32)> = sqlx::query_as(
        "SELECT o.id, o.branch_id, b.org_id, o.created_at, (o.voided_at IS NOT NULL), \
                o.subtotal, o.discount_amount, o.tax_amount \
           FROM orders o JOIN branches b ON b.id = o.branch_id \
          WHERE ($1::uuid IS NOT NULL AND o.id = $1) \
             OR ($1::uuid IS NULL AND o.idempotency_key = $2)",
    )
    .bind(order_id)
    .bind(order_key)
    .fetch_optional(pool)
    .await?;

    let (id, branch_id, org_id, created_at, voided, subtotal, discount_amount, tax_amount) = row
        .ok_or_else(|| {
            // A till whose order has not drained yet lands here. The op stays
            // queued and is retried, which is why this is a 404 and not a
            // dead-letter: the order is coming.
            AppError::NotFound("That sale is not on the server yet".into())
        })?;

    Ok(OrderFacts {
        id,
        branch_id,
        org_id,
        created_at,
        voided,
        subtotal,
        discount_amount,
        tax_amount,
    })
}

/// Is `requested_at` a claim this server will honour for this sale?
///
/// Pure, so the rule can be tested without a database and stated once. Returns
/// the reason it was refused, for a message a teller can act on.
pub fn window_check(
    order_created_at: DateTime<Utc>,
    requested_at: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Result<(), String> {
    if requested_at > now + Duration::minutes(FUTURE_SKEW_MINUTES) {
        return Err("that request is dated in the future".into());
    }
    if requested_at < order_created_at {
        return Err("that request is dated before the sale".into());
    }
    if requested_at - order_created_at > Duration::hours(AWARD_WINDOW_HOURS) {
        return Err(format!(
            "the {AWARD_WINDOW_HOURS}-hour window for adding points to this sale has closed"
        ));
    }
    Ok(())
}

/// The live route. Tellers press the button; the permission is the same `update`
/// the redeem action needs.
#[utoipa::path(post, path = "/loyalty/award", tag = "loyalty", operation_id = "loyalty_award",
    request_body = AwardRequest, responses((status = 200, body = AwardResult), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn award(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    body: web::Json<AwardRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "loyalty", "update").await?;
    require_branch_access(pool.get_ref(), &claims, body.branch_id).await?;
    let actor = claims.user_id_safe().ok();
    award_inner(pool.get_ref(), body.into_inner(), actor, claims.org_id()).await
}

/// The core both the live route and `/sync/replay` call.
///
/// `caller_org` pins the request to one tenant; `None` is a super admin, who is
/// still bounded by the order's own org.
pub async fn award_inner(
    pool: &PgPool,
    body: AwardRequest,
    actor: Option<Uuid>,
    caller_org: Option<Uuid>,
) -> Result<HttpResponse, AppError> {
    let order = load_order(pool, body.order_id, body.order_key).await?;
    if let Some(org) = caller_org
        && order.org_id != org
    {
        return Err(AppError::NotFound("Order not found".into()));
    }
    if order.voided {
        return Err(AppError::Conflict(
            "That sale was voided — it earns no points".into(),
        ));
    }

    let now = Utc::now();
    let requested_at = body.requested_at.unwrap_or(now);
    window_check(order.created_at, requested_at, now).map_err(AppError::Conflict)?;

    // The ORDER's branch decides the rules, not the branch the request names: a
    // sale made at one branch keeps its own terms even if the card is presented
    // at another.
    let settings = load_effective(pool, order.org_id, order.branch_id).await?;
    if !settings.enabled {
        return Err(AppError::Conflict(
            "The loyalty program is switched off for the branch that made this sale".into(),
        ));
    }

    let member = resolve_member(pool, order.org_id, &body).await?;

    let mut tx = pool.begin().await?;
    let points = model::award_for_order(
        &mut tx,
        order.org_id,
        order.branch_id,
        member.id,
        order.id,
        crate::loyalty::earn::OrderAmounts {
            subtotal: order.subtotal,
            discount_amount: order.discount_amount,
            tax_amount: order.tax_amount,
        },
        settings.rule(),
        settings.enabled,
        actor,
    )
    .await?;
    // Record who the sale earned for even when it rounded down to zero points,
    // so "was this sale claimed?" has an answer either way.
    sqlx::query(
        "UPDATE orders SET loyalty_customer_id = $2 \
          WHERE id = $1 AND loyalty_customer_id IS NULL",
    )
    .bind(order.id)
    .bind(member.id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    // A zero here means the ledger already had this order — the award happened
    // on an earlier attempt, and the honest answer is the balance as it stands.
    let already_awarded = points == 0
        && sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM loyalty_transactions \
              WHERE order_id = $1 AND kind = 'earn')",
        )
        .bind(order.id)
        .fetch_one(pool)
        .await?;

    if points > 0 {
        // After the commit, never inside it — the same rule realtime follows.
        wallet::push_update(pool, member.id);
    }

    let (rewards, _) =
        crate::loyalty::settings::load_effective_rewards(pool, order.org_id, order.branch_id)
            .await?;
    let mode = settings.mode();
    let target = model::cheapest_cost(&rewards).unwrap_or(settings.default_reward_cost);
    let fresh = model::find_by_id(pool, member.id)
        .await?
        .ok_or_else(|| AppError::NotFound("Member not found".into()))?;
    Ok(HttpResponse::Ok().json(AwardResult {
        member: fresh.view(mode, target),
        points_awarded: points,
        already_awarded,
        order_id: order.id,
    }))
}

/// The member an award names, checked against the ORDER's org.
///
/// A token is globally unique and carries no org, so a member from another
/// tenant would otherwise resolve. "Not found" rather than "wrong org" keeps one
/// tenant from probing another's membership.
async fn resolve_member(
    pool: &PgPool,
    org_id: Uuid,
    body: &AwardRequest,
) -> Result<MemberRow, AppError> {
    let found = match (&body.customer_id, &body.token, &body.phone) {
        (Some(id), _, _) => model::find_by_id(pool, *id).await?,
        (_, Some(token), _) if !token.trim().is_empty() => {
            model::find_by_token(pool, token.trim()).await?
        }
        (_, _, Some(phone)) if !phone.trim().is_empty() => {
            model::find_by_phone(pool, org_id, &normalize_phone(phone)?).await?
        }
        _ => {
            return Err(AppError::BadRequest(
                "Scan a card or supply a phone number".into(),
            ));
        }
    };
    match found {
        Some(m) if m.org_id == org_id => Ok(m),
        _ => Err(AppError::NotFound("No member for that card".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(h: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_760_000_000 + h * 3600, 0).unwrap()
    }

    #[test]
    fn a_press_inside_the_window_is_honoured() {
        // Right away, and a whole day later to the second.
        assert!(window_check(t(0), t(0), t(0)).is_ok());
        assert!(window_check(t(0), t(24), t(24)).is_ok());
        // And a press made in time but drained days later still counts — this is
        // the case an offline till depends on.
        assert!(window_check(t(0), t(2), t(200)).is_ok());
    }

    #[test]
    fn a_press_past_the_window_is_refused() {
        let err = window_check(t(0), t(25), t(25)).unwrap_err();
        assert!(err.contains("24-hour window"), "{err}");
    }

    #[test]
    fn a_forged_timestamp_cannot_reach_outside_the_window() {
        // Backdating to before the sale, to make a stale press look fresh.
        assert!(window_check(t(10), t(9), t(40)).is_err());
        // Post-dating, to keep a window open past its close.
        assert!(window_check(t(0), t(40), t(0)).is_err());
    }

    #[test]
    fn a_till_whose_clock_runs_slightly_fast_is_tolerated() {
        let now = t(1);
        let slightly_ahead = now + Duration::minutes(FUTURE_SKEW_MINUTES - 1);
        assert!(window_check(t(0), slightly_ahead, now).is_ok());
        let far_ahead = now + Duration::hours(2);
        assert!(window_check(t(0), far_ahead, now).is_err());
    }
}
