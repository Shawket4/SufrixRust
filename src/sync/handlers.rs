use actix_web::{HttpMessage, HttpRequest, HttpResponse, web};
use serde::Deserialize;
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::jwt::Claims;
use crate::errors::AppError;
use crate::models::UserRole;
use crate::realtime::hub::BranchEventHub;
use crate::sync::ActingContext;

use crate::floor_ops::handlers::{
    CreateFloorTransferRequest, FulfillTransferRequest, SwapTablesRequest,
};
use crate::orders::handlers::{CreateOrderRequest, VoidOrderRequest};
use crate::shifts::handlers::{CashMovementRequest, CloseShiftRequest, OpenShiftRequest};
use crate::tickets::handlers::{
    AddRoundRequest, CreateOpenTicketRequest, SettleOpenTicketRequest, VoidOpenTicketRequest,
};

/// One queued op from a device, carrying its ORIGINAL actor (`teller_id` — a
/// teller or a waiter) so the replay attributes the write to whoever rang it —
/// not to whoever is signed in when the backlog flushes. The `request` payloads
/// are the SAME bodies the live routes accept (idempotency keys ride inside
/// them), so a replayed op dedups server-side exactly like a lost-response retry
/// on the live endpoint.
#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ReplayOp {
    OpenShift {
        teller_id: Uuid,
        branch_id: Uuid,
        request: OpenShiftRequest,
    },
    CloseShift {
        teller_id: Uuid,
        shift_id: Uuid,
        request: CloseShiftRequest,
    },
    CreateOrder {
        teller_id: Uuid,
        request: CreateOrderRequest,
    },
    VoidOrder {
        teller_id: Uuid,
        order_id: Uuid,
        request: VoidOrderRequest,
    },
    // Adding a sale's loyalty points — an explicit teller action, queued when
    // the till was offline. The request carries `requested_at` (the moment the
    // button was pressed), so a drain days later still credits an award made in
    // time; the server bounds that claim against the order's own timestamp, so
    // a queued op can never reach outside the 24-hour window.
    AwardLoyaltyPoints {
        teller_id: Uuid,
        request: crate::loyalty::award::AwardRequest,
    },
    CashMovement {
        teller_id: Uuid,
        shift_id: Uuid,
        request: CashMovementRequest,
    },
    // Waiter open-ticket ops (fire + rounds are fired by a WAITER; the cashier
    // settle is a TELLER; either may void). `teller_id` carries the acting user.
    FireOpenTicket {
        teller_id: Uuid,
        request: CreateOpenTicketRequest,
    },
    AddTicketRound {
        teller_id: Uuid,
        ticket_id: Uuid,
        request: AddRoundRequest,
    },
    SettleOpenTicket {
        teller_id: Uuid,
        ticket_id: Uuid,
        request: SettleOpenTicketRequest,
    },
    VoidOpenTicket {
        teller_id: Uuid,
        ticket_id: Uuid,
        request: VoidOpenTicketRequest,
    },
    // KDS bump/unbump (kitchen device, or a teller on the till queue). `item_id`
    // is the kitchen line; it doubles as the idempotency key (re-bumping a bumped
    // line is a no-op, and a bump for a gone line replays as a clean no-op).
    BumpKitchenItem {
        teller_id: Uuid,
        item_id: Uuid,
    },
    UnbumpKitchenItem {
        teller_id: Uuid,
        item_id: Uuid,
    },
    // Held-order (teller parked-cart) ops. All idempotent on the CLIENT-minted
    // held-order id; a park that loses a table race applies WITHOUT the table
    // (never dead-letters — see held_orders::handlers).
    // Floor ops shared by tellers (held orders) and waiters (their tickets).
    // Per-occupant permissions are enforced inside the cores.
    SwapTables {
        teller_id: Uuid,
        request: SwapTablesRequest,
    },
    CreateTableTransfer {
        teller_id: Uuid,
        request: CreateFloorTransferRequest,
    },
    CancelTableTransfer {
        teller_id: Uuid,
        transfer_id: Uuid,
    },
    FulfillTableTransfer {
        teller_id: Uuid,
        transfer_id: Uuid,
        request: FulfillTransferRequest,
    },
    /// "The plates are gone" — the one table transition no server can observe.
    /// The POS has always queued this with an empty `request` (no branch), so
    /// the core reads the branch off the table; `request` is accepted and
    /// ignored purely so a body that IS sent doesn't fail the envelope.
    ClearTable {
        teller_id: Uuid,
        table_id: Uuid,
        #[serde(default)]
        request: serde_json::Value,
    },
    // Bookings at service time: a waiter/teller seats a booked party or marks
    // a no-show while the cloud is unreachable. Both idempotent on status.
    SeatBooking {
        teller_id: Uuid,
        booking_id: Uuid,
        #[serde(default)]
        request: crate::bookings::handlers::SeatBookingRequest,
    },
    NoShowBooking {
        teller_id: Uuid,
        booking_id: Uuid,
    },
}

impl ReplayOp {
    fn teller_id(&self) -> Uuid {
        match self {
            ReplayOp::OpenShift { teller_id, .. }
            | ReplayOp::CloseShift { teller_id, .. }
            | ReplayOp::CreateOrder { teller_id, .. }
            | ReplayOp::VoidOrder { teller_id, .. }
            | ReplayOp::CashMovement { teller_id, .. }
            | ReplayOp::FireOpenTicket { teller_id, .. }
            | ReplayOp::AddTicketRound { teller_id, .. }
            | ReplayOp::SettleOpenTicket { teller_id, .. }
            | ReplayOp::VoidOpenTicket { teller_id, .. }
            | ReplayOp::BumpKitchenItem { teller_id, .. }
            | ReplayOp::UnbumpKitchenItem { teller_id, .. }
            | ReplayOp::SwapTables { teller_id, .. }
            | ReplayOp::CreateTableTransfer { teller_id, .. }
            | ReplayOp::CancelTableTransfer { teller_id, .. }
            | ReplayOp::FulfillTableTransfer { teller_id, .. }
            | ReplayOp::ClearTable { teller_id, .. }
            | ReplayOp::SeatBooking { teller_id, .. }
            | ReplayOp::NoShowBooking { teller_id, .. }
            | ReplayOp::AwardLoyaltyPoints { teller_id, .. } => *teller_id,
        }
    }

    /// Whether the embedded actor's actual role may have produced this op. Cash /
    /// order / shift ops are TELLER-only (unchanged); a waiter fires tickets and
    /// rounds; a teller (cashier) settles; either a waiter or teller may void.
    /// This is the attribution-safety boundary — a write can never be replayed
    /// under a role that couldn't have performed it live.
    fn actor_role_allowed(&self, role: &UserRole) -> bool {
        use UserRole::{Kitchen, Teller, Waiter};
        match self {
            ReplayOp::OpenShift { .. }
            | ReplayOp::CloseShift { .. }
            | ReplayOp::CreateOrder { .. }
            | ReplayOp::VoidOrder { .. }
            | ReplayOp::CashMovement { .. }
            | ReplayOp::SettleOpenTicket { .. }
            // The award button lives on the teller's receipt and history; a
            // waiter never sees it, so a queued award attributed to one could
            // not have been made live.
            | ReplayOp::AwardLoyaltyPoints { .. } => *role == Teller,
            ReplayOp::FireOpenTicket { .. } | ReplayOp::AddTicketRound { .. } => *role == Waiter,
            ReplayOp::VoidOpenTicket { .. } => matches!(role, Teller | Waiter),
            // A kitchen device bumps; a teller may bump the till queue too.
            ReplayOp::BumpKitchenItem { .. } | ReplayOp::UnbumpKitchenItem { .. } => {
                matches!(role, Kitchen | Teller)
            }
            // Floor ops: a teller works the whole floor; a waiter moves/queues
            // their own tickets (per-occupant permissions gate inside the core).
            ReplayOp::SwapTables { .. }
            | ReplayOp::CreateTableTransfer { .. }
            | ReplayOp::CancelTableTransfer { .. }
            | ReplayOp::FulfillTableTransfer { .. }
            | ReplayOp::ClearTable { .. } => matches!(role, Teller | Waiter),
            ReplayOp::SeatBooking { .. } | ReplayOp::NoShowBooking { .. } => {
                matches!(role, Teller | Waiter)
            }
        }
    }

    /// The `(resource, action)` permission(s) the LIVE endpoint enforces for this op.
    /// Replay must check the SAME ones against the op's actor, so a per-user override
    /// (e.g. a teller whose `void` was revoked) can't be bypassed by queueing the
    /// write offline. Kept in lock-step with the per-endpoint `check_permission`
    /// calls (open=shifts/create, close+cash=shifts/update, order create/void=orders
    /// create/update, ticket fire=create / round+settle+void=update, settle also
    /// orders/create, bump=kitchen_orders/update).
    fn required_permissions(&self) -> &'static [(&'static str, &'static str)] {
        match self {
            ReplayOp::OpenShift { .. } => &[("shifts", "create")],
            ReplayOp::CloseShift { .. } => &[("shifts", "update")],
            ReplayOp::CashMovement { .. } => &[("shifts", "update")],
            ReplayOp::CreateOrder { .. } => &[("orders", "create")],
            ReplayOp::VoidOrder { .. } => &[("orders", "update")],
            ReplayOp::FireOpenTicket { .. } => &[("open_tickets", "create")],
            ReplayOp::AddTicketRound { .. } => &[("open_tickets", "update")],
            ReplayOp::SettleOpenTicket { .. } => {
                &[("open_tickets", "update"), ("orders", "create")]
            }
            ReplayOp::VoidOpenTicket { .. } => &[("open_tickets", "update")],
            ReplayOp::BumpKitchenItem { .. } | ReplayOp::UnbumpKitchenItem { .. } => {
                &[("kitchen_orders", "update")]
            }
            // Swap checks the moved tickets inside the core, and so does clear.
            ReplayOp::SwapTables { .. } | ReplayOp::ClearTable { .. } => &[],
            ReplayOp::CreateTableTransfer { .. } => &[("table_transfers", "create")],
            ReplayOp::CancelTableTransfer { .. } => &[("table_transfers", "update")],
            // Fulfill also checks the occupant's kind inside the core.
            ReplayOp::FulfillTableTransfer { .. } => &[("table_transfers", "update")],
            ReplayOp::SeatBooking { .. } | ReplayOp::NoShowBooking { .. } => {
                &[("bookings", "update")]
            }
            // Lock-step with `loyalty::award::award`'s own check, so a teller
            // whose loyalty grant was revoked cannot get an award through by
            // having queued it offline.
            ReplayOp::AwardLoyaltyPoints { .. } => &[("loyalty", "update")],
        }
    }
}

fn extract_claims(req: &HttpRequest) -> Result<Claims, AppError> {
    req.extensions()
        .get::<Claims>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("Missing authentication".into()))
}

/// POST /sync/replay — flush ONE queued op, attributed to its embedded teller.
///
/// Authorization: the bearer must be a member of an org, and the op's embedded
/// teller must be an ACTIVE TELLER OF THAT SAME ORG. So any teller (or, later, a
/// device principal) may flush the whole device backlog — A's ops and B's ops —
/// each landing under its true author. The op's target (branch / shift / order)
/// must also belong to the bearer's org, so a token can never replay across orgs.
///
/// One op per call keeps the proven client-side drain engine (FIFO, dependency
/// gating, backoff, idempotency, close-last) intact — the client just points each
/// op at this route instead of the per-resource live one.
pub async fn replay(
    req: HttpRequest,
    pool: crate::db::Db,
    hub: web::Data<BranchEventHub>,
    body: web::Json<ReplayOp>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    let token_org = claims
        .org_id()
        .ok_or_else(|| AppError::Unauthorized("Token has no organization".into()))?;

    let op = body.into_inner();
    let teller_id = op.teller_id();

    // The embedded actor must be an active PIN-login user (teller, waiter, or
    // kitchen) of the bearer's org, and its role must be one that could have
    // produced THIS op live. This is the attribution-safety boundary: a write can
    // never be replayed under an actor from a different org, nor attributed to a
    // role that couldn't perform it (the per-op `actor_role_allowed` enforces the
    // latter — e.g. only a kitchen/teller actor may replay a bump).
    let row: Option<(Option<Uuid>, bool, UserRole)> = sqlx::query_as(
        "SELECT org_id, is_active, role FROM users WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(teller_id)
    .fetch_optional(pool.get_ref())
    .await?;
    let (actor_org, is_active, actor_role) = match row {
        Some((Some(org), active, role)) => (org, active, role),
        _ => {
            return Err(AppError::Forbidden(
                "Replay actor is not a member of this organization".into(),
            ));
        }
    };
    if actor_org != token_org
        || !is_active
        || !matches!(
            actor_role,
            UserRole::Teller | UserRole::Waiter | UserRole::Kitchen
        )
        || !op.actor_role_allowed(&actor_role)
    {
        return Err(AppError::Forbidden(
            "Replay actor may not perform this operation for this organization".into(),
        ));
    }

    // Enforce the SAME per-user permissions the LIVE endpoint checks, against the
    // ACTOR (the op's embedded author) — NOT just the coarse role above. Without
    // this, a teller whose `void` (or any action) was revoked by a per-user override
    // could still perform it by queueing it offline and letting it replay.
    for &(resource, action) in op.required_permissions() {
        crate::permissions::checker::check_permission_for(
            pool.get_ref(),
            teller_id,
            &actor_role,
            resource,
            action,
        )
        .await?;
    }

    // The target must belong to the bearer's org — block any cross-org replay.
    op_branch_must_be_in_org(pool.get_ref(), &op, token_org).await?;

    let actor = ActingContext::replay_with_role(teller_id, token_org, actor_role);
    match op {
        ReplayOp::OpenShift {
            branch_id, request, ..
        } => {
            crate::shifts::handlers::open_shift_inner(
                pool.clone(),
                branch_id,
                web::Json(request),
                actor,
            )
            .await
        }
        ReplayOp::CloseShift {
            shift_id, request, ..
        } => {
            crate::shifts::handlers::close_shift_inner(
                pool.clone(),
                shift_id,
                web::Json(request),
                actor,
            )
            .await
        }
        ReplayOp::CreateOrder { request, .. } => {
            // Replay never fires to the KDS (the order is historical) → hub = None.
            // A replayed direct sale has no waiter (only ticket settles do) → None.
            crate::orders::handlers::create_order_inner(
                pool.clone(),
                web::Json(request),
                actor,
                None,
                None,
            )
            .await
        }
        ReplayOp::VoidOrder {
            order_id, request, ..
        } => {
            crate::orders::handlers::void_order_inner(
                pool.clone(),
                order_id,
                web::Json(request),
                actor,
            )
            .await
        }
        ReplayOp::AwardLoyaltyPoints { request, .. } => {
            crate::loyalty::award::award_inner(
                pool.get_ref(),
                request,
                Some(actor.teller_id),
                Some(actor.org_id),
            )
            .await
        }
        ReplayOp::CashMovement {
            shift_id, request, ..
        } => {
            crate::shifts::handlers::add_cash_movement_inner(
                pool.clone(),
                shift_id,
                web::Json(request),
                actor,
            )
            .await
        }
        // Ticket ops: publish to the realtime bus (hub = Some). Waiter devices fire
        // offline-first — the fire/round/settle/void ALWAYS arrives here via the
        // outbox, even when the waiter is online — so a connected teller/KDS only
        // gets a live push (and the ping/notification) if replay publishes. The
        // inner handlers dedup on the idempotency key BEFORE publishing, so an
        // at-least-once retry re-applies as a no-op and emits nothing; only the
        // first apply fires the event. A consumer that was offline still re-seeds
        // via the realtime snapshot on reconnect.
        ReplayOp::FireOpenTicket { request, .. } => {
            crate::tickets::handlers::create_open_ticket_inner(
                pool.clone(),
                web::Json(request),
                actor,
                Some(hub.get_ref()),
            )
            .await
        }
        ReplayOp::AddTicketRound {
            ticket_id, request, ..
        } => {
            crate::tickets::handlers::add_round_inner(
                pool.clone(),
                ticket_id,
                web::Json(request),
                actor,
                Some(hub.get_ref()),
            )
            .await
        }
        ReplayOp::SettleOpenTicket {
            ticket_id, request, ..
        } => {
            crate::tickets::handlers::settle_open_ticket_inner(
                pool.clone(),
                ticket_id,
                web::Json(request),
                actor,
                Some(hub.get_ref()),
            )
            .await
        }
        ReplayOp::VoidOpenTicket {
            ticket_id, request, ..
        } => {
            crate::tickets::handlers::void_open_ticket_inner(
                pool.clone(),
                ticket_id,
                web::Json(request),
                Some(hub.get_ref()),
            )
            .await
        }
        // Bump/unbump: publish so other KDS/till devices reflect the bump live.
        ReplayOp::BumpKitchenItem { item_id, .. } => {
            crate::kitchen::kds::set_bump_inner(
                pool.get_ref(),
                Some(hub.get_ref()),
                &actor,
                item_id,
                true,
            )
            .await
        }
        ReplayOp::UnbumpKitchenItem { item_id, .. } => {
            crate::kitchen::kds::set_bump_inner(
                pool.get_ref(),
                Some(hub.get_ref()),
                &actor,
                item_id,
                false,
            )
            .await
        }
        // Cross-table floor ops: publish (hub = Some) so other tills' canvases
        // update live; the cores dedup before publishing, same as tickets.
        //
        // Parking an order is NOT here. A parked order is a client-local draft,
        // so it has nothing to replay -- which is the point: the offline path
        // for the commonest POS action is now no path at all.
        ReplayOp::SwapTables { request, .. } => {
            crate::floor_ops::handlers::swap_tables_inner(
                pool.clone(),
                web::Json(request),
                actor,
                Some(hub.get_ref()),
            )
            .await
        }
        ReplayOp::CreateTableTransfer { request, .. } => {
            crate::floor_ops::handlers::create_transfer_inner(
                pool.clone(),
                web::Json(request),
                actor,
                Some(hub.get_ref()),
            )
            .await
        }
        ReplayOp::CancelTableTransfer { transfer_id, .. } => {
            crate::floor_ops::handlers::cancel_transfer_inner(
                pool.clone(),
                transfer_id,
                Some(hub.get_ref()),
            )
            .await
        }
        ReplayOp::ClearTable { table_id, .. } => {
            crate::floor_ops::handlers::clear_table_inner(
                pool.clone(),
                table_id,
                // No branch on the wire: the core reads it off the table.
                None,
                actor,
                Some(hub.get_ref()),
            )
            .await
        }
        ReplayOp::FulfillTableTransfer {
            transfer_id,
            request,
            ..
        } => {
            crate::floor_ops::handlers::fulfill_transfer_inner(
                pool.clone(),
                transfer_id,
                web::Json(request),
                actor,
                Some(hub.get_ref()),
            )
            .await
        }
        ReplayOp::SeatBooking {
            booking_id,
            request,
            ..
        } => {
            let resp =
                crate::bookings::handlers::seat_inner(pool.get_ref(), booking_id, &request, &actor)
                    .await?;
            crate::bookings::publish_booking(
                pool.get_ref(),
                hub.get_ref(),
                "booking.changed",
                booking_id,
            )
            .await;
            Ok(resp)
        }
        ReplayOp::NoShowBooking { booking_id, .. } => {
            let resp = crate::bookings::handlers::no_show_inner(pool.get_ref(), booking_id).await?;
            crate::bookings::publish_booking(
                pool.get_ref(),
                hub.get_ref(),
                "booking.changed",
                booking_id,
            )
            .await;
            Ok(resp)
        }
    }
}

/// Verify the op's effective branch belongs to `org` (resolving shift / order to
/// their branch first). A missing target is left to the inner handler (it will
/// 404/409 idempotently) — we only reject a target that exists in a DIFFERENT org.
async fn op_branch_must_be_in_org(pool: &PgPool, op: &ReplayOp, org: Uuid) -> Result<(), AppError> {
    let branch_org: Option<Uuid> = match op {
        ReplayOp::OpenShift { branch_id, .. } => {
            sqlx::query_scalar("SELECT org_id FROM branches WHERE id = $1")
                .bind(branch_id)
                .fetch_optional(pool)
                .await?
        }
        ReplayOp::CreateOrder { request, .. } => {
            sqlx::query_scalar("SELECT org_id FROM branches WHERE id = $1")
                .bind(request.branch_id)
                .fetch_optional(pool)
                .await?
        }
        ReplayOp::CloseShift { shift_id, .. } | ReplayOp::CashMovement { shift_id, .. } => {
            sqlx::query_scalar(
                "SELECT b.org_id FROM shifts s JOIN branches b ON b.id = s.branch_id WHERE s.id = $1",
            )
            .bind(shift_id)
            .fetch_optional(pool)
            .await?
        }
        ReplayOp::VoidOrder { order_id, .. } => {
            sqlx::query_scalar(
                "SELECT b.org_id FROM orders o JOIN branches b ON b.id = o.branch_id WHERE o.id = $1",
            )
            .bind(order_id)
            .fetch_optional(pool)
            .await?
        }
        // Resolved through the branch the op names; `award_inner` then re-checks
        // it against the ORDER's own org, which is the boundary that counts.
        ReplayOp::AwardLoyaltyPoints { request, .. } => {
            sqlx::query_scalar("SELECT org_id FROM branches WHERE id = $1")
                .bind(request.branch_id)
                .fetch_optional(pool)
                .await?
        }
        ReplayOp::FireOpenTicket { request, .. } => {
            sqlx::query_scalar("SELECT org_id FROM branches WHERE id = $1")
                .bind(request.branch_id)
                .fetch_optional(pool)
                .await?
        }
        ReplayOp::AddTicketRound { ticket_id, .. }
        | ReplayOp::SettleOpenTicket { ticket_id, .. }
        | ReplayOp::VoidOpenTicket { ticket_id, .. } => {
            sqlx::query_scalar(
                "SELECT b.org_id FROM open_tickets ot JOIN branches b ON b.id = ot.branch_id WHERE ot.id = $1",
            )
            .bind(ticket_id)
            .fetch_optional(pool)
            .await?
        }
        ReplayOp::BumpKitchenItem { item_id, .. } | ReplayOp::UnbumpKitchenItem { item_id, .. } => {
            sqlx::query_scalar(
                "SELECT b.org_id FROM kitchen_ticket_items kti \
                 JOIN kitchen_tickets kt ON kt.id = kti.kitchen_ticket_id \
                 JOIN branches b ON b.id = kt.branch_id WHERE kti.id = $1",
            )
            .bind(item_id)
            .fetch_optional(pool)
            .await?
        }
        ReplayOp::SwapTables { request, .. } => {
            sqlx::query_scalar("SELECT org_id FROM branches WHERE id = $1")
                .bind(request.branch_id)
                .fetch_optional(pool)
                .await?
        }
        // The op carries no branch, so the table is the only thing to resolve
        // through — which is exactly the cross-org check this fn exists for.
        ReplayOp::ClearTable { table_id, .. } => {
            sqlx::query_scalar(
                "SELECT b.org_id FROM branch_tables bt JOIN branches b ON b.id = bt.branch_id \
                 WHERE bt.id = $1",
            )
            .bind(table_id)
            .fetch_optional(pool)
            .await?
        }
        ReplayOp::CreateTableTransfer { request, .. } => {
            sqlx::query_scalar("SELECT org_id FROM branches WHERE id = $1")
                .bind(request.branch_id)
                .fetch_optional(pool)
                .await?
        }
        ReplayOp::CancelTableTransfer { transfer_id, .. }
        | ReplayOp::FulfillTableTransfer { transfer_id, .. } => {
            sqlx::query_scalar("SELECT org_id FROM table_transfer_requests WHERE id = $1")
                .bind(transfer_id)
                .fetch_optional(pool)
                .await?
        }
        ReplayOp::SeatBooking { booking_id, .. } | ReplayOp::NoShowBooking { booking_id, .. } => {
            sqlx::query_scalar("SELECT org_id FROM bookings WHERE id = $1")
                .bind(booking_id)
                .fetch_optional(pool)
                .await?
        }
    };
    match branch_org {
        Some(o) if o != org => Err(AppError::Forbidden(
            "Replay target belongs to another organization".into(),
        )),
        _ => Ok(()), // same org, or not-yet-present (inner handler resolves it)
    }
}
