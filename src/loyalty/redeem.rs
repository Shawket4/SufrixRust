//! Spending a balance on lines of a cart.
//!
//! A reward covers whole units of one line, and a basket may carry several — a
//! free coffee among four paid ones, or a free coffee and a free pastry when the
//! balance runs to both. The till names a member and some line indices; every
//! price in here is the server's.
//!
//! Redemption happens at TENDER, not after, because it changes what is owed.
//! (Earning is the opposite: it is a separate act with a 24-hour window — see
//! `loyalty::award`. Pay less now, collect after.)
//!
//! ## Why this cannot be done offline
//! A balance is shared state that any till in the org can spend, and a
//! redemption gives away goods. Two offline tills could each honour the last
//! reward and neither could be undone — the coffee is gone. So the POS refuses
//! to apply a reward while disconnected, and this module is only ever reached
//! by a live request.

use serde::Serialize;
use sqlx::{PgPool, Postgres, Transaction};
use utoipa::ToSchema;
use uuid::Uuid;

use super::model::{self, MemberRow};
use super::settings::{load_effective, load_effective_rewards};
use crate::errors::AppError;
use crate::orders::handlers::{LoyaltyRedemptionInput, OrderItemInput};

/// One priced redemption, ready to apply.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PlannedRedemption {
    pub item_index: usize,
    pub menu_item_id: Uuid,
    pub units: i32,
    /// `"points"` or `"visits"`.
    pub currency: String,
    /// Total spent for this line: the reward's cost × units.
    pub cost: i32,
}

/// The whole plan for one sale. Empty when nothing was redeemed, which is the
/// overwhelmingly common case and costs nothing to carry.
#[derive(Debug, Clone, Default)]
pub struct RedemptionPlan {
    pub member: Option<MemberRow>,
    pub lines: Vec<PlannedRedemption>,
}

impl RedemptionPlan {
    /// Units of `line_index` a reward pays for, if any.
    pub fn units_for(&self, line_index: usize) -> Option<i32> {
        self.lines
            .iter()
            .find(|l| l.item_index == line_index)
            .map(|l| l.units)
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }
}

/// Price and validate the rewards a sale wants to spend.
///
/// Everything that could make a redemption dishonest is refused here, before a
/// single price is computed: an unknown member, another tenant's member, a line
/// that is not a reward at this branch, more units than the line holds, or a
/// balance that does not cover it.
pub async fn plan(
    pool: &PgPool,
    org_id: Uuid,
    branch_id: Uuid,
    customer_id: Option<Uuid>,
    redemptions: &[LoyaltyRedemptionInput],
    items: &[OrderItemInput],
) -> Result<RedemptionPlan, AppError> {
    if redemptions.is_empty() {
        return Ok(RedemptionPlan::default());
    }
    let customer_id = customer_id
        .ok_or_else(|| AppError::BadRequest("A reward needs the member it belongs to".into()))?;

    let member = model::find_by_id(pool, customer_id)
        .await?
        .filter(|m| m.org_id == org_id)
        .ok_or_else(|| AppError::NotFound("No member for that card".into()))?;

    let settings = load_effective(pool, org_id, branch_id).await?;
    if !settings.enabled {
        return Err(AppError::Conflict(
            "The loyalty program is switched off for this branch".into(),
        ));
    }
    let mode = settings.mode();
    let (catalogue, _) = load_effective_rewards(pool, org_id, branch_id).await?;

    let mut lines: Vec<PlannedRedemption> = Vec::new();
    let mut spent = 0i32;
    for r in redemptions {
        // By this point the index is resolved: the cart path sends it, and the
        // ticket-settle path has had `ticket_line_id` translated into it.
        let index = r
            .item_index
            .ok_or_else(|| AppError::BadRequest("Name the line to reward".into()))?;
        let item = items
            .get(index)
            .ok_or_else(|| AppError::BadRequest(format!("No line {index} to reward")))?;
        // One redemption per line: the ledger's uniqueness is (order, line), so
        // two rows for one line could not both be recorded, and a silently
        // dropped one is a free item nobody was charged for.
        if lines.iter().any(|l| l.item_index == index) {
            return Err(AppError::BadRequest(
                "One reward per line — raise the units instead".into(),
            ));
        }
        let units = r.units.unwrap_or(1);
        if units < 1 {
            return Err(AppError::BadRequest(
                "A reward must cover at least one unit".into(),
            ));
        }
        if units > item.quantity {
            return Err(AppError::BadRequest(format!(
                "That line has {} of them; a reward cannot cover {units}",
                item.quantity
            )));
        }
        // A bundle is priced as a whole and its components are resolved
        // server-side; covering "one unit" of it has no single honest meaning,
        // so it is refused rather than guessed at.
        let menu_item_id = item
            .menu_item_id
            .ok_or_else(|| AppError::BadRequest("A bundle cannot be taken as a reward".into()))?;

        let reward = catalogue
            .iter()
            // Priced in this branch's currency by the loader, so the item id
            // is the whole question.
            .find(|c| c.menu_item_id == menu_item_id)
            .ok_or_else(|| {
                AppError::BadRequest("That item is not a reward at this branch".into())
            })?;

        let cost = reward.cost_amount.saturating_mul(units);
        spent = spent.saturating_add(cost);
        lines.push(PlannedRedemption {
            item_index: index,
            menu_item_id,
            units,
            currency: mode.as_str().into(),
            cost,
        });
    }

    // One check against the whole basket, not one per line: a balance that
    // covers the first reward but not the second must fail the sale, not hand
    // over half of what the teller told the customer they were getting.
    let balance = member.balance_in(mode);
    if spent > balance {
        return Err(AppError::Conflict(format!(
            "{} has {balance}; those rewards cost {spent}",
            member.name
        )));
    }

    Ok(RedemptionPlan {
        member: Some(member),
        lines,
    })
}

/// Record the plan against a placed order, inside that order's transaction.
///
/// Idempotent per covered line (`loyalty_transactions_redeem_line_key`), so a
/// retried checkout lands the same free coffee exactly once.
pub async fn record(
    tx: &mut Transaction<'_, Postgres>,
    plan: &RedemptionPlan,
    org_id: Uuid,
    branch_id: Uuid,
    order_id: Uuid,
    created_by: Option<Uuid>,
) -> Result<(), AppError> {
    let Some(member) = &plan.member else {
        return Ok(());
    };
    for line in &plan.lines {
        sqlx::query(
            "INSERT INTO loyalty_transactions \
                (org_id, customer_id, branch_id, kind, currency, points, order_id, \
                 order_line_index, reward_menu_item_id, created_by) \
             VALUES ($1,$2,$3,'redeem',$4,$5,$6,$7,$8,$9) \
             ON CONFLICT DO NOTHING",
        )
        .bind(org_id)
        .bind(member.id)
        .bind(branch_id)
        .bind(&line.currency)
        .bind(-line.cost)
        .bind(order_id)
        .bind(line.item_index as i32)
        .bind(line.menu_item_id)
        .bind(created_by)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}
