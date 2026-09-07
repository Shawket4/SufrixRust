//! Held-order endpoints: the sync list, park (offline-first upsert), the
//! resume claim/release pair, discard/complete tombstones, table assignment,
//! the atomic cross-entity table swap, and the transfer waitlist.
//!
//! Every mutation is split live-route / `*_inner` so `/sync/replay` can flush
//! a till's offline backlog through the same core (same idempotency, same
//! occupancy arbitration). Live wrappers do claims + permission + branch
//! checks; the cores stay claims-free and act for an [`ActingContext`].

use actix_web::{HttpRequest, HttpResponse, web};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::{
    FloorEvents, TransferView, TransfersSyncResponse, autofulfill_transfers, extract_claims,
    free_table, lock_table, occupant_of, require_branch_access, seat_table, transfer_view,
};
use crate::errors::{AppError, AppErrorResponse};
use crate::permissions::checker::{check_permission, check_permission_for};
use crate::realtime::hub::BranchEventHub;
use crate::sync::ActingContext;

// ── Requests ─────────────────────────────────────────────────────────────────

#[derive(Deserialize, Serialize, ToSchema)]
pub struct SwapTablesRequest {
    pub branch_id: Uuid,
    pub table_a: Uuid,
    pub table_b: Uuid,
}

/// Operational table-state edit from the POS: the layout (geometry/shape) is
/// dashboard-authored, but STATE — status walks (bussing a dirty table) and
/// which zone the physical table currently sits in — belongs to the floor
/// staff. Both fields optional; `clear_section` moves the table out of every
/// section (`section_id` wins when both are sent).
#[derive(Deserialize, Serialize, ToSchema)]
pub struct CreateFloorTransferRequest {
    /// Client-minted id (offline-first identity; retries dedup on it).
    pub id: Uuid,
    pub branch_id: Uuid,
    /// `held_order` | `open_ticket`.
    pub occupant_kind: String,
    pub occupant_id: Uuid,
    /// The wish: any table in this section…
    #[serde(default)]
    pub target_section_id: Option<Uuid>,
    /// …or exactly this table. At least one of the two is required.
    #[serde(default)]
    pub target_table_id: Option<Uuid>,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Deserialize, Serialize, ToSchema)]
pub struct FulfillTransferRequest {
    /// The table the party actually moves to (must satisfy the wish).
    pub table_id: Uuid,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListTransfersQuery {
    pub branch_id: Uuid,
    /// Sync cursor (as on /held-orders). Omit for the waiting queue only.
    #[serde(default)]
    pub since: Option<DateTime<Utc>>,
}

// ── Shared lookups ───────────────────────────────────────────────────────────

async fn require_transfer_branch_access(
    pool: &sqlx::PgPool,
    claims: &crate::auth::jwt::Claims,
    id: Uuid,
) -> Result<Uuid, AppError> {
    let branch_id: Option<Uuid> =
        sqlx::query_scalar("SELECT branch_id FROM table_transfer_requests WHERE id = $1")
            .bind(id)
            .fetch_optional(pool)
            .await?;
    let branch_id =
        branch_id.ok_or_else(|| AppError::NotFound("Transfer request not found".into()))?;
    require_branch_access(pool, claims, branch_id).await?;
    Ok(branch_id)
}

/// The live branch org (also confirms the branch exists and isn't deleted).
async fn branch_org(pool: &sqlx::PgPool, branch_id: Uuid) -> Result<Uuid, AppError> {
    sqlx::query_scalar("SELECT org_id FROM branches WHERE id = $1 AND deleted_at IS NULL")
        .bind(branch_id)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| AppError::NotFound("Branch not found".into()))
}

/// Move one ticket onto `to_table` (or off any table when `None`) inside the
/// caller's transaction, and resolve any transfer wish it just satisfied.
async fn move_ticket(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ticket_id: Uuid,
    to_table: Option<Uuid>,
    events: &mut FloorEvents,
) -> Result<(), AppError> {
    sqlx::query("UPDATE open_tickets SET table_id = $2, updated_at = now() WHERE id = $1")
        .bind(ticket_id)
        .bind(to_table)
        .execute(&mut **tx)
        .await?;
    events.tickets.push(ticket_id);
    if let Some(t) = to_table {
        events
            .transfers
            .extend(autofulfill_transfers(tx, ticket_id, t).await?);
    }
    Ok(())
}

// ── Swap (atomic; covers move-to-empty and cross-entity swaps) ───────────────

#[utoipa::path(post, path = "/floor/tables/swap", tag = "floor",
    request_body = SwapTablesRequest,
    responses((status = 200, description = "Occupants swapped/moved"), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn swap_tables(
    req: HttpRequest,
    pool: crate::db::Db,
    hub: web::Data<BranchEventHub>,
    body: web::Json<SwapTablesRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    // Per-occupant permissions are enforced in the core (it knows what sits on
    // each table); here only the branch boundary.
    require_branch_access(pool.get_ref(), &claims, body.branch_id).await?;
    swap_tables_inner(
        pool,
        body,
        ActingContext::live(&claims)?,
        Some(hub.get_ref()),
    )
    .await
}

/// Swap core: exchange the occupants of two tables in ONE transaction. One
/// empty side degenerates to a move; both empty is a 400. Works across entity
/// kinds (a held order can swap with a waiter ticket); the actor needs the
/// `update` permission of every kind it moves.
pub(crate) async fn swap_tables_inner(
    pool: crate::db::Db,
    body: web::Json<SwapTablesRequest>,
    actor: ActingContext,
    hub: Option<&BranchEventHub>,
) -> Result<HttpResponse, AppError> {
    if body.table_a == body.table_b {
        return Err(AppError::BadRequest("Pick two different tables".into()));
    }
    let mut events = FloorEvents::default();
    let mut tx = pool.get_ref().begin().await?;

    // Deadlock-proof: always take the two per-table locks in uuid order.
    let (first, second) = if body.table_a < body.table_b {
        (body.table_a, body.table_b)
    } else {
        (body.table_b, body.table_a)
    };
    for t in [first, second] {
        if !lock_table(&mut tx, t, body.branch_id).await? {
            return Err(AppError::BadRequest("Table is not in this branch".into()));
        }
    }
    let occ_a = occupant_of(&mut tx, body.table_a, None).await?;
    let occ_b = occupant_of(&mut tx, body.table_b, None).await?;
    if occ_a.is_none() && occ_b.is_none() {
        return Err(AppError::BadRequest("Both tables are empty".into()));
    }
    check_permission_for(
        pool.get_ref(),
        actor.teller_id,
        &actor.role,
        "open_tickets",
        "update",
    )
    .await?;

    // Clear both sides before landing either, so a concurrent read never sees
    // two tickets on one table.
    if let Some(t) = occ_a {
        move_ticket(&mut tx, t, None, &mut events).await?;
    }
    if let Some(t) = occ_b {
        move_ticket(&mut tx, t, None, &mut events).await?;
    }
    if let Some(t) = occ_a {
        move_ticket(&mut tx, t, Some(body.table_b), &mut events).await?;
    }
    if let Some(t) = occ_b {
        move_ticket(&mut tx, t, Some(body.table_a), &mut events).await?;
    }
    // A table someone landed on is seated; a side left empty is bused.
    match occ_b {
        Some(_) => seat_table(&mut *tx, body.table_a).await?,
        None => free_table(&mut *tx, body.table_a).await?,
    }
    match occ_a {
        Some(_) => seat_table(&mut *tx, body.table_b).await?,
        None => free_table(&mut *tx, body.table_b).await?,
    }
    events.tables.push(body.table_a);
    events.tables.push(body.table_b);
    tx.commit().await?;

    if let Some(hub) = hub {
        events.publish(pool.get_ref(), hub, body.branch_id).await;
    }
    Ok(HttpResponse::Ok().json(serde_json::json!({ "ok": true })))
}

// ── Clearing a bussed table ─────────────────────────────────────────────────

#[derive(Debug, Deserialize, ToSchema)]
pub struct ClearTableRequest {
    pub branch_id: Uuid,
}

/// Mark a bussed table ready for the next party.
///
/// The ONE human act the derived-status model needs. Everything else about a
/// table's status follows from the ticket on it: seated when one lands, free
/// when nobody vacated, dirty after a checkout. But no server can see that the
/// plates have been cleared, so a person says so.
///
/// Deliberately not a set-status endpoint. Its predecessor took any status and
/// wrote it with no lock and no occupancy check, so it could declare a table
/// free while a ticket was open on it. This performs exactly one transition,
/// `dirty` -> `free`, and refuses anything else.
#[utoipa::path(
    post, path = "/floor/tables/{id}/clear", tag = "floor",
    params(("id" = Uuid, Path, description = "Table ID")),
    request_body = ClearTableRequest,
    responses((status = 200, description = "Table is ready"), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn clear_table(
    req: HttpRequest,
    pool: crate::db::Db,
    hub: web::Data<BranchEventHub>,
    id: web::Path<Uuid>,
    body: web::Json<ClearTableRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "open_tickets", "update").await?;
    require_branch_access(pool.get_ref(), &claims, body.branch_id).await?;
    clear_table_inner(
        pool,
        *id,
        Some(body.branch_id),
        ActingContext::live(&claims)?,
        Some(hub.get_ref()),
    )
    .await
}

/// Clear core: the `dirty` -> `free` walk, shared by the live route and
/// `/sync/replay`.
///
/// `branch_id` is what the LIVE caller asserted, and the table must be in it.
/// Replay passes `None` and the branch is read off the table instead: the POS
/// queues this op with an empty `request` body (it has always sent `{}`), so
/// there is no branch on the wire — and shipped builds cannot be asked to start
/// sending one. That lookup is safe without a branch check of its own: `pool` is
/// tenant-scoped, so a table outside the caller's org is not visible and this
/// 404s (and `op_branch_must_be_in_org` has already rejected a cross-org table
/// outright), while the replay actor is always a teller/waiter/kitchen user —
/// all org-scoped rather than branch-scoped.
pub(crate) async fn clear_table_inner(
    pool: crate::db::Db,
    table_id: Uuid,
    branch_id: Option<Uuid>,
    actor: ActingContext,
    hub: Option<&BranchEventHub>,
) -> Result<HttpResponse, AppError> {
    let branch_id = match branch_id {
        Some(b) => b,
        None => sqlx::query_scalar("SELECT branch_id FROM branch_tables WHERE id = $1")
            .bind(table_id)
            .fetch_optional(pool.get_ref())
            .await?
            .ok_or_else(|| AppError::NotFound("Table not found".into()))?,
    };
    check_permission_for(
        pool.get_ref(),
        actor.teller_id,
        &actor.role,
        "open_tickets",
        "update",
    )
    .await?;

    let mut tx = pool.get_ref().begin().await?;
    if !lock_table(&mut tx, table_id, branch_id).await? {
        return Err(AppError::NotFound("Table not found".into()));
    }
    // A table someone is sitting at is not "bussed", whatever its status says.
    if occupant_of(&mut tx, table_id, None).await?.is_some() {
        return Err(AppError::Conflict("Someone is seated at this table".into()));
    }
    let status: String = sqlx::query_scalar("SELECT status FROM branch_tables WHERE id = $1")
        .bind(table_id)
        .fetch_one(&mut *tx)
        .await?;
    if status != "dirty" {
        // Idempotent for the common double-tap; loud for anything else.
        if status == "free" {
            tx.commit().await?;
            return Ok(HttpResponse::Ok().json(serde_json::json!({ "ok": true })));
        }
        return Err(AppError::Conflict(format!(
            "Table is {status}, not waiting to be cleared"
        )));
    }
    free_table(&mut *tx, table_id).await?;
    tx.commit().await?;

    if let Some(hub) = hub {
        let mut events = FloorEvents::default();
        events.tables.push(table_id);
        events.publish(pool.get_ref(), hub, branch_id).await;
    }
    Ok(HttpResponse::Ok().json(serde_json::json!({ "ok": true })))
}

// ── Transfer waitlist ────────────────────────────────────────────────────────

#[utoipa::path(get, path = "/floor/transfers", tag = "floor_transfers", params(ListTransfersQuery),
    responses((status = 200, body = TransfersSyncResponse), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn list_floor_transfers(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<ListTransfersQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "table_transfers", "read").await?;
    require_branch_access(pool.get_ref(), &claims, query.branch_id).await?;

    let server_time = Utc::now();
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM table_transfer_requests \
         WHERE branch_id = $1 \
           AND (($2::timestamptz IS NULL AND status = 'waiting') \
                OR ($2 IS NOT NULL AND updated_at > $2)) \
         ORDER BY created_at LIMIT 500",
    )
    .bind(query.branch_id)
    .bind(query.since)
    .fetch_all(pool.get_ref())
    .await?;
    let mut transfers = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(v) = transfer_view(pool.get_ref(), id).await? {
            transfers.push(v);
        }
    }
    Ok(HttpResponse::Ok().json(TransfersSyncResponse {
        server_time,
        transfers,
    }))
}

#[utoipa::path(post, path = "/floor/transfers", tag = "floor_transfers",
    request_body = CreateFloorTransferRequest,
    responses((status = 200, body = TransferView), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn create_floor_transfer(
    req: HttpRequest,
    pool: crate::db::Db,
    hub: web::Data<BranchEventHub>,
    body: web::Json<CreateFloorTransferRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "table_transfers", "create").await?;
    require_branch_access(pool.get_ref(), &claims, body.branch_id).await?;
    create_transfer_inner(
        pool,
        body,
        ActingContext::live(&claims)?,
        Some(hub.get_ref()),
    )
    .await
}

/// Create core. The occupant must be LIVE in this branch (a parked/resumed held
/// order or an open/ready ticket — a no-table "outside" order queues too, with
/// `from_table_id: null`). One waiting wish per party; a retried create with
/// the same id dedups to the stored row.
pub(crate) async fn create_transfer_inner(
    pool: crate::db::Db,
    body: web::Json<CreateFloorTransferRequest>,
    actor: ActingContext,
    hub: Option<&BranchEventHub>,
) -> Result<HttpResponse, AppError> {
    // Idempotent retry: the id already landed → return it as stored.
    if let Some(existing) = transfer_view(pool.get_ref(), body.id).await? {
        return Ok(HttpResponse::Ok().json(existing));
    }
    if body.target_section_id.is_none() && body.target_table_id.is_none() {
        return Err(AppError::BadRequest(
            "A transfer needs a target section or table".into(),
        ));
    }
    if let Some(note) = &body.note
        && note.chars().count() > 500
    {
        return Err(AppError::BadRequest("Note is too long".into()));
    }
    let org_id = branch_org(pool.get_ref(), body.branch_id).await?;

    // Validate the wish against this branch's layout.
    if let Some(s) = body.target_section_id {
        let ok: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM floor_sections WHERE id = $1 AND branch_id = $2)",
        )
        .bind(s)
        .bind(body.branch_id)
        .fetch_one(pool.get_ref())
        .await?;
        if !ok {
            return Err(AppError::BadRequest(
                "Target section is not in this branch".into(),
            ));
        }
    }
    if let Some(t) = body.target_table_id {
        let ok: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM branch_tables WHERE id = $1 AND branch_id = $2)",
        )
        .bind(t)
        .bind(body.branch_id)
        .fetch_one(pool.get_ref())
        .await?;
        if !ok {
            return Err(AppError::BadRequest(
                "Target table is not in this branch".into(),
            ));
        }
    }

    // The occupant must be live here; its CURRENT table becomes `from_table_id`.
    let from_table: Option<Uuid> = sqlx::query_scalar(
        "SELECT table_id FROM open_tickets \
         WHERE id = $1 AND branch_id = $2 AND status IN ('open','ready')",
    )
    .bind(body.occupant_id)
    .bind(body.branch_id)
    .fetch_optional(pool.get_ref())
    .await?
    .ok_or_else(|| AppError::NotFound("Order not found".into()))?;

    let waiting: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM table_transfer_requests \
         WHERE occupant_kind = $1 AND occupant_id = $2 AND status = 'waiting'",
    )
    .bind(&body.occupant_kind)
    .bind(body.occupant_id)
    .fetch_optional(pool.get_ref())
    .await?;
    if waiting.is_some() {
        return Err(AppError::Conflict(
            "This party already has a waiting transfer".into(),
        ));
    }

    sqlx::query(
        "INSERT INTO table_transfer_requests \
            (id, org_id, branch_id, occupant_kind, occupant_id, from_table_id, \
             target_section_id, target_table_id, note, requested_by) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
    )
    .bind(body.id)
    .bind(org_id)
    .bind(body.branch_id)
    .bind(&body.occupant_kind)
    .bind(body.occupant_id)
    .bind(from_table)
    .bind(body.target_section_id)
    .bind(body.target_table_id)
    .bind(&body.note)
    .bind(actor.teller_id)
    .execute(pool.get_ref())
    .await?;

    if let Some(hub) = hub {
        let mut events = FloorEvents::default();
        events.transfers.push(body.id);
        events.publish(pool.get_ref(), hub, body.branch_id).await;
    }
    let view = transfer_view(pool.get_ref(), body.id)
        .await?
        .ok_or(AppError::Internal)?;
    Ok(HttpResponse::Ok().json(view))
}

#[utoipa::path(post, path = "/floor/transfers/{id}/cancel", tag = "floor_transfers",
    params(("id" = Uuid, Path, description = "Transfer request ID")),
    responses((status = 200, body = TransferView), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn cancel_transfer(
    req: HttpRequest,
    pool: crate::db::Db,
    hub: web::Data<BranchEventHub>,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "table_transfers", "update").await?;
    require_transfer_branch_access(pool.get_ref(), &claims, *id).await?;
    cancel_transfer_inner(pool, *id, Some(hub.get_ref())).await
}

pub(crate) async fn cancel_transfer_inner(
    pool: crate::db::Db,
    id: Uuid,
    hub: Option<&BranchEventHub>,
) -> Result<HttpResponse, AppError> {
    let row: Option<(Uuid, String)> =
        sqlx::query_as("SELECT branch_id, status FROM table_transfer_requests WHERE id = $1")
            .bind(id)
            .fetch_optional(pool.get_ref())
            .await?;
    let Some((branch_id, status)) = row else {
        return Err(AppError::NotFound("Transfer request not found".into()));
    };
    match status.as_str() {
        "cancelled" => {} // idempotent
        "fulfilled" => {
            return Err(AppError::Conflict("Transfer is already fulfilled".into()));
        }
        _ => {
            sqlx::query(
                "UPDATE table_transfer_requests \
                 SET status = 'cancelled', resolved_at = now(), updated_at = now() \
                 WHERE id = $1 AND status = 'waiting'",
            )
            .bind(id)
            .execute(pool.get_ref())
            .await?;
            if let Some(hub) = hub {
                let mut events = FloorEvents::default();
                events.transfers.push(id);
                events.publish(pool.get_ref(), hub, branch_id).await;
            }
        }
    }
    let view = transfer_view(pool.get_ref(), id)
        .await?
        .ok_or(AppError::Internal)?;
    Ok(HttpResponse::Ok().json(view))
}

#[utoipa::path(post, path = "/floor/transfers/{id}/fulfill", tag = "floor_transfers",
    params(("id" = Uuid, Path, description = "Transfer request ID")),
    request_body = FulfillTransferRequest,
    responses((status = 200, body = TransferView), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn fulfill_transfer(
    req: HttpRequest,
    pool: crate::db::Db,
    hub: web::Data<BranchEventHub>,
    id: web::Path<Uuid>,
    body: web::Json<FulfillTransferRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "table_transfers", "update").await?;
    require_transfer_branch_access(pool.get_ref(), &claims, *id).await?;
    fulfill_transfer_inner(
        pool,
        *id,
        body,
        ActingContext::live(&claims)?,
        Some(hub.get_ref()),
    )
    .await
}

/// Fulfill core: seat the waiting party on `table_id` — which must satisfy the
/// wish (the exact wished table, or any table in the wished section) and be
/// free — through the same arbitration as every other move.
pub(crate) async fn fulfill_transfer_inner(
    pool: crate::db::Db,
    id: Uuid,
    body: web::Json<FulfillTransferRequest>,
    actor: ActingContext,
    hub: Option<&BranchEventHub>,
) -> Result<HttpResponse, AppError> {
    let mut events = FloorEvents::default();
    let mut tx = pool.get_ref().begin().await?;
    #[allow(clippy::type_complexity)]
    let row: Option<(Uuid, String, Uuid, Option<Uuid>, Option<Uuid>)> = sqlx::query_as(
        "SELECT branch_id, status, occupant_id, target_section_id, target_table_id \
         FROM table_transfer_requests WHERE id = $1 FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((branch_id, status, occupant_id, target_section, target_table)) = row else {
        return Err(AppError::NotFound("Transfer request not found".into()));
    };
    match status.as_str() {
        "fulfilled" => {
            tx.commit().await?; // replayed fulfill — idempotent
            let view = transfer_view(pool.get_ref(), id)
                .await?
                .ok_or(AppError::Internal)?;
            return Ok(HttpResponse::Ok().json(view));
        }
        "cancelled" => {
            return Err(AppError::Conflict("Transfer is already cancelled".into()));
        }
        _ => {}
    }

    // The chosen table must satisfy the wish.
    if let Some(t) = target_table
        && t != body.table_id
    {
        return Err(AppError::BadRequest(
            "The party asked for a different table".into(),
        ));
    }
    if !lock_table(&mut tx, body.table_id, branch_id).await? {
        return Err(AppError::BadRequest("Table is not in this branch".into()));
    }
    if target_table.is_none()
        && let Some(section) = target_section
    {
        let in_section: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM branch_tables WHERE id = $1 AND section_id = $2)",
        )
        .bind(body.table_id)
        .bind(section)
        .fetch_one(&mut *tx)
        .await?;
        if !in_section {
            return Err(AppError::BadRequest(
                "Table is not in the section the party asked for".into(),
            ));
        }
    }

    // The ticket must still be live.
    let live: Option<Option<Uuid>> = sqlx::query_scalar(
        "SELECT table_id FROM open_tickets WHERE id = $1 AND status IN ('open','ready')",
    )
    .bind(occupant_id)
    .fetch_optional(&mut *tx)
    .await?;
    if live.is_none() {
        return Err(AppError::Conflict(
            "The party's order is no longer live".into(),
        ));
    }
    check_permission_for(
        pool.get_ref(),
        actor.teller_id,
        &actor.role,
        "open_tickets",
        "update",
    )
    .await?;

    if occupant_of(&mut tx, body.table_id, Some(occupant_id))
        .await?
        .is_some()
    {
        return Err(AppError::Conflict("Table is already occupied".into()));
    }

    // The old table (if any) frees up; the party lands on the new one.
    let old_table: Option<Uuid> =
        sqlx::query_scalar("SELECT table_id FROM open_tickets WHERE id = $1")
            .bind(occupant_id)
            .fetch_one(&mut *tx)
            .await?;
    move_ticket(&mut tx, occupant_id, Some(body.table_id), &mut events).await?;
    if let Some(old) = old_table
        && old != body.table_id
    {
        free_table(&mut *tx, old).await?;
        events.tables.push(old);
    }
    seat_table(&mut *tx, body.table_id).await?;
    events.tables.push(body.table_id);

    // `autofulfill_transfers` inside `move_occupant` resolves this request when
    // the wish matches; a section wish landing on a table WITHOUT a section
    // (edge: table moved out of the section since) still needs the explicit stamp.
    sqlx::query(
        "UPDATE table_transfer_requests \
         SET status = 'fulfilled', fulfilled_table_id = $2, resolved_at = now(), updated_at = now() \
         WHERE id = $1 AND status = 'waiting'",
    )
    .bind(id)
    .bind(body.table_id)
    .execute(&mut *tx)
    .await?;
    events.transfers.push(id);
    tx.commit().await?;

    if let Some(hub) = hub {
        events.publish(pool.get_ref(), hub, branch_id).await;
    }
    let view = transfer_view(pool.get_ref(), id)
        .await?
        .ok_or(AppError::Internal)?;
    Ok(HttpResponse::Ok().json(view))
}
