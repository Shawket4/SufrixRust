use actix_web::{HttpMessage, HttpRequest, HttpResponse, web};
use chrono::Utc;
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    auth::jwt::Claims,
    errors::{AppError, AppErrorResponse},
    models::UserRole,
    orders::SOLD,
    permissions::checker::check_permission,
    sync::ActingContext,
};
use utoipa::{IntoParams, ToSchema};

/// Default page size when listing orders for a single shift (POS home stats, shift history).
const DEFAULT_PER_PAGE_SHIFT: i64 = 1000;
/// Default page size when listing orders for a whole branch (dashboard).
const DEFAULT_PER_PAGE_BRANCH: i64 = 100;
/// Upper bound on a single orders page. High enough that the POS can bulk-fetch
/// an entire shift's orders in one or two round trips (offline cache), but bounded
/// so a client can't request a near-unlimited result set in one query.
const MAX_PER_PAGE: i64 = 1000;

// ── Shared SELECT fragment ────────────────────────────────────
const ORDER_SELECT: &str =
    "SELECT o.id, o.branch_id, o.shift_id, o.teller_id, u.name AS teller_name,
     o.waiter_id, w.name AS waiter_name,
     o.order_number, o.order_ref, o.status::text, o.payment_method::text,
     COALESCE((SELECT json_agg(json_build_object('method', op.method, 'amount', op.amount) ORDER BY op.id)
               FROM order_payments op WHERE op.order_id = o.id), '[]'::json) AS payment_legs,
     o.subtotal, o.discount_type::text, o.discount_value,
     o.discount_amount, o.tax_amount, o.total_amount,
     o.amount_tendered, o.change_given, o.tip_amount, o.tip_payment_method, o.discount_id,
     o.customer_name, o.notes, o.order_type, o.delivery_fee, o.delivery_order_id,
     d.channel::text AS delivery_channel, d.customer_lat AS delivery_lat, d.customer_lng AS delivery_lng,
     o.voided_at, o.void_reason::text, o.void_note, o.voided_by, o.created_at
     FROM orders o JOIN users u ON u.id = o.teller_id
     LEFT JOIN users w ON w.id = o.waiter_id
     LEFT JOIN delivery_orders d ON d.id = o.delivery_order_id ";

// ── Shared summary aggregate columns ──────────────────────────
/// Aggregate columns hydrating [OrderSummary] (by name, via `FromRow`). Used by
/// both the list and export summary queries; assumes `orders o` is LEFT JOINed
/// to `delivery_orders d` (for the channel split). `exclude_idx`, when set, is
/// the bind position of a uuid[] of menu_item/bundle ids left out of the
/// `line_items` count ONLY — every money/count aggregate stays authoritative.
///
/// Money here is scoped by [`SOLD`], the SAME predicate the sales and shift
/// reports use. These columns used to filter on `status = 'completed'`, which
/// silently dropped orders still open on the KDS / on a waiter's ticket and made
/// this strip disagree with every other screen for the same day.
fn order_summary_cols(exclude_idx: Option<i32>) -> String {
    let exclude = exclude_idx
        .map(|i| format!(" AND COALESCE(oi.menu_item_id, oi.bundle_id) != ALL(${i}::uuid[])"))
        .unwrap_or_default();
    let sold = SOLD;
    format!(
    "COALESCE(SUM(CASE WHEN o.{sold} THEN o.total_amount ELSE 0 END), 0) AS revenue,
     COALESCE(SUM(CASE WHEN o.{sold} THEN 1 ELSE 0 END), 0) AS completed,
     COALESCE(SUM(CASE WHEN o.status::text = 'voided' THEN 1 ELSE 0 END), 0) AS voided,
     COALESCE(SUM(CASE WHEN o.{sold} THEN o.discount_amount ELSE 0 END), 0) AS discounts,
     COALESCE(SUM(CASE WHEN o.{sold} THEN COALESCE(o.tip_amount, 0) ELSE 0 END), 0) AS tips,
     COALESCE(SUM(CASE WHEN o.{sold} THEN o.delivery_fee ELSE 0 END), 0) AS delivery_fees,
     COALESCE(SUM(CASE WHEN o.{sold} AND o.order_type = 'delivery' THEN 1 ELSE 0 END), 0) AS delivery_orders,
     COALESCE(SUM(CASE WHEN o.{sold} AND o.order_type = 'delivery' THEN o.total_amount ELSE 0 END), 0) AS delivery_revenue,
     COALESCE(SUM(CASE WHEN o.{sold} AND d.channel::text = 'in_mall' THEN 1 ELSE 0 END), 0) AS in_mall_orders,
     COALESCE(SUM(CASE WHEN o.{sold} AND d.channel::text = 'in_mall' THEN o.total_amount ELSE 0 END), 0) AS in_mall_revenue,
     COALESCE(SUM(CASE WHEN o.{sold} AND d.channel::text = 'in_mall' THEN o.delivery_fee ELSE 0 END), 0) AS in_mall_fees,
     COALESCE(SUM(CASE WHEN o.{sold} AND d.channel::text = 'outside' THEN 1 ELSE 0 END), 0) AS outside_orders,
     COALESCE(SUM(CASE WHEN o.{sold} AND d.channel::text = 'outside' THEN o.total_amount ELSE 0 END), 0) AS outside_revenue,
     COALESCE(SUM(CASE WHEN o.{sold} AND d.channel::text = 'outside' THEN o.delivery_fee ELSE 0 END), 0) AS outside_fees,
     COALESCE(SUM(CASE WHEN o.{sold} THEN (SELECT COALESCE(SUM(oi.quantity), 0) FROM order_items oi WHERE oi.order_id = o.id{exclude}) ELSE 0 END), 0)::bigint AS line_items"
    )
}

/// SQL for the `payment_method` query filter, at bind position `idx` (a `text[]`).
///
/// Matches on what was ACTUALLY tendered (`order_payments`) as well as the
/// nominal label. Filtering on the legs alone is what makes "card" agree with
/// the card bucket in the sales and shift reports: a split sale stores the
/// literal `'mixed'` in `orders.payment_method`, so the old label-only filter
/// hid the card half of every split order while the reports counted it.
/// The nominal label is kept in the OR so `?payment_method=mixed` still selects
/// split orders as a group.
fn payment_method_filter(idx: i32) -> String {
    format!(
        " AND (o.payment_method::text = ANY(${idx}::text[]) \
           OR EXISTS (SELECT 1 FROM order_payments op \
                      WHERE op.order_id = o.id AND op.method = ANY(${idx}::text[])))"
    )
}

/// Parse a comma-separated uuid list (query-param form for uuid[] filters).
pub(crate) fn parse_uuid_csv(param: &str, raw: &str) -> Result<Option<Vec<Uuid>>, AppError> {
    let ids = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            Uuid::parse_str(s)
                .map_err(|_| AppError::BadRequest(format!("Invalid uuid in {param}: {s}")))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(if ids.is_empty() { None } else { Some(ids) })
}

// ── Models ────────────────────────────────────────────────────

/// One tender against an order (`order_payments`). A split sale has several.
#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct PaymentLeg {
    pub method: String,
    pub amount: i32,
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct Order {
    pub id: Uuid,
    pub branch_id: Uuid,
    pub shift_id: Uuid,
    pub teller_id: Uuid,
    pub teller_name: String,
    /// The WAITER who opened this order's ticket (`open_tickets.opened_by`),
    /// stamped server-side at settle time. `null` for direct teller sales and
    /// delivery orders (they never pass through a waiter's ticket).
    pub waiter_id: Option<Uuid>,
    pub waiter_name: Option<String>,
    pub order_number: i32,
    /// Human-readable, org-unique reference (e.g. "DT-260614-0042"). Additive
    /// alongside the per-shift order_number. Optional only during the rollout
    /// window before the historical backfill runs; never null afterwards.
    pub order_ref: Option<String>,
    pub status: String,
    /// The order's NOMINAL payment label. For a split order this is the literal
    /// `'mixed'` — a label that exists in no money report, because reports bucket
    /// by what was actually tendered. Use [`Order::payment_legs`] for the real
    /// methods; treat this as a display badge only.
    pub payment_method: String,
    /// What was ACTUALLY tendered, one entry per `order_payments` row — the same
    /// rows every money report buckets by. A single-tender order has one leg; a
    /// split order has one per leg (e.g. card 285.00 + cash 255.00). Empty on the
    /// response to order creation, where the legs are written just after the row
    /// this statement returns; every read hydrates it.
    #[sqlx(json)]
    pub payment_legs: Vec<PaymentLeg>,
    pub subtotal: i32,
    pub discount_type: Option<String>,
    pub discount_value: i32,
    pub discount_amount: i32,
    pub tax_amount: i32,
    pub total_amount: i32,
    pub amount_tendered: Option<i32>,
    pub change_given: Option<i32>,
    pub tip_amount: Option<i32>,
    pub tip_payment_method: Option<String>,
    pub discount_id: Option<Uuid>,
    pub customer_name: Option<String>,
    pub notes: Option<String>,
    /// Order origin: "dine_in" (POS sale) or "delivery" (finalized delivery
    /// order). Defaults to "dine_in" for every POS sale.
    pub order_type: String,
    /// Delivery charge in piastres, shown separately from the item subtotal.
    /// Always 0 for dine-in orders; for delivery orders
    /// `total_amount == subtotal + tax_amount + delivery_fee` (minus discount).
    pub delivery_fee: i32,
    /// Links a finalized delivery order back to its `delivery_orders` row
    /// (customer, address, channel, zone). `null` for dine-in orders.
    pub delivery_order_id: Option<Uuid>,
    /// Delivery channel ("in_mall" | "outside") of the linked delivery order,
    /// surfaced on the list so clients can flag + segment delivery orders
    /// without a per-order detail fetch. `null` for dine-in orders.
    pub delivery_channel: Option<String>,
    /// Customer location of the linked delivery order, so clients can link out
    /// to a map (e.g. Google Maps) without a per-order detail fetch. `null` for
    /// dine-in orders or delivery orders without captured coordinates.
    pub delivery_lat: Option<f64>,
    pub delivery_lng: Option<f64>,
    pub voided_at: Option<chrono::DateTime<chrono::Utc>>,
    pub void_reason: Option<String>,
    pub void_note: Option<String>,
    pub voided_by: Option<Uuid>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct OrderItem {
    pub id: Uuid,
    pub order_id: Uuid,
    pub menu_item_id: Option<Uuid>,
    pub item_name: String,
    #[schema(value_type = Object)]
    pub name_translations: serde_json::Value,
    pub size_label: Option<String>,
    pub unit_price: i32,
    pub quantity: i32,
    pub line_total: i32,
    pub notes: Option<String>,
    pub deductions_snapshot: serde_json::Value,
    pub bundle_id: Option<Uuid>,
    pub bundle_unit_price: Option<i32>,
    /// Full line COGS in piastres (recipe + addons + optionals + components).
    /// `null` ⟺ unknown.
    pub line_cost: Option<i64>,
    /// Recipe-only cost per unit in piastres (incl. swaps). `null` ⟺ unknown
    /// or bundle line.
    pub unit_cost: Option<i64>,
    /// True when any cost component could not be resolved.
    pub cost_missing: bool,
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct OrderItemAddon {
    pub id: Uuid,
    pub order_item_id: Uuid,
    pub addon_item_id: Uuid,
    pub addon_name: String,
    #[schema(value_type = Object)]
    pub name_translations: serde_json::Value,
    pub unit_price: i32,
    pub quantity: i32,
    pub line_total: i32,
    /// Ingredient cost of this addon line in piastres. `null` ⟺ unknown, or
    /// a swap addon (its cost lives in the item's recipe cost).
    pub line_cost: Option<i64>,
}

/// Serialize an `Option<BigDecimal>` as a JSON number (or null) instead of
/// bigdecimal's default string form, so the wire matches the `number` the
/// OpenAPI schema + generated POS client expect.
fn serialize_bigdecimal_opt_as_number<S>(
    v: &Option<sqlx::types::BigDecimal>,
    s: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    match v {
        Some(bd) => s.serialize_f64(bd.to_string().parse::<f64>().unwrap_or(0.0)),
        None => s.serialize_none(),
    }
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct OrderItemOptional {
    pub id: Uuid,
    pub order_item_id: Uuid,
    pub optional_field_id: Option<Uuid>,
    pub field_name: String,
    #[schema(value_type = Object)]
    pub name_translations: serde_json::Value,
    pub price: i32,
    pub org_ingredient_id: Option<Uuid>,
    pub ingredient_name: Option<String>,
    pub ingredient_unit: Option<String>,
    // bigdecimal's Serialize emits a JSON STRING ("0.5"), but the OpenAPI schema
    // (and the generated client) advertise a `number`. Without this adapter the
    // POS can't decode the create-order response → the queued sale never acks and
    // dead-letters even though it was saved. Emit a real JSON number.
    #[schema(value_type = Option<f64>)]
    #[serde(serialize_with = "serialize_bigdecimal_opt_as_number")]
    pub quantity_deducted: Option<sqlx::types::BigDecimal>,
    /// Ingredient cost per parent-item unit in piastres. `null` ⟺ unknown or
    /// no ingredient linked.
    pub cost: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct OrderFull {
    #[serde(flatten)]
    pub order: Order,
    pub items: Vec<OrderItemFull>,
    /// Non-fatal warnings raised while placing the order — currently used to
    /// flag ingredients that were oversold (stock driven below zero). Empty
    /// for reads/refunds.
    #[serde(default)]
    pub warnings: Vec<String>,
    /// Delivery context (customer phone, address, channel, zone), populated
    /// only on the single-order detail endpoint and only when the order
    /// originated from a delivery order. `null`/absent for dine-in orders.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery: Option<OrderDeliveryInfo>,
}

/// Customer-facing delivery context attached to a finalized delivery order's
/// detail view. Sourced from the linked `delivery_orders` row (joined to its
/// delivery zone for the zone name). The delivery *fee* lives on [Order];
/// this carries the non-financial fulfilment details a teller/manager needs.
#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct OrderDeliveryInfo {
    /// "in_mall" or "outside".
    pub channel: String,
    pub customer_phone: String,
    pub place_name: Option<String>,
    pub floor: Option<String>,
    pub unit_number: Option<String>,
    pub landmark: Option<String>,
    pub address_line: Option<String>,
    pub delivery_notes: Option<String>,
    /// Road distance (meters) used to price the delivery, when known.
    pub road_distance_meters: Option<i32>,
    /// Name of the matched delivery zone ring, when an outside order matched one.
    pub zone_name: Option<String>,
    /// Human-readable delivery reference (e.g. "D-DT-260614-0042").
    pub delivery_ref: Option<String>,
    /// Payment method the customer indicated at intake ("cash"/"card"); the
    /// teller confirms the actual method at finalize.
    pub payment_method_hint: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct OrderBundleComponentAddon {
    pub id: Uuid,
    pub order_line_id: Uuid,
    pub component_item_id: Uuid,
    pub addon_item_id: Uuid,
    pub addon_name: String,
    #[schema(value_type = Object)]
    pub name_translations: serde_json::Value,
    pub unit_price: i32,
    pub quantity: i32,
    pub line_total: i32,
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct OrderBundleComponentOptional {
    pub id: Uuid,
    pub order_line_id: Uuid,
    pub component_item_id: Uuid,
    pub optional_field_id: Option<Uuid>,
    pub field_name: String,
    #[schema(value_type = Object)]
    pub name_translations: serde_json::Value,
    pub price: i32,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct OrderBundleComponentFull {
    pub item_id: Uuid,
    pub item_name: String,
    #[schema(value_type = Object)]
    pub name_translations: serde_json::Value,
    pub quantity: i32,
    pub size_label: Option<String>,
    pub addons: Vec<OrderBundleComponentAddon>,
    pub optionals: Vec<OrderBundleComponentOptional>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct OrderItemFull {
    #[serde(flatten)]
    pub item: OrderItem,
    pub addons: Vec<OrderItemAddon>,
    pub optionals: Vec<OrderItemOptional>,
    #[serde(default)]
    pub bundle_components: Vec<OrderBundleComponentFull>,
}

#[derive(Deserialize, Serialize, ToSchema)]
pub struct PaymentSplitInput {
    pub method: String,
    pub amount: i32,
    pub reference: Option<String>,
}

pub use crate::orders::component_resolve::AddonInput;

#[derive(Deserialize, Serialize, Default, ToSchema)]
pub struct OrderItemInput {
    #[serde(default)]
    pub menu_item_id: Option<Uuid>,
    #[serde(default)]
    pub bundle_id: Option<Uuid>,
    #[serde(default)]
    pub size_label: Option<String>,
    pub quantity: i32,
    #[serde(default)]
    pub addons: Vec<AddonInput>,
    #[serde(default)]
    pub optional_field_ids: Vec<Uuid>,
    #[serde(default)]
    pub bundle_components: Vec<crate::orders::component_resolve::BundleComponentInput>,
    #[serde(default)]
    pub notes: Option<String>,
    /// Charged unit price (piastres) the POS applied for this item/bundle line. When
    /// present it is RECORDED as the line's unit_price; absent → the server's expected
    /// (catalog + branch override) price is used. Recording what the customer was
    /// actually charged keeps the DB equal to the printed receipt even when the POS's
    /// synced menu/override prices are stale or it was offline at sale time.
    #[serde(default)]
    pub unit_price: Option<i32>,
}

#[derive(Deserialize, Serialize, Default, ToSchema)]
pub struct CreateOrderRequest {
    pub branch_id: Uuid,
    pub shift_id: Uuid,
    pub payment_method: String,
    pub customer_name: Option<String>,
    pub notes: Option<String>,
    pub discount_type: Option<String>,
    pub discount_value: Option<i32>,
    pub discount_id: Option<Uuid>,
    pub amount_tendered: Option<i32>,
    pub tip_amount: Option<i32>,
    pub tip_payment_method: Option<String>,
    pub payment_splits: Option<Vec<PaymentSplitInput>>,
    pub items: Vec<OrderItemInput>,
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    // ── Charged money breakdown (POS source of truth) ─────────────────────────
    // When supplied these are RECORDED VERBATIM as what the customer paid; the
    // server uses its catalog only to compute an expected total and flag
    // deviations (never reject). Omitted → the server computes them (legacy /
    // pre-update POS builds / tests).
    #[serde(default)]
    pub subtotal: Option<i32>,
    #[serde(default)]
    pub discount_amount: Option<i32>,
    #[serde(default)]
    pub tax_amount: Option<i32>,
    #[serde(default)]
    pub total_amount: Option<i32>,
    #[serde(default)]
    pub change_given: Option<i32>,
    // Exactly-once key. The canonical, in-body idempotency token (preferred over
    // the legacy `Idempotency-Key` header): a client mints it once per sale and
    // it rides inside the persisted offline payload, so a replay after a lost
    // response — even months later — dedups against `orders.idempotency_key`.
    #[serde(default)]
    pub idempotency_key: Option<Uuid>,
    /// IGNORED by the server (accepted for backward compatibility only). The
    /// authoritative per-shift number is ALWAYS `MAX(order_number)+1` computed under
    /// the shift advisory lock — never the client value, which is used only on the
    /// device's local receipt. The byte-identical-at-reprint guarantee rides on
    /// `order_ref`, not this field. Two tills on one shift get distinct numbers
    /// (UNIQUE(shift_id, order_number) + the lock).
    #[serde(default)]
    pub order_number: Option<i32>,
    /// Client-minted order reference (`<BRANCH>-<YYMMDD>-<DEVICE>-<NNNN>`). Stored
    /// verbatim when present; absent → the server mints the deterministic
    /// shift-based ref. The global `UNIQUE(order_ref)` index keeps both paths
    /// collision-safe (a managed per-device code makes concurrent tills unique).
    #[serde(default)]
    pub order_ref: Option<String>,
    /// The loyalty member spending a balance on this sale. Required when
    /// `loyalty_redemptions` is non-empty, and ONLY for that: earning is a
    /// separate, later act (`POST /loyalty/award`), so a sale that redeems
    /// nothing never names a member here.
    #[serde(default)]
    pub loyalty_customer_id: Option<Uuid>,
    /// Rewards covering lines of this cart. Each names a line by its index in
    /// `items` and how many of that line's units the reward pays for, so a
    /// mixed basket can have one free coffee among four paid ones.
    #[serde(default)]
    pub loyalty_redemptions: Vec<LoyaltyRedemptionInput>,
}

/// One reward applied to one line of the cart.
#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct LoyaltyRedemptionInput {
    /// Index into `items`. An index rather than an id because a cart may hold
    /// the same menu item on two lines with different modifiers, and only the
    /// position tells them apart.
    ///
    /// Optional because a TICKET settle names its lines by id instead (see
    /// `ticket_line_id`) and the server fills this in — a till settling a ticket
    /// cannot see the order the server will flatten its rounds into, and a
    /// guessed index takes the wrong item off the bill.
    #[serde(default)]
    pub item_index: Option<usize>,
    /// `open_ticket_items.id` — how a ticket settle names the line to cover.
    /// Resolved to `item_index` by `settle_open_ticket` before pricing.
    #[serde(default)]
    pub ticket_line_id: Option<Uuid>,
    /// How many of that line's units the reward covers. Defaults to one.
    #[serde(default)]
    pub units: Option<i32>,
}

#[derive(Deserialize, Serialize, ToSchema)]
pub struct VoidOrderRequest {
    pub reason: String,
    /// Free-text explanation. Required when `reason` is "other".
    pub note: Option<String>,
    pub voided_at: Option<chrono::DateTime<chrono::Utc>>,
    pub restore_inventory: Option<bool>,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListOrdersQuery {
    pub branch_id: Option<Uuid>,
    pub shift_id: Option<Uuid>,
    pub updated_after: Option<chrono::DateTime<chrono::Utc>>,
    pub page: Option<i64>,
    pub per_page: Option<i64>,
    pub teller_name: Option<String>,
    /// Filter by the WAITER who opened the ticket (ILIKE, partial match). Matches
    /// only orders that carry a waiter (dine-in settled from a waiter's ticket).
    pub waiter_name: Option<String>,
    pub payment_method: Option<String>,
    pub status: Option<String>,
    pub from: Option<chrono::DateTime<chrono::Utc>>,
    pub to: Option<chrono::DateTime<chrono::Utc>>,
    /// Filter by order origin: "dine_in" or "delivery".
    pub order_type: Option<String>,
    /// Filter delivery orders by channel: "in_mall" or "outside".
    pub channel: Option<String>,
    /// When true, each order in `data` embeds its full line items
    /// (addons/optionals/bundle components) — the response shape becomes
    /// [PaginatedOrdersFull]. Lets offline-first clients cache complete
    /// orders in one round trip instead of fetching each order separately.
    pub include_items: Option<bool>,
    /// Comma-separated menu_item/bundle UUIDs left out of the summary's
    /// `line_items` count (units sold) — e.g. water bottles or service
    /// pseudo-items that inflate it. Affects ONLY that KPI: revenue, order
    /// counts, and the order rows themselves are untouched.
    pub exclude_items: Option<String>,
}

#[derive(Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct OrderSummary {
    pub revenue: i64,
    pub completed: i64,
    pub voided: i64,
    pub discounts: i64,
    pub tips: i64,
    /// Total delivery charges (piastres) across completed orders in scope.
    /// Lets the dashboard surface delivery revenue separately from item sales.
    #[serde(default)]
    pub delivery_fees: i64,
    // ── Delivery channel split (completed orders in scope) ──────────────
    /// Count of completed delivery orders.
    #[serde(default)]
    pub delivery_orders: i64,
    /// Gross revenue (total_amount) of completed delivery orders.
    #[serde(default)]
    pub delivery_revenue: i64,
    /// In-mall channel: order count / gross revenue / delivery fees.
    #[serde(default)]
    pub in_mall_orders: i64,
    #[serde(default)]
    pub in_mall_revenue: i64,
    #[serde(default)]
    pub in_mall_fees: i64,
    /// Outside channel: order count / gross revenue / delivery fees.
    #[serde(default)]
    pub outside_orders: i64,
    #[serde(default)]
    pub outside_revenue: i64,
    #[serde(default)]
    pub outside_fees: i64,
    /// Units sold (SUM of order_items.quantity) across completed orders in
    /// scope. Counts units, not distinct lines, matching the item-sales
    /// reports ("3× burger" contributes 3).
    #[serde(default)]
    pub line_items: i64,
}

#[derive(Serialize, Deserialize, ToSchema)]
pub struct PaginatedOrders {
    pub data: Vec<Order>,
    pub total: i64,
    pub page: i64,
    pub per_page: i64,
    pub total_pages: i64,
    pub summary: OrderSummary, // ← add this
}

/// Same envelope as [PaginatedOrders] but each order carries its line items
/// (returned when `include_items=true`).
#[derive(Serialize, Deserialize, ToSchema)]
pub struct PaginatedOrdersFull {
    pub data: Vec<OrderFull>,
    pub total: i64,
    pub page: i64,
    pub per_page: i64,
    pub total_pages: i64,
    pub summary: OrderSummary,
}

#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct OrderPayment {
    pub id: Uuid,
    pub order_id: Uuid,
    pub method: String,
    pub amount: i32,
    pub reference: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct OrderExport {
    #[serde(flatten)]
    pub order: Order,
    pub items: Vec<OrderItemFull>,
    pub payments: Vec<OrderPayment>,
}

#[derive(Serialize, ToSchema)]
pub struct ExportResponse {
    pub data: Vec<OrderExport>,
    pub total: i64,
    pub generated_at: chrono::DateTime<chrono::Utc>,
    pub summary: OrderSummary,
    pub ingredient_costs: std::collections::HashMap<Uuid, i32>, // NEW: org_ingredient_id → cost_per_unit (piastres)
}

#[derive(Deserialize, Serialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ExportOrdersQuery {
    pub branch_id: Option<Uuid>,
    pub shift_id: Option<Uuid>,
    pub teller_name: Option<String>,
    /// Filter by the WAITER who opened the ticket (ILIKE, partial match).
    pub waiter_name: Option<String>,
    pub payment_method: Option<String>, // same comma-separated semantics
    pub status: Option<String>,
    pub from: Option<chrono::DateTime<chrono::Utc>>,
    pub to: Option<chrono::DateTime<chrono::Utc>>,
}

// ── Deduction helper ──────────────────────────────────────────
// The enriched `InventoryDeduction`, `LineCostSummary`, and the pure
// `summarize_line_costs` rollup live in `cost_math` so they can be unit-tested
// and fuzzed without a DB. Imported here so construction sites read unchanged.
use crate::orders::cost_math::{InventoryDeduction, summarize_line_costs};

// ── POST /orders ──────────────────────────────────────────────

#[utoipa::path(
    post,
    path = "/orders",
    tag = "orders",
    request_body = CreateOrderRequest,
    responses((status = 201, description = "Order created", body = OrderFull), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn create_order(
    req: HttpRequest,
    pool: crate::db::Db,
    // Optional so test apps (and any harness) that don't register the bus still
    // create orders — only the live KDS push is skipped when it's absent.
    hub: Option<web::Data<crate::realtime::hub::BranchEventHub>>,
    body: web::Json<CreateOrderRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "orders", "create").await?;
    require_branch_access(pool.get_ref(), &claims, body.branch_id).await?;

    // Prefer the in-body idempotency key (the canonical, replay-durable token);
    // fall back to the legacy `Idempotency-Key` header for older clients. Resolve
    // it HERE (the only place with the request headers) so the inner core — which
    // the replay path also calls, and which has no headers — works off `body`.
    let mut body = body;
    if body.idempotency_key.is_none() {
        body.idempotency_key = req
            .headers()
            .get("Idempotency-Key")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| Uuid::parse_str(s).ok());
    }
    create_order_inner(
        pool.clone(),
        body,
        ActingContext::live(&claims)?,
        hub.as_ref().map(|d| d.get_ref()),
        None, // a direct POS sale has no waiter — only ticket settles do
    )
    .await
}

/// Create-order core. LIVE attributes the order to the JWT teller and requires
/// the target shift to belong to them; REPLAY attributes it to the queued op's
/// embedded teller and drops that ownership filter (a different teller may be
/// flushing the device). BOTH still require the shift to be OPEN at the branch —
/// a queued order whose shift was force-closed server-side genuinely has nowhere
/// to land and must surface, not silently vanish — and dedup on the in-body
/// idempotency key.
pub(crate) async fn create_order_inner(
    pool: crate::db::Db,
    body: web::Json<CreateOrderRequest>,
    actor: ActingContext,
    // The realtime bus, for firing a LIVE order to the KDS. `None` on replay (a
    // queued offline order is historical and must not re-appear on the kitchen).
    hub: Option<&crate::realtime::hub::BranchEventHub>,
    // The WAITER who opened the settled ticket (`open_tickets.opened_by`). Only the
    // ticket-settle path supplies it; direct POS sales and delivery pass `None`.
    // Kept as an internal param (not on `CreateOrderRequest`) so a POS client can't
    // spoof the attribution — it's derived server-side from the ticket.
    waiter_id: Option<Uuid>,
) -> Result<HttpResponse, AppError> {
    if let Some(key) = body.idempotency_key
        && let Some(existing) =
            fetch_order_by_idempotency_key(pool.get_ref(), key, actor.org_id).await?
    {
        let items = fetch_order_items_full(pool.get_ref(), existing.id).await?;
        return Ok(HttpResponse::Ok().json(OrderFull {
            order: existing,
            items,
            warnings: Vec::new(),
            delivery: None,
        }));
    }

    if body.items.is_empty() {
        return Err(AppError::BadRequest(
            "Order must have at least one item".into(),
        ));
    }

    // The order must attach to a shift at this branch — and, for a LIVE teller
    // action, an OPEN one that belongs to them. A REPLAY drops the teller filter
    // (recorded history) AND the open requirement: a sale queued offline is
    // filed onto the shift it was placed under even if another till has since
    // closed that shift (the cross-device close race). The embedded shift_id —
    // stamped by the core when the shift was open on that device — is the proof
    // of attribution; the closed shift's cash total is reconciled after insert.
    let teller_match = if !actor.replay && actor.role == UserRole::Teller {
        Some(actor.teller_id)
    } else {
        None
    };
    let shift_ok: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM shifts \
         WHERE id = $1 AND branch_id = $2 AND (status = 'open' OR $4) \
           AND ($3::uuid IS NULL OR teller_id = $3))",
    )
    .bind(body.shift_id)
    .bind(body.branch_id)
    .bind(teller_match)
    .bind(actor.replay)
    .fetch_one(pool.get_ref())
    .await?;

    if !shift_ok {
        return Err(AppError::BadRequest(
            "Shift is not open, does not belong to this branch, or is not yours.".into(),
        ));
    }

    let org_id = actor.org_id;

    validate_payment_method(pool.get_ref(), org_id, &body.payment_method).await?;
    if let Some(dt) = &body.discount_type {
        validate_discount_type(dt)?;
    }
    if let Some(tpm) = &body.tip_payment_method {
        validate_payment_method(pool.get_ref(), org_id, tpm).await?;
    }

    // Snapshot is_cash for every method now, so a later method rename / is_cash
    // flip can't rewrite this order's contribution to shift cash totals (V30).
    let pm_is_cash: std::collections::HashMap<String, bool> = sqlx::query_as::<_, (String, bool)>(
        "SELECT name, is_cash FROM org_payment_methods WHERE org_id = $1",
    )
    .bind(org_id)
    .fetch_all(pool.get_ref())
    .await?
    .into_iter()
    .collect();
    let is_cash_of = |m: &str| pm_is_cash.get(m).copied().unwrap_or(m == "cash");

    let (resolved_discount_type, resolved_discount_value) = if let Some(disc_id) = body.discount_id
    {
        let row: Option<(String, i32)> = sqlx::query_as(
                "SELECT type::text, value FROM discounts WHERE id = $1 AND org_id = $2 AND is_active = true"
            )
            .bind(disc_id)
            .bind(org_id)
            .fetch_optional(pool.get_ref())
            .await?;
        match row {
            Some((dtype, dvalue)) => (Some(dtype), dvalue),
            None => {
                return Err(AppError::BadRequest(
                    "Discount not found or inactive".into(),
                ));
            }
        }
    } else {
        (body.discount_type.clone(), body.discount_value.unwrap_or(0))
    };

    let tax_rate: sqlx::types::BigDecimal = sqlx::query_scalar(
        "SELECT o.tax_rate FROM organizations o JOIN branches b ON b.org_id = o.id WHERE b.id = $1",
    )
    .bind(body.branch_id)
    .fetch_one(pool.get_ref())
    .await?;

    // ── Local types ───────────────────────────────────────────
    struct ResolvedOptional {
        optional_field_id: Uuid,
        field_name: String,
        name_translations: serde_json::Value,
        price: i32,
        org_ingredient_id: Option<Uuid>,
        ingredient_name: Option<String>,
        ingredient_unit: Option<String>,
        quantity_used: Option<f64>,
    }

    #[allow(dead_code)]
    struct ResolvedBundleComponent {
        item_id: Uuid,
        item_name: String,
        name_translations: serde_json::Value,
        quantity: i32,
        size_label: Option<String>,
        addons: Vec<ResolvedAddon>,
        optionals: Vec<ResolvedOptional>,
    }

    struct ResolvedItem {
        menu_item_id: Option<Uuid>,
        item_name: String,
        name_translations: serde_json::Value,
        size_label: Option<String>,
        /// Charged unit price recorded on the line (client value, else expected).
        unit_price: i32,
        /// True when this line's charged price/availability deviated from the catalog.
        price_flagged: bool,
        /// A reward paid for some or all of this line.
        is_reward: bool,
        /// Piastres the reward covered on this line. Subtracted from the stored
        /// line total so the persisted order agrees with the subtotal the
        /// customer was charged — the receipt and the books read the same.
        reward_covered: i32,
        quantity: i32,
        notes: Option<String>,
        addons: Vec<ResolvedAddon>,
        optionals: Vec<ResolvedOptional>,
        deductions: Vec<InventoryDeduction>,
        bundle_id: Option<Uuid>,
        bundle_unit_price: Option<i32>,
        bundle_components: Vec<ResolvedBundleComponent>,
        component_surcharge: i32,
    }

    struct ResolvedAddon {
        addon_item_id: Uuid,
        addon_name: String,
        name_translations: serde_json::Value,
        unit_price: i32,
        quantity: i32,
        /// False when the addon has no ingredient rows (additive addons only
        /// — swap addons fold into the recipe). No ingredients ⟹ cost-missing.
        has_ingredients: bool,
        /// True when this addon acted as a milk/coffee swap (cost lives in
        /// the recipe-scope deduction it replaced).
        is_swap: bool,
    }

    // ── Loyalty redemptions ─────────────────────────────────────────────────
    // Resolved BEFORE pricing: a reward changes what is owed, so a redemption
    // that cannot be honoured must fail the sale outright rather than leave a
    // line free that nobody paid for. Everything here is server-side — the till
    // names a member and some lines, never a price.
    let redemption_plan = crate::loyalty::redeem::plan(
        pool.get_ref(),
        actor.org_id,
        body.branch_id,
        body.loyalty_customer_id,
        &body.loyalty_redemptions,
        &body.items,
    )
    .await?;

    let mut resolved_items: Vec<ResolvedItem> = Vec::new();
    // `subtotal` accumulates the CHARGED line totals (what the customer paid);
    // `expected_subtotal` mirrors it using catalog + branch-override prices so we
    // can flag deviations without rejecting the order.
    let mut subtotal: i32 = 0;
    let mut expected_subtotal: i32 = 0;

    for (line_index, item_input) in body.items.iter().enumerate() {
        if item_input.quantity <= 0 {
            return Err(AppError::BadRequest("Item quantity must be > 0".into()));
        }

        let mut deductions: Vec<InventoryDeduction> = Vec::new();
        let mut resolved_addons: Vec<ResolvedAddon> = Vec::new();
        let mut resolved_optionals: Vec<ResolvedOptional> = Vec::new();
        let mut bundle_components = Vec::new();

        let mut component_surcharge: i32 = 0;
        // Note: `unit_price` returned here is the EXPECTED (catalog + branch override)
        // price; the client's charged price is overlaid after this block.
        // `expected_addon_per_unit` is the catalog addon total per single item unit
        // (0 for bundles, whose surcharge is computed separately); `branch_disabled`
        // is true when this branch has the item turned off (flagged, not rejected).
        let (
            resolved_menu_item_id,
            item_name,
            name_translations,
            unit_price,
            bundle_id,
            bundle_unit_price,
            expected_addon_per_unit,
            branch_disabled,
        ) = if let Some(b_id) = item_input.bundle_id {
            // ── 1. Resolve Bundle ─────────────────────────────
            let bundle: (Uuid, String, i32, String) = sqlx::query_as(
                "SELECT id, name, price, status::text FROM bundles WHERE id = $1 AND org_id = $2",
            )
            .bind(b_id)
            .bind(org_id)
            .fetch_optional(pool.get_ref())
            .await?
            .ok_or_else(|| AppError::NotFound(format!("Bundle {} not found", b_id)))?;

            if bundle.3 != "active" {
                return Err(AppError::BadRequest(format!(
                    "Bundle {} is not active",
                    bundle.1
                )));
            }

            // Branch availability
            let available_in_branch: bool = sqlx::query_scalar(
                "SELECT EXISTS(
                    SELECT 1 FROM bundle_branch_availability WHERE bundle_id = $1 AND branch_id = $2
                 ) OR NOT EXISTS(
                    SELECT 1 FROM bundle_branch_availability WHERE bundle_id = $1
                 )",
            )
            .bind(bundle.0)
            .bind(body.branch_id)
            .fetch_one(pool.get_ref())
            .await?;

            if !available_in_branch {
                return Err(AppError::BadRequest(format!(
                    "Bundle {} is not available in branch {}",
                    bundle.1, body.branch_id
                )));
            }

            // Date / Time window validation
            let order_time = body.created_at.unwrap_or_else(Utc::now);
            let branch_tz: String = sqlx::query_scalar(
                "SELECT COALESCE(b.timezone, o.timezone)::text
                 FROM branches b JOIN organizations o ON o.id = b.org_id WHERE b.id = $1",
            )
            .bind(body.branch_id)
            .fetch_one(pool.get_ref())
            .await?;

            let local_dt_rows: Option<(chrono::NaiveDate, chrono::NaiveTime)> = sqlx::query_as(
                "SELECT ($1::timestamptz AT TIME ZONE $2)::date, ($1::timestamptz AT TIME ZONE $2)::time"
            )
            .bind(order_time)
            .bind(&branch_tz)
            .fetch_optional(pool.get_ref())
            .await?;

            if let Some((local_date, local_time)) = local_dt_rows {
                let bundle_limits: (Option<chrono::NaiveDate>, Option<chrono::NaiveDate>, Option<chrono::NaiveTime>, Option<chrono::NaiveTime>) = sqlx::query_as(
                    "SELECT available_from_date, available_until_date, available_from_time, available_until_time \
                     FROM bundles WHERE id = $1"
                )
                .bind(bundle.0)
                .fetch_one(pool.get_ref())
                .await?;

                if let Some(from_d) = bundle_limits.0
                    && local_date < from_d
                {
                    return Err(AppError::BadRequest(format!(
                        "Bundle {} is not yet available",
                        bundle.1
                    )));
                }
                if let Some(until_d) = bundle_limits.1
                    && local_date > until_d
                {
                    return Err(AppError::BadRequest(format!(
                        "Bundle {} availability has expired",
                        bundle.1
                    )));
                }
                if let Some(from_t) = bundle_limits.2
                    && local_time < from_t
                {
                    return Err(AppError::BadRequest(format!(
                        "Bundle {} is not available at this hour",
                        bundle.1
                    )));
                }
                if let Some(until_t) = bundle_limits.3
                    && local_time > until_t
                {
                    return Err(AppError::BadRequest(format!(
                        "Bundle {} is not available at this hour",
                        bundle.1
                    )));
                }
            }

            // Resolve components (client snapshot or catalog defaults)
            let catalog: Vec<(Uuid, i32, String, serde_json::Value)> = sqlx::query_as(
                "SELECT bc.item_id, bc.quantity, mi.name, mi.name_translations \
                 FROM bundle_components bc \
                 JOIN menu_items mi ON mi.id = bc.item_id \
                 WHERE bc.bundle_id = $1 \
                 ORDER BY bc.position ASC",
            )
            .bind(bundle.0)
            .fetch_all(pool.get_ref())
            .await?;

            if catalog.is_empty() {
                return Err(AppError::BadRequest(format!(
                    "Bundle {} has no components",
                    bundle.1
                )));
            }

            let catalog_map: std::collections::HashMap<Uuid, (i32, String, serde_json::Value)> =
                catalog
                    .iter()
                    .map(|(id, qty, name, tr)| (*id, (*qty, name.clone(), tr.clone())))
                    .collect();

            let component_inputs: Vec<crate::orders::component_resolve::BundleComponentInput> =
                if item_input.bundle_components.is_empty() {
                    catalog
                        .iter()
                        .map(|(id, qty, _, _)| {
                            crate::orders::component_resolve::BundleComponentInput {
                                item_id: *id,
                                quantity: *qty,
                                size_label: None,
                                addons: vec![],
                                optional_field_ids: vec![],
                            }
                        })
                        .collect()
                } else {
                    item_input.bundle_components.clone()
                };

            for comp_in in component_inputs {
                let Some((catalog_qty, item_name, name_translations)) =
                    catalog_map.get(&comp_in.item_id)
                else {
                    return Err(AppError::BadRequest(format!(
                        "Item {} is not a component of bundle {}",
                        comp_in.item_id, bundle.1
                    )));
                };
                if comp_in.quantity != *catalog_qty {
                    return Err(AppError::BadRequest(format!(
                        "Invalid quantity for component {} in bundle {}",
                        item_name, bundle.1
                    )));
                }

                let line_qty = comp_in.quantity * item_input.quantity;
                let config = crate::orders::component_resolve::resolve_menu_item_configuration(
                    pool.get_ref(),
                    comp_in.item_id,
                    comp_in.size_label.clone(),
                    line_qty,
                    &comp_in.addons,
                    &comp_in.optional_field_ids,
                    body.branch_id,
                )
                .await?;

                component_surcharge += (config.addon_line + config.optional_line)
                    * comp_in.quantity
                    * item_input.quantity;

                for d in config.deductions {
                    deductions.push(InventoryDeduction {
                        org_ingredient_id: d.org_ingredient_id,
                        ingredient_name: d.ingredient_name,
                        unit: d.unit,
                        quantity: d.quantity,
                        source: format!("bundle_component:{}", item_name),
                        category: d.category,
                        addon_item_id: d.addon_item_id,
                        optional_field_id: d.optional_field_id,
                        component_item_id: Some(comp_in.item_id),
                        cost_per_unit: None,
                        line_cost: None,
                    });
                }

                let comp_addons: Vec<ResolvedAddon> = config
                    .addons
                    .into_iter()
                    .map(|a| ResolvedAddon {
                        addon_item_id: a.addon_item_id,
                        addon_name: a.addon_name,
                        name_translations: a.name_translations,
                        unit_price: a.unit_price,
                        quantity: a.quantity,
                        has_ingredients: true, // component-level costing rolls up via deductions
                        is_swap: false,
                    })
                    .collect();

                let comp_optionals: Vec<ResolvedOptional> = config
                    .optionals
                    .into_iter()
                    .map(|o| ResolvedOptional {
                        optional_field_id: o.optional_field_id,
                        field_name: o.field_name,
                        name_translations: o.name_translations,
                        price: o.price,
                        org_ingredient_id: o.org_ingredient_id,
                        ingredient_name: o.ingredient_name,
                        ingredient_unit: o.ingredient_unit,
                        quantity_used: o.quantity_used,
                    })
                    .collect();

                bundle_components.push(ResolvedBundleComponent {
                    item_id: comp_in.item_id,
                    item_name: item_name.clone(),
                    name_translations: name_translations.clone(),
                    quantity: comp_in.quantity,
                    size_label: comp_in.size_label.clone(),
                    addons: comp_addons,
                    optionals: comp_optionals,
                });
            }

            (
                None,
                bundle.1,
                serde_json::json!({}),
                bundle.2,
                Some(bundle.0),
                Some(bundle.2),
                0,
                false,
            )
        } else if let Some(m_item_id) = item_input.menu_item_id {
            // ── 2. Resolve Menu Item ──────────────────────────
            // Pull the branch override alongside the catalog row: the branch layer can
            // replace the price (price_override, piastres) and/or disable the item at
            // this branch. A disabled item is flagged (price_flagged) but NOT rejected
            // — an offline/stale POS may legitimately still be selling it.
            let (item_name, name_translations, base_price, branch_price_override, branch_disabled):
                (String, serde_json::Value, i32, Option<i32>, bool) = sqlx::query_as(
                "SELECT mi.name, mi.name_translations, mi.base_price,
                        bmo.price_override,
                        COALESCE(bmo.is_available, true) = false AS branch_disabled
                 FROM menu_items mi
                 LEFT JOIN branch_menu_overrides bmo
                        ON bmo.menu_item_id = mi.id AND bmo.branch_id = $2
                 WHERE mi.id = $1 AND mi.deleted_at IS NULL",
            )
            .bind(m_item_id)
            .bind(body.branch_id)
            .fetch_optional(pool.get_ref())
            .await?
            .ok_or_else(|| AppError::NotFound(
                format!("Menu item {} not found", m_item_id)
            ))?;

            // Branch-effective base: the override price replaces the catalog base_price.
            let base_price = branch_price_override.unwrap_or(base_price);

            let unit_price: i32 = match &item_input.size_label {
                Some(size) => {
                    // A per-(branch, item, size) override wins for that size; otherwise the
                    // catalog size price; otherwise the branch-effective base. (A branch base
                    // override never silently changes an explicitly-priced size.)
                    let branch_size: Option<i32> = sqlx::query_scalar(
                        "SELECT price_override FROM branch_menu_size_overrides \
                         WHERE branch_id = $1 AND menu_item_id = $2 AND size_label = $3",
                    )
                    .bind(body.branch_id)
                    .bind(m_item_id)
                    .bind(size)
                    .fetch_optional(pool.get_ref())
                    .await?;

                    match branch_size {
                        Some(bs) => bs,
                        None => {
                            let p: Option<i32> = sqlx::query_scalar(
                                "SELECT price_override FROM item_sizes \
                                 WHERE menu_item_id = $1 AND label = $2 AND is_active = true",
                            )
                            .bind(m_item_id)
                            .bind(size)
                            .fetch_optional(pool.get_ref())
                            .await?
                            .flatten();
                            p.unwrap_or(base_price)
                        }
                    }
                }
                None => base_price,
            };

            // Resolve recipe + addons (incl. milk/coffee swaps) + optionals via the
            // SHARED resolver that bundle components also use, so the deduction +
            // swap rules live in exactly one place. Map its output into the
            // order-line structs (which additionally carry cost fields).
            let config = crate::orders::component_resolve::resolve_menu_item_configuration(
                pool.get_ref(),
                m_item_id,
                item_input.size_label.clone(),
                item_input.quantity,
                &item_input.addons,
                &item_input.optional_field_ids,
                body.branch_id,
            )
            .await?;
            for d in config.deductions {
                deductions.push(InventoryDeduction {
                    org_ingredient_id: d.org_ingredient_id,
                    ingredient_name: d.ingredient_name,
                    unit: d.unit,
                    quantity: d.quantity,
                    source: d.source,
                    category: d.category,
                    addon_item_id: d.addon_item_id,
                    optional_field_id: d.optional_field_id,
                    component_item_id: None,
                    cost_per_unit: None,
                    line_cost: None,
                });
            }
            for a in config.addons {
                resolved_addons.push(ResolvedAddon {
                    addon_item_id: a.addon_item_id,
                    addon_name: a.addon_name,
                    name_translations: a.name_translations,
                    unit_price: a.unit_price,
                    quantity: a.quantity,
                    has_ingredients: a.has_ingredients,
                    is_swap: a.is_swap,
                });
            }
            for o in config.optionals {
                resolved_optionals.push(ResolvedOptional {
                    optional_field_id: o.optional_field_id,
                    field_name: o.field_name,
                    name_translations: o.name_translations,
                    price: o.price,
                    org_ingredient_id: o.org_ingredient_id,
                    ingredient_name: o.ingredient_name,
                    ingredient_unit: o.ingredient_unit,
                    quantity_used: o.quantity_used,
                });
            }

            // Capture the catalog (expected) addon total per single item unit, then
            // overlay the POS's charged addon prices — recorded verbatim, with any
            // deviation surfaced via the line price flag below.
            let expected_addon_per_unit: i32 = resolved_addons
                .iter()
                .map(|a| a.unit_price * a.quantity)
                .sum();
            for (i, a) in resolved_addons.iter_mut().enumerate() {
                if let Some(p) = item_input.addons.get(i).and_then(|ai| ai.unit_price) {
                    a.unit_price = p;
                }
            }

            (
                Some(m_item_id),
                item_name,
                name_translations,
                unit_price,
                None,
                None,
                expected_addon_per_unit,
                branch_disabled,
            )
        } else {
            return Err(AppError::BadRequest(
                "Each line item must have either menu_item_id or bundle_id".into(),
            ));
        };

        // `unit_price` from the resolution is the EXPECTED (catalog + branch override)
        // price; overlay the POS's charged price so the recorded line equals the
        // receipt. `resolved_addons` already carry charged prices (overlaid above for
        // menu items; bundle components stay server-priced via the surcharge).
        let expected_unit_price = unit_price;
        let unit_price = item_input.unit_price.unwrap_or(expected_unit_price);

        let charged_addon_per_unit: i32 = if bundle_id.is_some() {
            0
        } else {
            resolved_addons
                .iter()
                .map(|a| a.unit_price * a.quantity)
                .sum()
        };
        let optional_per_unit: i32 = if bundle_id.is_some() {
            0
        } else {
            resolved_optionals.iter().map(|o| o.price).sum()
        };

        let charged_line_subtotal = (unit_price + charged_addon_per_unit + optional_per_unit)
            * item_input.quantity
            + component_surcharge;
        let expected_line_subtotal =
            (expected_unit_price + expected_addon_per_unit + optional_per_unit)
                * item_input.quantity
                + component_surcharge;

        // A reward pays for whole units of this line, modifiers included — the
        // customer chose oat milk and the reward is the drink they chose. The
        // charge is reduced, never taken below zero.
        let covered = redemption_plan
            .units_for(line_index)
            .map(|units| {
                let per_unit = unit_price + charged_addon_per_unit + optional_per_unit;
                (per_unit * units).min(charged_line_subtotal)
            })
            .unwrap_or(0);
        let charged_line_subtotal = charged_line_subtotal - covered;
        let is_reward_line = covered > 0;

        // Flag the line when the charged price deviated from the catalog, or the item
        // was disabled at this branch (a stale/offline sale — recorded, not rejected).
        // A reward line is EXEMPT: it is meant to differ from the catalog price,
        // and flagging it would bury the real price anomalies in noise.
        let line_price_flagged =
            !is_reward_line && (branch_disabled || charged_line_subtotal != expected_line_subtotal);

        subtotal += charged_line_subtotal;
        expected_subtotal += expected_line_subtotal;

        resolved_items.push(ResolvedItem {
            menu_item_id: resolved_menu_item_id,
            item_name,
            name_translations,
            size_label: item_input.size_label.clone(),
            unit_price,
            is_reward: is_reward_line,
            reward_covered: covered,
            price_flagged: line_price_flagged,
            quantity: item_input.quantity,
            notes: item_input.notes.clone(),
            addons: resolved_addons,
            optionals: resolved_optionals,
            deductions,
            bundle_id,
            bundle_unit_price,
            bundle_components,
            component_surcharge,
        });
    }

    let tax_rate_f64: f64 = tax_rate.to_string().parse().unwrap_or(0.14);
    // Shared with the delivery-order discount path so the two can never drift.
    let calc_discount = |sub: i32| -> i32 {
        crate::discounts::handlers::calc_discount(
            resolved_discount_type.as_deref(),
            resolved_discount_value,
            sub,
        )
    };

    // Server EXPECTED breakdown (catalog + branch override) — used only to detect and
    // flag deviations; it never overrides what the customer was actually charged.
    let expected_discount = calc_discount(expected_subtotal);
    let expected_taxable = expected_subtotal - expected_discount;
    let expected_tax = (expected_taxable as f64 * tax_rate_f64).round() as i32;
    let expected_total = expected_taxable + expected_tax;

    // RECORDED breakdown — the POS's charged numbers are the source of truth; any field
    // the POS omits falls back to a server computation over the charged subtotal
    // (legacy / pre-update POS builds / tests).
    let subtotal = body.subtotal.unwrap_or(subtotal);
    let discount_amount = body
        .discount_amount
        .unwrap_or_else(|| calc_discount(subtotal))
        .clamp(0, subtotal);
    let taxable = subtotal - discount_amount;
    let tax_amount = body
        .tax_amount
        .unwrap_or_else(|| (taxable as f64 * tax_rate_f64).round() as i32);
    let total_amount = body.total_amount.unwrap_or(taxable + tax_amount);
    let change_given = body
        .change_given
        .or_else(|| body.amount_tendered.map(|t| (t - total_amount).max(0)));

    // Split payments must reconcile to the order total. They are the SOLE source
    // of drawer cash in compute_system_cash, so a mismatch (POS bug / spoof) would
    // silently leave the teller over or short with no way to trace it. (Per-split
    // method + positivity are validated again where the rows are inserted.)
    if let Some(splits) = &body.payment_splits {
        let split_total: i64 = splits.iter().map(|s| s.amount as i64).sum();
        if split_total != total_amount as i64 {
            return Err(AppError::BadRequest(format!(
                "Split payments ({split_total}) must sum to the order total ({total_amount})."
            )));
        }
    }

    // The order is flagged when any line deviated, the charged subtotal differs from the
    // catalog expectation, or the recorded total differs from the expected total.
    let price_flagged = resolved_items.iter().any(|r| r.price_flagged)
        || subtotal != expected_subtotal
        || total_amount != expected_total;

    let created_at = body.created_at.unwrap_or_else(chrono::Utc::now);
    // A future-dated order would mint its order_ref in a future business day and
    // hide the sale from "today" reports. An offline POS legitimately syncs PAST
    // timestamps, so reject only the future side (small clock-skew tolerance).
    crate::clock::reject_if_future(created_at, "created_at")?;

    // ── Cost snapshot ─────────────────────────────────────────
    // Resolve point-in-time ingredient costs once for the whole order and
    // stamp them onto the deduction entries; per-line / per-addon /
    // per-optional rollups happen at insert time.
    {
        let ingredient_ids: Vec<Uuid> = resolved_items
            .iter()
            .flat_map(|ri| ri.deductions.iter().filter_map(|d| d.org_ingredient_id))
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        let ingredient_costs = crate::costing::ingredient_costs_at(
            pool.get_ref(),
            body.branch_id,
            &ingredient_ids,
            created_at,
        )
        .await?;
        for ri in &mut resolved_items {
            for d in &mut ri.deductions {
                // Piastres per ingredient unit, straight from the catalog.
                let cost = d
                    .org_ingredient_id
                    .and_then(|id| ingredient_costs.get(&id))
                    .and_then(|c| c.to_f64());
                d.cost_per_unit = cost;
                d.line_cost = cost.map(|c| (d.quantity * c).round() as i64);
            }
        }
    }

    let mut tx = pool.get_ref().begin().await?;

    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1::text))")
        .bind(body.shift_id.to_string())
        .execute(&mut *tx)
        .await?;

    // Re-verify the shift under the per-shift lock and resolve its AUTHORITATIVE
    // branch + teller. close_shift takes the SAME advisory lock while it snapshots
    // cash, so this closes the TOCTOU window (an order's cash landing on a shift
    // that was just closed) AND pins the order to exactly the shift it attaches
    // to. Defense in depth: even if the one-open-per-branch / per-teller
    // invariants were somehow violated, the order is filed on the SHIFT'S OWN
    // branch (not the client-sent branch_id) and, for tellers, only onto their
    // own shift — so a sale can never be mis-registered onto the wrong shift or
    // branch.
    let shift_row: Option<(Uuid, Uuid, String)> =
        sqlx::query_as("SELECT branch_id, teller_id, status::text FROM shifts WHERE id = $1")
            .bind(body.shift_id)
            .fetch_optional(&mut *tx)
            .await?;
    let (shift_branch_id, shift_teller_id, shift_status) = shift_row.ok_or_else(|| {
        AppError::Conflict("Shift was closed before the order could be recorded".into())
    })?;
    // Live actions require the shift still open (TOCTOU vs a concurrent close).
    // A REPLAY is allowed onto a since-closed shift — the sale already happened;
    // its cash is reconciled onto the closed shift below.
    if shift_status != "open" && !actor.replay {
        return Err(AppError::Conflict(
            "Shift was closed before the order could be recorded".into(),
        ));
    }
    if shift_branch_id != body.branch_id {
        return Err(AppError::BadRequest(
            "Shift does not belong to the specified branch".into(),
        ));
    }
    // Live only: a teller's order must target their OWN shift. Replay bypasses
    // this — the order is attributed to its embedded teller, which may differ
    // from whoever is flushing the device backlog.
    if !actor.replay && actor.role == UserRole::Teller && shift_teller_id != actor.teller_id {
        return Err(AppError::Forbidden(
            "This shift belongs to another teller".into(),
        ));
    }

    // order_number stays SERVER-COMPUTED and per-shift — it's `UNIQUE(shift_id,
    // order_number)`, so a client value can't be authoritative (two devices would
    // both mint #1 into a shared shift and collide). A POS device PREDICTS the same
    // per-shift number offline (single numberer per shift) for its receipt.
    let order_number: i32 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(order_number), 0) + 1 FROM orders WHERE shift_id = $1",
    )
    .bind(body.shift_id)
    .fetch_one(&mut *tx)
    .await?;

    // The order_ref IS client-authoritative: a POS device mints it once with its
    // MANAGED DEVICE CODE + a per-device-day sequence (independent of order_number),
    // so the OFFLINE receipt is byte-identical to the synced reprint and concurrent
    // devices never collide. Stored VERBATIM when present; absent (dashboard/legacy)
    // → the server mints the deterministic <BRANCH>-<YYMMDD>-<SHIFT6>-<NNN> fallback.
    // The global UNIQUE(order_ref) index backstops either path.
    let order_ref = match &body.order_ref {
        Some(r) => r.clone(),
        None => {
            let (branch_code, biz_date): (String, chrono::NaiveDate) = sqlx::query_as(
                "SELECT b.code, ($1::timestamptz AT TIME ZONE COALESCE(b.timezone, o.timezone)::text)::date
                 FROM branches b JOIN organizations o ON o.id = b.org_id WHERE b.id = $2",
            )
            .bind(created_at)
            .bind(body.branch_id)
            .fetch_one(&mut *tx)
            .await?;
            let shift6 = body.shift_id.simple().to_string()[..6].to_uppercase();
            format!(
                "{}-{}-{}-{:03}",
                branch_code,
                biz_date.format("%y%m%d"),
                shift6,
                order_number
            )
        }
    };

    // Snapshot whether the tip was paid in cash (V30) — only meaningful when a
    // tip exists; resolves the tip method now so a later rename can't change it.
    let tip_is_cash: Option<bool> = if body.tip_amount.unwrap_or(0) > 0 {
        Some(is_cash_of(
            body.tip_payment_method
                .as_deref()
                .unwrap_or(&body.payment_method),
        ))
    } else {
        None
    };

    let order = match sqlx::query_as::<_, Order>(
        r#"
        INSERT INTO orders
            (branch_id, shift_id, teller_id, order_number,
             payment_method, subtotal, discount_type, discount_value,
             discount_amount, tax_amount, total_amount,
             amount_tendered, change_given, tip_amount, tip_payment_method,
             discount_id, customer_name, notes, status,
             idempotency_key, created_at, tip_is_cash, order_ref,
             price_flagged, price_expected_total, waiter_id)
        VALUES ($1, $2, $3, $4, $5, $6, $7::discount_type, $8,
                $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, 'completed', $19, $20, $21, $22,
                $23, $24, $25)
        RETURNING
            id, branch_id, shift_id, teller_id,
            (SELECT name FROM users WHERE id = $3) AS teller_name,
            waiter_id, (SELECT name FROM users WHERE id = $25) AS waiter_name,
            order_number, order_ref, status::text, payment_method::text,
            -- The payment rows are inserted just after this statement, so there is
            -- nothing to aggregate yet. Reads hydrate the real legs.
            '[]'::json AS payment_legs,
            subtotal, discount_type::text, discount_value,
            discount_amount, tax_amount, total_amount,
            amount_tendered, change_given, tip_amount, tip_payment_method, discount_id,
            customer_name, notes, order_type, delivery_fee, delivery_order_id,
            (SELECT channel::text FROM delivery_orders WHERE id = orders.delivery_order_id) AS delivery_channel,
            (SELECT customer_lat FROM delivery_orders WHERE id = orders.delivery_order_id) AS delivery_lat,
            (SELECT customer_lng FROM delivery_orders WHERE id = orders.delivery_order_id) AS delivery_lng,
            voided_at, void_reason::text, void_note, voided_by, created_at
        "#,
    )
    .bind(shift_branch_id) // authoritative: the order's branch IS its shift's branch
    .bind(body.shift_id)
    .bind(actor.teller_id)
    .bind(order_number)
    .bind(&body.payment_method)
    .bind(subtotal)
    .bind(&resolved_discount_type)
    .bind(resolved_discount_value)
    .bind(discount_amount)
    .bind(tax_amount)
    .bind(total_amount)
    .bind(body.amount_tendered)
    .bind(change_given)
    .bind(body.tip_amount.unwrap_or(0))
    .bind(body.tip_payment_method.as_deref())
    .bind(body.discount_id)
    .bind(&body.customer_name)
    .bind(&body.notes)
    .bind(body.idempotency_key)
    .bind(created_at)
    .bind(tip_is_cash)
    .bind(&order_ref)
    .bind(price_flagged)
    .bind(expected_total)
    .bind(waiter_id)
    .fetch_one(&mut *tx)
    .await
    {
        Ok(o) => o,
        // Idempotency-key race: a concurrent request with the same key already
        // committed this order. Replay the existing order instead of returning a
        // raw 500 from the unique-violation.
        Err(sqlx::Error::Database(db))
            if db.code().as_deref() == Some("23505")
                && db
                    .constraint()
                    .is_some_and(|c| c.contains("idempotency") || c.contains("order_ref")) =>
        {
            drop(tx);
            // Idempotent replay: a prior attempt already committed this order. Match
            // it by idempotency_key OR by the client-minted order_ref — a device
            // whose ref_seq counter rewound after a reinstall/restore re-sends with a
            // NEW idempotency key but a REUSED order_ref, which trips the global
            // UNIQUE(order_ref). Return the existing order, not a money-losing 409
            // that the offline client would dead-letter into a lost sale.
            if let Some(key) = body.idempotency_key
                && let Some(existing) = fetch_order_by_idempotency_key(pool.get_ref(), key, actor.org_id).await? {
                    let items = fetch_order_items_full(pool.get_ref(), existing.id).await?;
                    return Ok(HttpResponse::Ok().json(OrderFull { order: existing, items, warnings: Vec::new(), delivery: None }));
                }
            if let Some(order_ref) = &body.order_ref
                && let Some(existing) = fetch_order_by_order_ref(pool.get_ref(), order_ref, actor.org_id).await? {
                    let items = fetch_order_items_full(pool.get_ref(), existing.id).await?;
                    return Ok(HttpResponse::Ok().json(OrderFull { order: existing, items, warnings: Vec::new(), delivery: None }));
                }
            return Err(AppError::Conflict("Duplicate order".into()));
        }
        Err(e) => return Err(e.into()),
    };

    // Payment splits
    if let Some(splits) = &body.payment_splits {
        for split in splits {
            if split.amount <= 0 {
                return Err(AppError::BadRequest(
                    "Split payment amounts must be greater than 0".into(),
                ));
            }
            validate_payment_method(pool.get_ref(), org_id, &split.method).await?;
            sqlx::query(
                "INSERT INTO order_payments (order_id, method, amount, reference, is_cash) \
                 VALUES ($1, $2, $3, $4, $5)",
            )
            .bind(order.id)
            .bind(&split.method)
            .bind(split.amount)
            .bind(&split.reference)
            .bind(is_cash_of(&split.method))
            .execute(&mut *tx)
            .await?;
        }
    } else {
        sqlx::query(
            "INSERT INTO order_payments (order_id, method, amount, is_cash) \
             VALUES ($1, $2, $3, $4)",
        )
        .bind(order.id)
        .bind(&body.payment_method)
        .bind(total_amount)
        .bind(is_cash_of(&body.payment_method))
        .execute(&mut *tx)
        .await?;
    }

    // Late replay onto an already-closed shift: reconcile its cash snapshot.
    // The physical cash for this sale was already in the drawer when the teller
    // counted it at close, so `closing_cash_declared` already reflects it — but
    // `closing_cash_system`, snapshotted at close, did not (the order hadn't
    // synced). Recompute the snapshot from the SAME formula close_shift uses
    // (over the now-inserted payments, visible within this tx) so `expected_cash`
    // and the generated `cash_discrepancy` reconcile. A no-op for card sales
    // (cash-only sum) and, guarded by the WHERE, for shifts still open.
    if actor.replay && shift_status != "open" {
        let system_cash =
            crate::shifts::handlers::compute_system_cash(&mut *tx, body.shift_id).await?;
        sqlx::query(
            "UPDATE shifts SET closing_cash_system = $1 WHERE id = $2 AND status <> 'open'",
        )
        .bind(system_cash as i32)
        .bind(body.shift_id)
        .execute(&mut *tx)
        .await?;
    }

    let mut order_items_full: Vec<OrderItemFull> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    // Build the slim kitchen display lines BEFORE the insert loop consumes
    // `resolved_items` (live orders fire to the KDS after the items commit).
    let kitchen_lines: Vec<crate::kitchen::KitchenLine> = if actor.replay {
        Vec::new()
    } else {
        resolved_items
            .iter()
            .map(|ri| {
                let modifiers: Vec<String> = ri
                    .addons
                    .iter()
                    .map(|a| {
                        if a.quantity > 1 {
                            format!("{}× {}", a.quantity, a.addon_name)
                        } else {
                            a.addon_name.clone()
                        }
                    })
                    .collect();
                crate::kitchen::KitchenLine {
                    menu_item_id: ri.menu_item_id,
                    name: ri.item_name.clone(),
                    qty: ri.quantity,
                    size_label: ri.size_label.clone(),
                    modifiers,
                    notes: ri.notes.clone(),
                    // Teller orders fire to the KDS LIVE (online) only → server ids.
                    kitchen_item_id: None,
                }
            })
            .collect()
    };

    for resolved in resolved_items {
        let line_total = (resolved.unit_price * resolved.quantity + resolved.component_surcharge
            - resolved.reward_covered)
            .max(0);
        let snapshot = serde_json::to_value(&resolved.deductions)
            .unwrap_or_else(|_| serde_json::Value::Array(Vec::new()));

        let has_uncosted_addon = resolved
            .addons
            .iter()
            .any(|a| !a.is_swap && !a.has_ingredients);
        let costs = summarize_line_costs(
            &resolved.deductions,
            resolved.quantity,
            resolved.bundle_id.is_some(),
            has_uncosted_addon,
        );

        let order_item = sqlx::query_as::<_, OrderItem>(
            r#"INSERT INTO order_items
                (order_id, menu_item_id, item_name, name_translations, size_label,
                 unit_price, quantity, line_total, notes, deductions_snapshot,
                 bundle_id, bundle_unit_price, line_cost, unit_cost, cost_missing,
                 price_flagged, is_reward)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17)
               RETURNING id, order_id, menu_item_id, item_name, name_translations, size_label,
                         unit_price, quantity, line_total, notes, deductions_snapshot,
                         bundle_id, bundle_unit_price, line_cost, unit_cost, cost_missing"#,
        )
        .bind(order.id)
        .bind(resolved.menu_item_id)
        .bind(&resolved.item_name)
        .bind(&resolved.name_translations)
        .bind(&resolved.size_label)
        .bind(resolved.unit_price)
        .bind(resolved.quantity)
        .bind(line_total)
        .bind(&resolved.notes)
        .bind(snapshot)
        .bind(resolved.bundle_id)
        .bind(resolved.bundle_unit_price)
        .bind(costs.line_cost)
        .bind(costs.unit_cost)
        .bind(costs.cost_missing)
        .bind(resolved.price_flagged)
        .bind(resolved.is_reward)
        .fetch_one(&mut *tx)
        .await?;

        if let Some(_b_id) = resolved.bundle_id {
            for comp in &resolved.bundle_components {
                // Per-component cost: every enriched deduction attributed to
                // this component. None when any entry is unknown or the
                // component contributed no deductions (no recipe).
                let comp_entries: Vec<&InventoryDeduction> = resolved
                    .deductions
                    .iter()
                    .filter(|d| d.component_item_id == Some(comp.item_id))
                    .collect();
                let comp_cost: Option<i64> = if comp_entries.is_empty()
                    || comp_entries.iter().any(|d| d.cost_per_unit.is_none())
                {
                    None
                } else {
                    let cost: f64 = comp_entries
                        .iter()
                        .map(|d| d.cost_per_unit.unwrap() * d.quantity)
                        .sum();
                    Some(cost.round() as i64)
                };

                sqlx::query(
                    "INSERT INTO order_line_bundle_components \
                        (order_line_id, item_id, quantity, size_label, name_translations, line_cost) \
                     VALUES ($1, $2, $3, $4, $5, $6)",
                )
                .bind(order_item.id)
                .bind(comp.item_id)
                .bind(comp.quantity)
                .bind(&comp.size_label)
                .bind(&comp.name_translations)
                .bind(comp_cost)
                .execute(&mut *tx)
                .await?;

                for addon in &comp.addons {
                    let addon_line =
                        addon.unit_price * addon.quantity * comp.quantity * resolved.quantity;
                    sqlx::query(
                        "INSERT INTO order_line_bundle_component_addons \
                            (order_line_id, component_item_id, addon_item_id, addon_name, name_translations, \
                             unit_price, quantity, line_total) \
                         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
                    )
                    .bind(order_item.id)
                    .bind(comp.item_id)
                    .bind(addon.addon_item_id)
                    .bind(&addon.addon_name)
                    .bind(&addon.name_translations)
                    .bind(addon.unit_price)
                    .bind(addon.quantity)
                    .bind(addon_line)
                    .execute(&mut *tx)
                    .await?;
                }

                for opt in &comp.optionals {
                    sqlx::query(
                        "INSERT INTO order_line_bundle_component_optionals \
                            (order_line_id, component_item_id, optional_field_id, field_name, name_translations, \
                             price, org_ingredient_id, ingredient_name, ingredient_unit, quantity_deducted) \
                         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
                    )
                    .bind(order_item.id)
                    .bind(comp.item_id)
                    .bind(opt.optional_field_id)
                    .bind(&opt.field_name)
                    .bind(&opt.name_translations)
                    .bind(opt.price)
                    .bind(opt.org_ingredient_id)
                    .bind(&opt.ingredient_name)
                    .bind(&opt.ingredient_unit)
                    .bind(opt.quantity_used)
                    .execute(&mut *tx)
                    .await?;
                }
            }
        }

        // Addons
        let mut addon_rows: Vec<OrderItemAddon> = Vec::new();
        for addon in &resolved.addons {
            let addon_line = addon.unit_price * addon.quantity * resolved.quantity;

            // Additive addons: rollup of their attributed deduction entries.
            // Swap addons keep NULL — their cost lives in the recipe scope.
            let addon_cost: Option<i64> = if addon.is_swap || !addon.has_ingredients {
                None
            } else {
                let entries: Vec<&InventoryDeduction> = resolved
                    .deductions
                    .iter()
                    .filter(|d| d.source == "addon" && d.addon_item_id == Some(addon.addon_item_id))
                    .collect();
                if entries.is_empty() || entries.iter().any(|d| d.cost_per_unit.is_none()) {
                    None
                } else {
                    let cost: f64 = entries
                        .iter()
                        .map(|d| d.cost_per_unit.unwrap() * d.quantity)
                        .sum();
                    Some(cost.round() as i64)
                }
            };

            let row = sqlx::query_as::<_, OrderItemAddon>(
                r#"INSERT INTO order_item_addons
                    (order_item_id, addon_item_id, addon_name, name_translations, unit_price, quantity, line_total, line_cost)
                   VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
                   RETURNING id, order_item_id, addon_item_id, addon_name, name_translations,
                             unit_price, quantity, line_total, line_cost"#,
            )
            .bind(order_item.id)
            .bind(addon.addon_item_id)
            .bind(&addon.addon_name)
            .bind(&addon.name_translations)
            .bind(addon.unit_price)
            .bind(addon.quantity)
            .bind(addon_line)
            .bind(addon_cost)
            .fetch_one(&mut *tx)
            .await?;
            addon_rows.push(row);
        }

        // Optionals
        let mut optional_rows: Vec<OrderItemOptional> = Vec::new();
        for opt in &resolved.optionals {
            // Cost per parent-item unit (matches quantity_deducted semantics):
            // unit cost comes from the enriched deduction for this field.
            let opt_cost: Option<i64> = match (opt.quantity_used, opt.org_ingredient_id) {
                (Some(qty), Some(_)) => resolved
                    .deductions
                    .iter()
                    .find(|d| d.optional_field_id == Some(opt.optional_field_id))
                    .and_then(|d| d.cost_per_unit)
                    .map(|cost| (qty * cost).round() as i64),
                // No ingredient linked ⟹ genuinely zero marginal cost.
                _ => Some(0),
            };

            let row = sqlx::query_as::<_, OrderItemOptional>(
                r#"INSERT INTO order_item_optionals
                    (order_item_id, optional_field_id, field_name, name_translations, price,
                     org_ingredient_id, ingredient_name, ingredient_unit, quantity_deducted, cost)
                   VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
                   RETURNING id, order_item_id, optional_field_id, field_name, name_translations, price,
                             org_ingredient_id, ingredient_name, ingredient_unit, quantity_deducted, cost"#,
            )
            .bind(order_item.id)
            .bind(opt.optional_field_id)
            .bind(&opt.field_name)
            .bind(&opt.name_translations)
            .bind(opt.price)
            .bind(opt.org_ingredient_id)
            .bind(&opt.ingredient_name)
            .bind(&opt.ingredient_unit)
            .bind(opt.quantity_used)
            .bind(opt_cost)
            .fetch_one(&mut *tx)
            .await?;
            optional_rows.push(row);
        }

        // Apply inventory deductions through the ledger. Negative stock is
        // ALLOWED but FLAGGED: the movement records below_zero and the sale
        // surfaces a warning (Foodics default).
        for deduction in &resolved.deductions {
            let Some(ing_id) = deduction.org_ingredient_id else {
                tracing::warn!(
                    source     = %deduction.source,
                    ingredient = %deduction.ingredient_name,
                    "Deduction skipped — no org_ingredient_id"
                );
                continue;
            };

            // Post the sale to the ledger. The trigger upserts the branch
            // balance (creating it on an ingredient's first sale here) and
            // reports the resulting balance; negative is allowed but flagged.
            let posted = crate::inventory::movements::record_movement(
                &mut *tx,
                crate::inventory::movements::MovementParams {
                    branch_id: body.branch_id,
                    org_ingredient_id: ing_id,
                    movement_type: "sale",
                    quantity: -deduction.quantity,
                    unit_cost: deduction.cost_per_unit.map(|c| c.round() as i64),
                    reason: None,
                    source_type: Some("order"),
                    source_id: Some(order.id),
                    note: None,
                    created_by: Some(actor.teller_id),
                },
            )
            .await?;
            if posted.below_zero {
                warnings.push(format!(
                    "{} is oversold — stock is now {:.3} {}",
                    deduction.ingredient_name, posted.balance_after, deduction.unit
                ));
            }
        }

        order_items_full.push(OrderItemFull {
            item: order_item,
            addons: addon_rows,
            optionals: optional_rows,
            bundle_components: vec![],
        });
    }

    // Fire the order to the kitchen — LIVE orders only (a replayed offline order is
    // historical and must not re-appear on the KDS). Same source-agnostic substrate
    // the waiter tickets use; the routing mode decides where it renders client-side.
    let mut kitchen_ticket_id: Option<Uuid> = None;
    if !actor.replay && !kitchen_lines.is_empty() {
        kitchen_ticket_id = crate::kitchen::emit_kitchen_ticket(
            &mut tx,
            &crate::kitchen::EmitKitchen {
                org_id: actor.org_id,
                branch_id: body.branch_id,
                source_type: "order",
                source_id: order.id,
                round_number: 1,
                table_label: None,
                kitchen_ref: order.order_ref.as_deref(),
                kitchen_ticket_id: None, // live-only fire → server-generated id
            },
            &kitchen_lines,
        )
        .await?;
    }

    // Record what the rewards spent, inside the order's own transaction: a free
    // coffee and the ledger row that paid for it commit together or not at all.
    crate::loyalty::redeem::record(
        &mut tx,
        &redemption_plan,
        actor.org_id,
        body.branch_id,
        order.id,
        Some(actor.teller_id),
    )
    .await?;

    tx.commit().await?;
    // The balance on the customer's phone must not outlive the sale that spent
    // it. After the commit, never inside — the same rule realtime follows.
    if let Some(member) = &redemption_plan.member
        && !redemption_plan.is_empty()
    {
        crate::loyalty::wallet::push_update(pool.get_ref(), member.id);
    }

    if let (Some(hub), Some(kt_id)) = (hub, kitchen_ticket_id) {
        crate::kitchen::publish_kitchen(
            pool.get_ref(),
            hub,
            body.branch_id,
            "kitchen.fired",
            kt_id,
        )
        .await;
    }
    Ok(HttpResponse::Created().json(OrderFull {
        order,
        items: order_items_full,
        warnings,
        delivery: None,
    }))
}

// ── GET /orders ───────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/orders",
    tag = "orders",
    params(ListOrdersQuery),
    responses((status = 200, description = "List orders", body = PaginatedOrders), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_orders(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<ListOrdersQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "orders", "read").await?;

    let page = query.page.unwrap_or(1).max(1);
    let default_per_page = if query.shift_id.is_some() {
        DEFAULT_PER_PAGE_SHIFT
    } else {
        DEFAULT_PER_PAGE_BRANCH
    };
    let per_page = query
        .per_page
        .unwrap_or(default_per_page)
        .clamp(1, MAX_PER_PAGE);
    let offset = (page - 1) * per_page;

    let org_id = claims
        .org_id()
        .ok_or_else(|| AppError::Forbidden("No org in token".into()))?;

    let parsed_payment_methods = match &query.payment_method {
        Some(pm) => {
            let methods = parse_payment_methods(pool.get_ref(), org_id, pm).await?;
            if methods.is_empty() {
                None
            } else {
                Some(methods)
            }
        }
        None => None,
    };

    // Scope: a single shift, a single branch, or — when no shift is given and
    // branch_id is absent or the all-zeros (nil) UUID — every branch in the
    // caller's org (the "All branches" view). org_id was validated above, so
    // the org roll-up stays inside the caller's own org.
    let all_branches = query.shift_id.is_none() && query.branch_id.map_or(true, |b| b.is_nil());

    let (scope_condition, scope_id): (&str, Uuid) = if let Some(shift_id) = query.shift_id {
        let bid: Option<Uuid> = sqlx::query_scalar("SELECT branch_id FROM shifts WHERE id = $1")
            .bind(shift_id)
            .fetch_optional(pool.get_ref())
            .await?
            .flatten();
        let bid = bid.ok_or_else(|| AppError::NotFound("Shift not found".into()))?;
        require_branch_access(pool.get_ref(), &claims, bid).await?;
        ("o.shift_id = $1", shift_id)
    } else if all_branches {
        (
            "o.branch_id IN (SELECT id FROM branches WHERE org_id = $1 AND deleted_at IS NULL)",
            org_id,
        )
    } else {
        let bid = query
            .branch_id
            .expect("branch_id present when not all_branches");
        require_branch_access(pool.get_ref(), &claims, bid).await?;
        ("o.branch_id = $1", bid)
    };

    let mut data_filter = String::new();
    let mut count_filter = String::new();
    let mut data_idx = 2i32;
    let mut count_idx = 2i32;

    macro_rules! push_filter {
        ($col:expr, $opt:expr) => {
            if $opt.is_some() {
                data_filter.push_str(&format!(" AND {} ${}", $col, data_idx));
                count_filter.push_str(&format!(" AND {} ${}", $col, count_idx));
                data_idx += 1;
                count_idx += 1;
            }
        };
    }

    push_filter!("u.name ILIKE", query.teller_name);
    push_filter!("w.name ILIKE", query.waiter_name);
    if parsed_payment_methods.is_some() {
        data_filter.push_str(&payment_method_filter(data_idx));
        count_filter.push_str(&payment_method_filter(count_idx));
        data_idx += 1;
        count_idx += 1;
    }
    push_filter!("o.status::text =", query.status);
    push_filter!("o.created_at >=", query.from);
    push_filter!("o.created_at <=", query.to);
    push_filter!("o.updated_at >", query.updated_after);
    push_filter!("o.order_type =", query.order_type);
    push_filter!("d.channel::text =", query.channel);

    let data_sql = format!(
        "{} WHERE {} {} ORDER BY o.created_at DESC LIMIT ${} OFFSET ${}",
        ORDER_SELECT,
        scope_condition,
        data_filter,
        data_idx,
        data_idx + 1
    );
    let count_sql = format!(
        "SELECT COUNT(*) FROM orders o JOIN users u ON u.id = o.teller_id
         LEFT JOIN users w ON w.id = o.waiter_id
         LEFT JOIN delivery_orders d ON d.id = o.delivery_order_id
         WHERE {} {}",
        scope_condition, count_filter
    );

    tracing::debug!(
        data_params = data_idx + 1,
        count_params = count_idx - 1,
        "List orders filters constructed"
    );

    macro_rules! bind_filters {
        ($q:expr) => {{
            let mut q = $q;
            if let Some(ref v) = query.teller_name {
                q = q.bind(format!("%{}%", v));
            }
            if let Some(ref v) = query.waiter_name {
                q = q.bind(format!("%{}%", v));
            }
            if let Some(v) = &parsed_payment_methods {
                q = q.bind(v);
            }
            if let Some(ref v) = query.status {
                q = q.bind(v.clone());
            }
            if let Some(v) = query.from {
                q = q.bind(v);
            }
            if let Some(v) = query.to {
                q = q.bind(v);
            }
            if let Some(v) = query.updated_after {
                q = q.bind(v);
            }
            if let Some(ref v) = query.order_type {
                q = q.bind(v.clone());
            }
            if let Some(ref v) = query.channel {
                q = q.bind(v.clone());
            }
            q
        }};
    }

    let total: i64 = bind_filters!(sqlx::query_scalar(&count_sql).bind(scope_id))
        .fetch_one(pool.get_ref())
        .await?;

    // Exclusions ride the summary query only ($count_idx is the next free
    // placeholder after the shared filters, bound last below).
    let parsed_exclude_items = match &query.exclude_items {
        Some(raw) => parse_uuid_csv("exclude_items", raw)?,
        None => None,
    };
    let summary_sql = format!(
        "SELECT {} \
         FROM orders o JOIN users u ON u.id = o.teller_id \
         LEFT JOIN users w ON w.id = o.waiter_id \
         LEFT JOIN delivery_orders d ON d.id = o.delivery_order_id \
         WHERE {} {}",
        order_summary_cols(parsed_exclude_items.as_ref().map(|_| count_idx)),
        scope_condition,
        count_filter
    );

    let mut summary_q =
        bind_filters!(sqlx::query_as::<_, OrderSummary>(&summary_sql).bind(scope_id));
    if let Some(ref v) = parsed_exclude_items {
        summary_q = summary_q.bind(v);
    }
    let summary: OrderSummary = summary_q.fetch_one(pool.get_ref()).await?;

    let data: Vec<Order> = bind_filters!(sqlx::query_as::<_, Order>(&data_sql).bind(scope_id))
        .bind(per_page)
        .bind(offset)
        .fetch_all(pool.get_ref())
        .await?;

    let total_pages = (total as f64 / per_page as f64).ceil() as i64;

    if query.include_items.unwrap_or(false) {
        let ids: Vec<Uuid> = data.iter().map(|o| o.id).collect();
        let mut items_map = fetch_orders_items_full_batch(pool.get_ref(), &ids).await?;
        let data: Vec<OrderFull> = data
            .into_iter()
            .map(|order| {
                let items = items_map.remove(&order.id).unwrap_or_default();
                OrderFull {
                    order,
                    items,
                    warnings: Vec::new(),
                    delivery: None,
                }
            })
            .collect();
        return Ok(HttpResponse::Ok().json(PaginatedOrdersFull {
            data,
            total,
            page,
            per_page,
            total_pages,
            summary,
        }));
    }

    Ok(HttpResponse::Ok().json(PaginatedOrders {
        data,
        total,
        page,
        per_page,
        total_pages,
        summary,
    }))
}

// ── GET /orders/:id ───────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/orders/{order_id}",
    tag = "orders",
    params(("order_id" = Uuid, Path, description = "Order ID")),
    responses((status = 200, description = "Get order", body = OrderFull), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn get_order(
    req: HttpRequest,
    pool: crate::db::Db,
    order_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "orders", "read").await?;
    let order = fetch_order_or_404(pool.get_ref(), *order_id).await?;
    require_branch_access(pool.get_ref(), &claims, order.branch_id).await?;
    let items = fetch_order_items_full(pool.get_ref(), order.id).await?;
    let delivery = match order.delivery_order_id {
        Some(did) => fetch_order_delivery_info(pool.get_ref(), did).await?,
        None => None,
    };
    Ok(HttpResponse::Ok().json(OrderFull {
        order,
        items,
        warnings: Vec::new(),
        delivery,
    }))
}

// ── POST /orders/:id/void ─────────────────────────────────────

#[utoipa::path(
    post,
    path = "/orders/{order_id}/void",
    tag = "orders",
    params(("order_id" = Uuid, Path, description = "Order ID")),
    request_body = VoidOrderRequest,
    responses((status = 200, description = "Void order", body = Order), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn void_order(
    req: HttpRequest,
    pool: crate::db::Db,
    order_id: web::Path<Uuid>,
    body: web::Json<VoidOrderRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "orders", "update").await?;
    let order = fetch_order_or_404(pool.get_ref(), *order_id).await?;
    require_branch_access(pool.get_ref(), &claims, order.branch_id).await?;
    void_order_inner(
        pool.clone(),
        order_id.into_inner(),
        body,
        ActingContext::live(&claims)?,
    )
    .await
}

/// Void-order core. LIVE attributes `voided_by` to the JWT teller and blocks a
/// teller from voiding into a SETTLED (closed) shift; REPLAY attributes it to the
/// queued op's teller and skips that guard — a queued void was rung while the
/// shift was still open and is recorded history. Idempotent (guarded CAS).
pub(crate) async fn void_order_inner(
    pool: crate::db::Db,
    order_id: Uuid,
    body: web::Json<VoidOrderRequest>,
    actor: ActingContext,
) -> Result<HttpResponse, AppError> {
    let order = fetch_order_or_404(pool.get_ref(), order_id).await?;
    if order.status == "voided" {
        return Ok(HttpResponse::Ok().json(order));
    }

    // A teller may not rewrite the history of a SETTLED shift: once a shift is
    // closed / force-closed its cash is reconciled, so voiding one of its orders
    // is a correction that belongs to a manager. (The closing snapshot is frozen,
    // so a manager's void does not silently move a closed shift's recorded drawer
    // figure.) Replay bypasses this — the void happened while the shift was open.
    if !actor.replay && actor.role == UserRole::Teller {
        let shift_open: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM shifts WHERE id = $1 AND status = 'open')",
        )
        .bind(order.shift_id)
        .fetch_one(pool.get_ref())
        .await?;
        if !shift_open {
            return Err(AppError::Forbidden(
                "This order's shift is closed — ask a manager to void it.".into(),
            ));
        }
    }

    validate_void_reason(&body.reason)?;
    let void_note = body
        .note
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if body.reason == "other" && void_note.is_none() {
        return Err(AppError::BadRequest(
            "A note is required when void reason is 'other'".into(),
        ));
    }
    let voided_at = body.voided_at.unwrap_or_else(chrono::Utc::now);
    // Offline voids carry their real time; reject only a future device clock.
    crate::clock::reject_if_future(voided_at, "voided_at")?;

    // restock=true → items go back to stock; restock=false → they were made and
    // discarded (logged as waste). Either way the void touches the ledger, so
    // fetch the lines now.
    let restock = body.restore_inventory.unwrap_or(false);
    let items = fetch_order_items_full(pool.get_ref(), order_id).await?;

    let mut tx = pool.begin().await?;

    let updated = sqlx::query_as::<_, Order>(
        r#"UPDATE orders
           SET status      = 'voided',
               voided_at   = $3,
               void_reason = $2::void_reason,
               voided_by   = $4,
               void_note   = $5
           WHERE id = $1 AND status <> 'voided'
           RETURNING
               id, branch_id, shift_id, teller_id,
               (SELECT name FROM users WHERE id = teller_id) AS teller_name,
               waiter_id, (SELECT name FROM users WHERE id = waiter_id) AS waiter_name,
               order_number, order_ref, status::text, payment_method::text,
               COALESCE((SELECT json_agg(json_build_object('method', op.method, 'amount', op.amount) ORDER BY op.id)
                         FROM order_payments op WHERE op.order_id = orders.id), '[]'::json) AS payment_legs,
               subtotal, discount_type::text, discount_value,
               discount_amount, tax_amount, total_amount,
               amount_tendered, change_given, tip_amount, tip_payment_method,
               discount_id, customer_name, notes,
               order_type, delivery_fee, delivery_order_id,
               (SELECT channel::text FROM delivery_orders WHERE id = orders.delivery_order_id) AS delivery_channel,
               (SELECT customer_lat FROM delivery_orders WHERE id = orders.delivery_order_id) AS delivery_lat,
               (SELECT customer_lng FROM delivery_orders WHERE id = orders.delivery_order_id) AS delivery_lng,
               voided_at, void_reason::text, void_note, voided_by, created_at"#,
    )
    .bind(order_id)
    .bind(&body.reason)
    .bind(voided_at)
    .bind(actor.teller_id)
    .bind(void_note)
    .fetch_optional(&mut *tx)
    .await?;

    // A concurrent/retried void already won the race (UPDATE matched 0 rows
    // because status was already 'voided'): do NOT restock a second time —
    // return the already-voided order idempotently.
    let Some(updated) = updated else {
        tx.rollback().await?;
        let current = fetch_order_or_404(pool.get_ref(), order_id).await?;
        return Ok(HttpResponse::Ok().json(current));
    };

    // A void always REVERSES the original sale deduction (void_restock +). When
    // the food was NOT put back (restock=false) it was made and discarded, so we
    // then re-deduct it as WASTE (−). Net stock for a discard is unchanged, but
    // the ledger now reads "sale reversed → logged as waste" — self-describing and
    // consistent with how a delivery cancel logs a made-but-not-restocked order,
    // instead of leaving an orphan `sale` deduction on a voided order.
    for item in items {
        let Some(deductions) = item.item.deductions_snapshot.as_array() else {
            continue;
        };
        for d in deductions {
            let (Some(qty), Some(ing_id_str)) = (
                d.get("quantity").and_then(|v| v.as_f64()),
                d.get("org_ingredient_id").and_then(|v| v.as_str()),
            ) else {
                continue;
            };
            let Ok(ing_id) = Uuid::parse_str(ing_id_str) else {
                continue;
            };
            let unit_cost = d
                .get("cost_per_unit")
                .and_then(|v| v.as_f64())
                .map(|c| c.round() as i64);

            // Reverse the sale deduction (back into stock) through the ledger.
            crate::inventory::movements::record_movement(
                &mut *tx,
                crate::inventory::movements::MovementParams {
                    branch_id: order.branch_id,
                    org_ingredient_id: ing_id,
                    movement_type: "void_restock",
                    quantity: qty,
                    unit_cost,
                    reason: None,
                    source_type: Some("order"),
                    source_id: Some(order.id),
                    note: Some("Void restock"),
                    created_by: Some(actor.teller_id),
                },
            )
            .await?;

            if !restock {
                // Made & discarded → re-deduct, logged as waste.
                crate::inventory::movements::record_movement(
                    &mut *tx,
                    crate::inventory::movements::MovementParams {
                        branch_id: order.branch_id,
                        org_ingredient_id: ing_id,
                        movement_type: "waste",
                        quantity: -qty,
                        unit_cost,
                        reason: Some("order_cancelled"),
                        source_type: Some("order"),
                        source_id: Some(order.id),
                        note: Some("Order voided — made, not restocked"),
                        created_by: Some(actor.teller_id),
                    },
                )
                .await?;
            }
        }
    }

    tx.commit().await?;
    Ok(HttpResponse::Ok().json(updated))
}

// ── POST /orders/preview-recipe ───────────────────────────────

#[derive(Deserialize, Serialize, ToSchema)]
pub struct PreviewAddonInput {
    pub addon_item_id: Uuid,
    #[serde(default = "crate::orders::component_resolve::default_qty")]
    pub quantity: i32,
}

#[derive(Deserialize, Serialize, ToSchema)]
pub struct PreviewRecipeRequest {
    pub menu_item_id: Uuid,
    pub size_label: Option<String>,
    pub addons: Vec<PreviewAddonInput>,
    pub optional_field_ids: Vec<Uuid>,
}

#[derive(Serialize, Clone, ToSchema)]
pub struct PreviewIngredient {
    pub org_ingredient_id: Option<Uuid>,
    pub ingredient_name: String,
    pub unit: String,
    pub quantity: f64,
    pub source: String,
    pub category: String,
}

#[utoipa::path(
    post,
    path = "/orders/preview-recipe",
    tag = "orders",
    request_body = PreviewRecipeRequest,
    responses((status = 200, description = "Preview recipe", body = Vec<PreviewIngredient>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn preview_recipe(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<PreviewRecipeRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "orders", "create").await?;

    let mut result: Vec<PreviewIngredient> = Vec::new();

    // Base recipe
    let recipe_rows: Vec<(Option<Uuid>, f64, String, String, String)> = if let Some(size) =
        &body.size_label
    {
        sqlx::query_as(
                r#"SELECT r.org_ingredient_id, r.quantity_used::float8,
                          r.ingredient_name, r.ingredient_unit,
                          (SELECT ic.slug FROM ingredient_categories ic WHERE ic.id = i.category_id) as category
                   FROM   menu_item_recipes r
                   LEFT JOIN org_ingredients i ON i.id = r.org_ingredient_id
                   WHERE  r.menu_item_id = $1 AND r.size_label = $2"#,
            )
            .bind(body.menu_item_id)
            .bind(size)
            .fetch_all(pool.get_ref())
            .await?
    } else {
        sqlx::query_as(
                r#"SELECT r.org_ingredient_id, r.quantity_used::float8,
                          r.ingredient_name, r.ingredient_unit,
                          (SELECT ic.slug FROM ingredient_categories ic WHERE ic.id = i.category_id) as category
                   FROM   menu_item_recipes r
                   LEFT JOIN org_ingredients i ON i.id = r.org_ingredient_id
                   WHERE  r.menu_item_id = $1
                     AND  r.size_label = (
                         SELECT size_label FROM menu_item_recipes
                         WHERE  menu_item_id = $1 ORDER BY size_label LIMIT 1
                     )"#,
            )
            .bind(body.menu_item_id)
            .fetch_all(pool.get_ref())
            .await?
    };

    for (ing_id, qty, name, unit, category) in recipe_rows {
        result.push(PreviewIngredient {
            org_ingredient_id: ing_id,
            ingredient_name: name,
            unit,
            quantity: qty,
            source: "drink_recipe".into(),
            category,
        });
    }

    // Addons
    for addon in &body.addons {
        let addon_qty = addon.quantity.max(1) as f64;

        let (addon_name, addon_type): (String, String) =
            sqlx::query_as("SELECT name, type FROM addon_items WHERE id = $1")
                .bind(addon.addon_item_id)
                .fetch_optional(pool.get_ref())
                .await?
                .ok_or_else(|| {
                    AppError::NotFound(format!("Addon {} not found", addon.addon_item_id))
                })?;

        let rows: Vec<(Option<Uuid>, f64, String, String)> = sqlx::query_as(
            "SELECT org_ingredient_id, quantity_used::float8, ingredient_name, ingredient_unit
             FROM   addon_item_ingredients WHERE addon_item_id = $1",
        )
        .bind(addon.addon_item_id)
        .fetch_all(pool.get_ref())
        .await?;

        let target_category = match addon_type.as_str() {
            "milk_type" => Some("milk"),
            "coffee_type" => Some("coffee_bean"),
            _ => None,
        };

        if let Some(cat) = target_category {
            // Find the base recipe's ingredient for this category
            let base_ing_id = result
                .iter()
                .find(|r| r.source == "drink_recipe" && r.category == cat)
                .and_then(|r| r.org_ingredient_id);

            // Find the addon's ingredient
            let addon_ing_id = rows.first().and_then(|(id, _, _, _)| *id);

            // If both match → this IS the base, not a swap — skip
            let is_base =
                base_ing_id.is_some() && addon_ing_id.is_some() && base_ing_id == addon_ing_id;

            if !is_base && let Some((_, _, repl_name, repl_unit)) = rows.first() {
                for r in result.iter_mut() {
                    if r.source == "drink_recipe" && r.category == cat {
                        r.ingredient_name = repl_name.clone();
                        r.unit = repl_unit.clone();
                        r.source = format!("addon_swap:{}", addon_name);
                    }
                }
            }
            continue;
        }

        for (ing_id, qty, name, unit) in rows {
            result.push(PreviewIngredient {
                org_ingredient_id: ing_id,
                ingredient_name: name,
                unit,
                quantity: qty * addon_qty,
                source: "addon".into(),
                category: "general".into(),
            });
        }
    }

    // Optionals
    for &field_id in &body.optional_field_ids {
        // Explicit type annotation fixes the inference issue
        let row_result =
            sqlx::query_as::<_, (String, Option<f64>, Option<String>, Option<String>)>(
                "SELECT name, quantity_used::float8, ingredient_name, ingredient_unit
             FROM menu_item_optional_fields
             WHERE id = $1 AND menu_item_id = $2 AND is_active = true",
            )
            .bind(field_id)
            .bind(body.menu_item_id)
            .fetch_optional(pool.get_ref())
            .await?;

        if let Some((fname, Some(qty), Some(ing_name), Some(ing_unit))) = row_result {
            result.push(PreviewIngredient {
                org_ingredient_id: None,
                ingredient_name: ing_name,
                unit: ing_unit,
                quantity: qty,
                source: format!("optional:{}", fname),
                category: "general".into(),
            });
        }
    }
    Ok(HttpResponse::Ok().json(result))
}

// ── Helpers ───────────────────────────────────────────────────

fn extract_claims(req: &HttpRequest) -> Result<Claims, AppError> {
    req.extensions()
        .get::<Claims>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("Missing claims".into()))
}

async fn fetch_order_or_404(pool: &PgPool, order_id: Uuid) -> Result<Order, AppError> {
    let sql = format!("{} WHERE o.id = $1", ORDER_SELECT);
    sqlx::query_as::<_, Order>(&sql)
        .bind(order_id)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| AppError::NotFound("Order not found".into()))
}

/// Load the delivery context for a finalized delivery order's detail view.
/// Returns `None` if the linked `delivery_orders` row no longer exists.
async fn fetch_order_delivery_info(
    pool: &PgPool,
    delivery_order_id: Uuid,
) -> Result<Option<OrderDeliveryInfo>, AppError> {
    let info = sqlx::query_as::<_, OrderDeliveryInfo>(
        "SELECT d.channel::text AS channel, d.customer_phone, d.place_name, d.floor,
                d.unit_number, d.landmark, d.address_line, d.delivery_notes,
                d.road_distance_meters, z.name AS zone_name, d.delivery_ref,
                d.payment_method_hint
         FROM delivery_orders d
         LEFT JOIN delivery_zones z ON z.id = d.delivery_zone_id
         WHERE d.id = $1",
    )
    .bind(delivery_order_id)
    .fetch_optional(pool)
    .await?;
    Ok(info)
}

pub(crate) async fn fetch_order_by_idempotency_key(
    pool: &PgPool,
    key: Uuid,
    org_id: Uuid,
) -> Result<Option<Order>, AppError> {
    // Org-scope the lookup: an idempotency key is only unique within an org, and
    // an unscoped match would return (and echo back) another org's full order to
    // a caller that merely guessed/collided on the key. Scope by the actor's org
    // via the order's branch so the early-return can only ever surface our own.
    let sql = format!(
        "{} WHERE o.idempotency_key = $1 \
           AND o.branch_id IN (SELECT id FROM branches WHERE org_id = $2)",
        ORDER_SELECT
    );
    Ok(sqlx::query_as::<_, Order>(&sql)
        .bind(key)
        .bind(org_id)
        .fetch_optional(pool)
        .await?)
}

/// Like [`fetch_order_by_idempotency_key`] but keyed on the CLIENT-minted
/// `order_ref` (also globally unique, also org-scoped for the same safety reason).
/// Used to make a replayed order whose `order_ref` collides — but whose idempotency
/// key does NOT match — idempotent (return the existing order) instead of a 409 the
/// offline client would dead-letter into a lost sale.
pub(crate) async fn fetch_order_by_order_ref(
    pool: &PgPool,
    order_ref: &str,
    org_id: Uuid,
) -> Result<Option<Order>, AppError> {
    let sql = format!(
        "{} WHERE o.order_ref = $1 \
           AND o.branch_id IN (SELECT id FROM branches WHERE org_id = $2)",
        ORDER_SELECT
    );
    Ok(sqlx::query_as::<_, Order>(&sql)
        .bind(order_ref)
        .bind(org_id)
        .fetch_optional(pool)
        .await?)
}

async fn fetch_order_items_full(
    pool: &PgPool,
    order_id: Uuid,
) -> Result<Vec<OrderItemFull>, AppError> {
    let items = sqlx::query_as::<_, OrderItem>(
        "SELECT id, order_id, menu_item_id, item_name, name_translations, size_label, \
                unit_price, quantity, line_total, notes, deductions_snapshot, \
                bundle_id, bundle_unit_price, line_cost, unit_cost, cost_missing \
         FROM order_items WHERE order_id = $1 ORDER BY id",
    )
    .bind(order_id)
    .fetch_all(pool)
    .await?;

    let mut result = Vec::new();
    for item in items {
        let addons = sqlx::query_as::<_, OrderItemAddon>(
            "SELECT id, order_item_id, addon_item_id, addon_name, name_translations, \
                    unit_price, quantity, line_total, line_cost \
             FROM order_item_addons WHERE order_item_id = $1 ORDER BY id",
        )
        .bind(item.id)
        .fetch_all(pool)
        .await?;

        let optionals = sqlx::query_as::<_, OrderItemOptional>(
            "SELECT id, order_item_id, optional_field_id, field_name, name_translations, price, \
                    org_ingredient_id, ingredient_name, ingredient_unit, quantity_deducted, cost \
             FROM order_item_optionals WHERE order_item_id = $1 ORDER BY id",
        )
        .bind(item.id)
        .fetch_all(pool)
        .await?;

        let bundle_components = if item.bundle_id.is_some() {
            let comps: Vec<(Uuid, i32, Option<String>, serde_json::Value)> = sqlx::query_as(
                "SELECT item_id, quantity, size_label, name_translations \
                 FROM order_line_bundle_components WHERE order_line_id = $1",
            )
            .bind(item.id)
            .fetch_all(pool)
            .await?;

            let mut out = Vec::new();
            for (comp_item_id, qty, size_label, name_translations) in comps {
                let item_name: String =
                    sqlx::query_scalar("SELECT name FROM menu_items WHERE id = $1")
                        .bind(comp_item_id)
                        .fetch_one(pool)
                        .await?;

                let comp_addons = sqlx::query_as::<_, OrderBundleComponentAddon>(
                    "SELECT id, order_line_id, component_item_id, addon_item_id, addon_name, name_translations, \
                            unit_price, quantity, line_total \
                     FROM order_line_bundle_component_addons \
                     WHERE order_line_id = $1 AND component_item_id = $2 \
                     ORDER BY id",
                )
                .bind(item.id)
                .bind(comp_item_id)
                .fetch_all(pool)
                .await?;

                let comp_optionals = sqlx::query_as::<_, OrderBundleComponentOptional>(
                    "SELECT id, order_line_id, component_item_id, optional_field_id, field_name, name_translations, price \
                     FROM order_line_bundle_component_optionals \
                     WHERE order_line_id = $1 AND component_item_id = $2 \
                     ORDER BY id",
                )
                .bind(item.id)
                .bind(comp_item_id)
                .fetch_all(pool)
                .await?;

                out.push(OrderBundleComponentFull {
                    item_id: comp_item_id,
                    item_name,
                    name_translations,
                    quantity: qty,
                    size_label,
                    addons: comp_addons,
                    optionals: comp_optionals,
                });
            }
            out
        } else {
            vec![]
        };

        result.push(OrderItemFull {
            item,
            addons,
            optionals,
            bundle_components,
        });
    }
    Ok(result)
}

/// Batched variant of [fetch_order_items_full] for `include_items=true` list
/// responses: one query per table (ANY($1)) instead of N+1 per order.
async fn fetch_orders_items_full_batch(
    pool: &PgPool,
    order_ids: &[Uuid],
) -> Result<std::collections::HashMap<Uuid, Vec<OrderItemFull>>, AppError> {
    use std::collections::HashMap;

    let mut by_order: HashMap<Uuid, Vec<OrderItemFull>> = HashMap::new();
    if order_ids.is_empty() {
        return Ok(by_order);
    }

    let items = sqlx::query_as::<_, OrderItem>(
        "SELECT id, order_id, menu_item_id, item_name, name_translations, size_label, \
                unit_price, quantity, line_total, notes, deductions_snapshot, \
                bundle_id, bundle_unit_price, line_cost, unit_cost, cost_missing \
         FROM order_items WHERE order_id = ANY($1) ORDER BY id",
    )
    .bind(order_ids)
    .fetch_all(pool)
    .await?;

    let item_ids: Vec<Uuid> = items.iter().map(|i| i.id).collect();

    let mut addons_by_item: HashMap<Uuid, Vec<OrderItemAddon>> = HashMap::new();
    for a in sqlx::query_as::<_, OrderItemAddon>(
        "SELECT id, order_item_id, addon_item_id, addon_name, name_translations, \
                unit_price, quantity, line_total, line_cost \
         FROM order_item_addons WHERE order_item_id = ANY($1) ORDER BY id",
    )
    .bind(&item_ids)
    .fetch_all(pool)
    .await?
    {
        addons_by_item.entry(a.order_item_id).or_default().push(a);
    }

    let mut optionals_by_item: HashMap<Uuid, Vec<OrderItemOptional>> = HashMap::new();
    for o in sqlx::query_as::<_, OrderItemOptional>(
        "SELECT id, order_item_id, optional_field_id, field_name, name_translations, price, \
                org_ingredient_id, ingredient_name, ingredient_unit, quantity_deducted, cost \
         FROM order_item_optionals WHERE order_item_id = ANY($1) ORDER BY id",
    )
    .bind(&item_ids)
    .fetch_all(pool)
    .await?
    {
        optionals_by_item
            .entry(o.order_item_id)
            .or_default()
            .push(o);
    }

    // ── Bundle components (only for bundle lines) ────────────────────────────
    let bundle_line_ids: Vec<Uuid> = items
        .iter()
        .filter(|i| i.bundle_id.is_some())
        .map(|i| i.id)
        .collect();

    let mut comps_by_line: HashMap<Uuid, Vec<OrderBundleComponentFull>> = HashMap::new();
    if !bundle_line_ids.is_empty() {
        let comp_rows: Vec<(Uuid, Uuid, i32, Option<String>, serde_json::Value)> = sqlx::query_as(
            "SELECT order_line_id, item_id, quantity, size_label, name_translations \
                 FROM order_line_bundle_components WHERE order_line_id = ANY($1)",
        )
        .bind(&bundle_line_ids)
        .fetch_all(pool)
        .await?;

        let comp_item_ids: Vec<Uuid> = comp_rows.iter().map(|r| r.1).collect();
        let mut item_names: HashMap<Uuid, String> = HashMap::new();
        if !comp_item_ids.is_empty() {
            let name_rows: Vec<(Uuid, String)> =
                sqlx::query_as("SELECT id, name FROM menu_items WHERE id = ANY($1)")
                    .bind(&comp_item_ids)
                    .fetch_all(pool)
                    .await?;
            item_names.extend(name_rows);
        }

        let mut comp_addons: HashMap<(Uuid, Uuid), Vec<OrderBundleComponentAddon>> = HashMap::new();
        for a in sqlx::query_as::<_, OrderBundleComponentAddon>(
            "SELECT id, order_line_id, component_item_id, addon_item_id, addon_name, name_translations, \
                    unit_price, quantity, line_total \
             FROM order_line_bundle_component_addons \
             WHERE order_line_id = ANY($1) ORDER BY id",
        )
        .bind(&bundle_line_ids)
        .fetch_all(pool)
        .await?
        {
            comp_addons
                .entry((a.order_line_id, a.component_item_id))
                .or_default()
                .push(a);
        }

        let mut comp_optionals: HashMap<(Uuid, Uuid), Vec<OrderBundleComponentOptional>> =
            HashMap::new();
        for o in sqlx::query_as::<_, OrderBundleComponentOptional>(
            "SELECT id, order_line_id, component_item_id, optional_field_id, field_name, name_translations, price \
             FROM order_line_bundle_component_optionals \
             WHERE order_line_id = ANY($1) ORDER BY id",
        )
        .bind(&bundle_line_ids)
        .fetch_all(pool)
        .await?
        {
            comp_optionals
                .entry((o.order_line_id, o.component_item_id))
                .or_default()
                .push(o);
        }

        for (line_id, comp_item_id, qty, size_label, name_translations) in comp_rows {
            comps_by_line
                .entry(line_id)
                .or_default()
                .push(OrderBundleComponentFull {
                    item_id: comp_item_id,
                    item_name: item_names.get(&comp_item_id).cloned().unwrap_or_default(),
                    name_translations,
                    quantity: qty,
                    size_label,
                    addons: comp_addons
                        .remove(&(line_id, comp_item_id))
                        .unwrap_or_default(),
                    optionals: comp_optionals
                        .remove(&(line_id, comp_item_id))
                        .unwrap_or_default(),
                });
        }
    }

    for item in items {
        let full = OrderItemFull {
            addons: addons_by_item.remove(&item.id).unwrap_or_default(),
            optionals: optionals_by_item.remove(&item.id).unwrap_or_default(),
            bundle_components: comps_by_line.remove(&item.id).unwrap_or_default(),
            item,
        };
        by_order.entry(full.item.order_id).or_default().push(full);
    }

    Ok(by_order)
}

async fn require_branch_access(
    pool: &PgPool,
    claims: &Claims,
    branch_id: Uuid,
) -> Result<(), AppError> {
    if claims.role == UserRole::SuperAdmin {
        return Ok(());
    }

    let branch_org: Option<Uuid> =
        sqlx::query_scalar("SELECT org_id FROM branches WHERE id = $1 AND deleted_at IS NULL")
            .bind(branch_id)
            .fetch_optional(pool)
            .await?
            .flatten();

    let branch_org = branch_org.ok_or_else(|| AppError::NotFound("Branch not found".into()))?;

    if claims.org_id() != Some(branch_org) {
        return Err(AppError::Forbidden(
            "Branch belongs to a different org".into(),
        ));
    }
    if claims.role == UserRole::OrgAdmin {
        return Ok(());
    }

    // D13: tellers are ORG-scoped, not branch-scoped — any active teller in the
    // branch's org may ring up here (the org check above is the boundary). The
    // order still records this device's branch, so revenue stays attributed
    // correctly.
    if claims.role == UserRole::Teller {
        return Ok(());
    }

    // Branch managers stay branch-scoped via their explicit assignments.
    let assigned: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM user_branch_assignments \
         WHERE user_id = $1 AND branch_id = $2)",
    )
    .bind(claims.user_id())
    .bind(branch_id)
    .fetch_one(pool)
    .await?;

    if !assigned {
        return Err(AppError::Forbidden("Not assigned to this branch".into()));
    }

    Ok(())
}

async fn validate_payment_method(
    pool: &PgPool,
    org_id: Uuid,
    method: &str,
) -> Result<(), AppError> {
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM org_payment_methods WHERE org_id = $1 AND name = $2 AND is_active = true)"
    )
    .bind(org_id)
    .bind(method)
    .fetch_one(pool)
    .await?;

    if exists {
        Ok(())
    } else {
        Err(AppError::BadRequest(format!(
            "Invalid or inactive payment_method: {}",
            method
        )))
    }
}

async fn parse_payment_methods(
    pool: &PgPool,
    org_id: Uuid,
    raw: &str,
) -> Result<Vec<String>, AppError> {
    let mut methods = Vec::new();
    for part in raw.split(',') {
        let trimmed = part.trim();
        if !trimmed.is_empty() {
            validate_payment_method(pool, org_id, trimmed).await?;
            methods.push(trimmed.to_string());
        }
    }
    Ok(methods)
}

#[allow(dead_code)]
async fn fetch_order_payments(
    pool: &PgPool,
    order_id: Uuid,
) -> Result<Vec<OrderPayment>, AppError> {
    let rows = sqlx::query_as::<_, OrderPayment>(
        "SELECT id, order_id, method::text AS method, amount, reference \
         FROM order_payments WHERE order_id = $1 ORDER BY id",
    )
    .bind(order_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

fn validate_discount_type(dt: &str) -> Result<(), AppError> {
    match dt {
        "percentage" | "fixed" => Ok(()),
        _ => Err(AppError::BadRequest(
            "discount_type must be 'percentage' or 'fixed'".into(),
        )),
    }
}

fn validate_void_reason(reason: &str) -> Result<(), AppError> {
    match reason {
        "customer_request" | "wrong_order" | "quality_issue" | "other" => Ok(()),
        _ => Err(AppError::BadRequest("Invalid void_reason".into())),
    }
}

#[allow(unused_assignments)]
#[utoipa::path(
    get,
    path = "/orders/export",
    tag = "orders",
    params(ExportOrdersQuery),
    responses((status = 200, description = "Export orders", body = ExportResponse), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn export_orders(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<ExportOrdersQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "orders", "read").await?;

    let org_id = claims
        .org_id()
        .ok_or_else(|| AppError::Forbidden("No org in token".into()))?;

    // Same scope rule as list_orders: shift, single branch, or every branch in
    // the org when no shift is given and branch_id is absent or the nil UUID.
    let all_branches = query.shift_id.is_none() && query.branch_id.map_or(true, |b| b.is_nil());

    let (scope_condition, scope_id): (&str, Uuid) = if let Some(shift_id) = query.shift_id {
        let bid: Option<Uuid> = sqlx::query_scalar("SELECT branch_id FROM shifts WHERE id = $1")
            .bind(shift_id)
            .fetch_optional(pool.get_ref())
            .await?
            .flatten();
        let bid = bid.ok_or_else(|| AppError::NotFound("Shift not found".into()))?;
        require_branch_access(pool.get_ref(), &claims, bid).await?;
        ("o.shift_id = $1", shift_id)
    } else if all_branches {
        (
            "o.branch_id IN (SELECT id FROM branches WHERE org_id = $1 AND deleted_at IS NULL)",
            org_id,
        )
    } else {
        let bid = query
            .branch_id
            .expect("branch_id present when not all_branches");
        require_branch_access(pool.get_ref(), &claims, bid).await?;
        ("o.branch_id = $1", bid)
    };

    let parsed_payment_methods = match &query.payment_method {
        Some(pm) => {
            let methods = parse_payment_methods(pool.get_ref(), org_id, pm).await?;
            if methods.is_empty() {
                None
            } else {
                Some(methods)
            }
        }
        None => None,
    };

    let mut filter = String::new();
    #[allow(unused_assignments)]
    let mut idx = 2i32;

    macro_rules! push_export_filter {
        ($col:expr, $opt:expr) => {
            if $opt.is_some() {
                filter.push_str(&format!(" AND {} ${}", $col, idx));
                idx += 1;
            }
        };
    }

    push_export_filter!("u.name ILIKE", query.teller_name);
    push_export_filter!("w.name ILIKE", query.waiter_name);
    if parsed_payment_methods.is_some() {
        filter.push_str(&payment_method_filter(idx));
        idx += 1;
    }
    push_export_filter!("o.status::text =", query.status);
    push_export_filter!("o.created_at >=", query.from);
    push_export_filter!("o.created_at <=", query.to);

    macro_rules! bind_export_filters {
        ($q:expr) => {{
            let mut q = $q;
            if let Some(ref v) = query.teller_name {
                q = q.bind(format!("%{}%", v));
            }
            if let Some(ref v) = query.waiter_name {
                q = q.bind(format!("%{}%", v));
            }
            if let Some(v) = &parsed_payment_methods {
                q = q.bind(v);
            }
            if let Some(ref v) = query.status {
                q = q.bind(v.clone());
            }
            if let Some(v) = query.from {
                q = q.bind(v);
            }
            if let Some(v) = query.to {
                q = q.bind(v);
            }
            q
        }};
    }

    let count_sql = format!(
        "SELECT COUNT(*) FROM orders o JOIN users u ON u.id = o.teller_id \
         LEFT JOIN users w ON w.id = o.waiter_id WHERE {} {}",
        scope_condition, filter
    );

    let total: i64 = bind_export_filters!(sqlx::query_scalar(&count_sql).bind(scope_id))
        .fetch_one(pool.get_ref())
        .await?;

    if total > 50_000 {
        return Err(AppError::BadRequest(format!(
            "Export too large: {} orders match. Narrow the date range or add filters (limit: 50000).",
            total
        )));
    }

    let summary_sql = format!(
        "SELECT {} \
         FROM orders o JOIN users u ON u.id = o.teller_id \
         LEFT JOIN users w ON w.id = o.waiter_id \
         LEFT JOIN delivery_orders d ON d.id = o.delivery_order_id \
         WHERE {} {}",
        order_summary_cols(None),
        scope_condition,
        filter
    );

    let summary: OrderSummary =
        bind_export_filters!(sqlx::query_as::<_, OrderSummary>(&summary_sql).bind(scope_id))
            .fetch_one(pool.get_ref())
            .await?;

    let data_sql = format!(
        "{} WHERE {} {} ORDER BY o.created_at DESC",
        ORDER_SELECT, scope_condition, filter
    );

    let orders: Vec<Order> =
        bind_export_filters!(sqlx::query_as::<_, Order>(&data_sql).bind(scope_id))
            .fetch_all(pool.get_ref())
            .await?;

    let order_ids: Vec<Uuid> = orders.iter().map(|o| o.id).collect();
    let payments_rows = if order_ids.is_empty() {
        Vec::new()
    } else {
        sqlx::query_as::<_, OrderPayment>(
            "SELECT id, order_id, method::text AS method, amount, reference \
             FROM order_payments WHERE order_id = ANY($1) ORDER BY id",
        )
        .bind(&order_ids)
        .fetch_all(pool.get_ref())
        .await?
    };

    use std::collections::HashMap;
    let mut payments_by_order: HashMap<Uuid, Vec<OrderPayment>> = HashMap::new();
    for p in payments_rows {
        payments_by_order.entry(p.order_id).or_default().push(p);
    }

    let mut data = Vec::with_capacity(orders.len());
    for order in orders {
        let order_id = order.id;
        let items = fetch_order_items_full(pool.get_ref(), order_id).await?;
        let payments = payments_by_order.remove(&order_id).unwrap_or_default();
        data.push(OrderExport {
            order,
            items,
            payments,
        });
    }

    use std::collections::HashSet;

    // Collect every distinct org_ingredient_id from all deduction snapshots
    let ingredient_ids: Vec<Uuid> = data
        .iter()
        .flat_map(|o| o.items.iter())
        .flat_map(|i| {
            i.item
                .deductions_snapshot
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|d| {
                    d.get("org_ingredient_id")
                        .and_then(|v| v.as_str())
                        .and_then(|s| Uuid::parse_str(s).ok())
                })
        })
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();

    let ingredient_costs: HashMap<Uuid, i32> = if ingredient_ids.is_empty() {
        HashMap::new()
    } else {
        let decimal_costs: Vec<(Uuid, Option<Decimal>)> =
            sqlx::query_as::<_, (Uuid, Option<Decimal>)>(
                "SELECT id, cost_per_unit FROM org_ingredients WHERE id = ANY($1)",
            )
            .bind(&ingredient_ids)
            .fetch_all(pool.get_ref())
            .await?;

        decimal_costs
            .into_iter()
            // NULL cost ⟺ never entered (unknown, NOT free). Omit those rows
            // so the frontend can distinguish "unknown" from a genuine 0 —
            // mirrors the catalog's Option<Decimal> contract.
            .filter_map(|(id, cost)| {
                // cost_per_unit is stored in piastres; just round to integer.
                cost.map(|c| (id, c.round().to_i32().unwrap_or(0)))
            })
            .collect()
    };

    Ok(HttpResponse::Ok().json(ExportResponse {
        data,
        total,
        generated_at: chrono::Utc::now(),
        summary,
        ingredient_costs,
    }))
}

#[cfg(test)]
mod wire_tests {
    use super::*;

    /// Regression: `quantity_deducted` must serialize as a JSON NUMBER (not the
    /// bigdecimal default string), so the generated POS client can decode the
    /// create-order response and ack the queued sale. (A string here was
    /// dead-lettering every order that had an ingredient-deducting optional.)
    #[test]
    fn quantity_deducted_serializes_as_number_not_string() {
        let opt = OrderItemOptional {
            id: Uuid::nil(),
            order_item_id: Uuid::nil(),
            optional_field_id: None,
            field_name: "Extra shot".into(),
            name_translations: serde_json::json!({}),
            price: 0,
            org_ingredient_id: None,
            ingredient_name: None,
            ingredient_unit: None,
            quantity_deducted: Some("0.5".parse().unwrap()),
            cost: None,
        };
        let v = serde_json::to_value(&opt).unwrap();
        assert!(
            v["quantity_deducted"].is_number(),
            "must be a JSON number, got {}",
            v["quantity_deducted"]
        );
        assert_eq!(v["quantity_deducted"].as_f64(), Some(0.5));

        // None serializes as null, never a string.
        let opt_none = OrderItemOptional {
            quantity_deducted: None,
            ..opt
        };
        let vn = serde_json::to_value(&opt_none).unwrap();
        assert!(vn["quantity_deducted"].is_null());
    }
}
