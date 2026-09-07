//! Table occupancy and the transfer waitlist.
//!
//! The invariant under test: a table has at most one live occupant, an occupant
//! is always an open ticket, and `branch_tables.status` is only ever moved by
//! the three shared walks. Parked orders are client-local drafts and have no
//! server presence to test.

use actix_web::{App, test, web};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::jwt::{JwtSecret, create_token};
use crate::floor_ops::TransferView;
use crate::models::UserRole;
use crate::realtime::hub::BranchEventHub;
use crate::tickets::OpenTicketView;

fn secret() -> JwtSecret {
    JwtSecret("secret".into())
}
fn token(uid: Uuid, org: Uuid, role: UserRole) -> String {
    create_token(&secret(), uid, Some(org), role, None, 24).unwrap()
}

async fn seed_org(pool: &PgPool) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, 'Org', $2)")
        .bind(id)
        .bind(format!("org-{id}"))
        .execute(pool)
        .await
        .unwrap();
    id
}
async fn seed_branch(pool: &PgPool, org: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO branches (id, org_id, name) VALUES ($1, $2, 'Branch')")
        .bind(id)
        .bind(org)
        .execute(pool)
        .await
        .unwrap();
    id
}
async fn seed_user(pool: &PgPool, org: Uuid, role: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, org_id, name, email, password_hash, role) \
         VALUES ($1, $2, $3, $4, 'h', $5::user_role)",
    )
    .bind(id)
    .bind(org)
    .bind(format!("{role}-{id}"))
    .bind(format!("{id}@t.com"))
    .bind(role)
    .execute(pool)
    .await
    .unwrap();
    id
}
async fn seed_menu_item(pool: &PgPool, org: Uuid, price: i32) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO menu_items (id, org_id, name, base_price) VALUES ($1, $2, 'Burger', $3)",
    )
    .bind(id)
    .bind(org)
    .bind(price)
    .execute(pool)
    .await
    .unwrap();
    id
}
async fn seed_section(pool: &PgPool, org: Uuid, branch: Uuid, name: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO floor_sections (id, org_id, branch_id, name) VALUES ($1, $2, $3, $4)")
        .bind(id)
        .bind(org)
        .bind(branch)
        .bind(name)
        .execute(pool)
        .await
        .unwrap();
    id
}
async fn seed_table(
    pool: &PgPool,
    org: Uuid,
    branch: Uuid,
    section: Option<Uuid>,
    label: &str,
) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO branch_tables (id, org_id, branch_id, section_id, label) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(id)
    .bind(org)
    .bind(branch)
    .bind(section)
    .bind(label)
    .execute(pool)
    .await
    .unwrap();
    id
}
async fn table_status(pool: &PgPool, table: Uuid) -> String {
    sqlx::query_scalar("SELECT status FROM branch_tables WHERE id = $1")
        .bind(table)
        .fetch_one(pool)
        .await
        .unwrap()
}
/// An open shift, returning its id (the settle path needs one to bank into).
async fn open_shift_row(pool: &PgPool, branch: Uuid, teller: Uuid) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO shifts (branch_id, teller_id, status, opening_cash) \
         VALUES ($1,$2,'open',0) RETURNING id",
    )
    .bind(branch)
    .bind(teller)
    .fetch_one(pool)
    .await
    .unwrap()
}
async fn seed_cash_method(pool: &PgPool, org: Uuid) {
    sqlx::query(
        "INSERT INTO org_payment_methods (org_id, name, color, icon, is_cash, is_active) \
         VALUES ($1, 'cash', '#000', 'cash', true, true)",
    )
    .bind(org)
    .execute(pool)
    .await
    .unwrap();
}
async fn shift_row(pool: &PgPool, branch: Uuid, teller: Uuid) {
    sqlx::query(
        "INSERT INTO shifts (branch_id, teller_id, status, opening_cash) VALUES ($1,$2,'open',0)",
    )
    .bind(branch)
    .bind(teller)
    .execute(pool)
    .await
    .unwrap();
}
async fn grant(pool: &PgPool, role: &str, resource: &str, action: &str) {
    sqlx::query(
        "INSERT INTO role_permissions (role, resource, action, granted) \
         VALUES ($1::user_role, $2::permission_resource, $3::permission_action, true) ON CONFLICT DO NOTHING",
    )
    .bind(role).bind(resource).bind(action).execute(pool).await.unwrap();
}
async fn grant_defaults(pool: &PgPool) {
    for (resource, action) in [
        ("held_orders", "create"),
        ("held_orders", "read"),
        ("held_orders", "update"),
        ("table_transfers", "create"),
        ("table_transfers", "read"),
        ("table_transfers", "update"),
        ("open_tickets", "read"),
        ("open_tickets", "update"),
        // The host ops the seeder really grants a teller — table state included,
        // which is how a checked-out table gets cleared from the POS.
        ("floor_plan", "read"),
        ("reservations", "read"),
        ("reservations", "update"),
    ] {
        grant(pool, "teller", resource, action).await;
    }
    for (resource, action) in [
        ("open_tickets", "create"),
        ("open_tickets", "read"),
        ("open_tickets", "update"),
        ("table_transfers", "create"),
        ("table_transfers", "read"),
        ("table_transfers", "update"),
        ("held_orders", "read"),
    ] {
        grant(pool, "waiter", resource, action).await;
    }
}

macro_rules! app {
    ($pool:expr) => {
        test::init_service(
            App::new()
                .app_data(web::Data::new($pool.clone()))
                .app_data(web::Data::new(secret()))
                .app_data(web::Data::new(BranchEventHub::new()))
                // The real `/floor` scope, ops routes included.
                .configure(crate::reservations::routes::configure)
                .configure(crate::tickets::routes::configure)
                .configure(crate::sync::routes::configure),
        )
        .await
    };
}

macro_rules! post_json {
    ($app:expr, $tok:expr, $uri:expr, $body:expr) => {
        test::call_service(
            &$app,
            test::TestRequest::post()
                .uri($uri)
                .insert_header(("Authorization", format!("Bearer {}", $tok)))
                .set_json(&$body)
                .to_request(),
        )
        .await
    };
}
macro_rules! get_req {
    ($app:expr, $tok:expr, $uri:expr) => {
        test::call_service(
            &$app,
            test::TestRequest::get()
                .uri($uri)
                .insert_header(("Authorization", format!("Bearer {}", $tok)))
                .to_request(),
        )
        .await
    };
}

/// Fire a ticket, optionally onto a table. Returns the created ticket.
///
/// The only way an order reaches the floor now, so it stands in wherever these
/// tests used to park a held order.
macro_rules! fire_on {
    ($app:expr, $tok:expr, $branch:expr, $item:expr, $table:expr) => {{
        let resp = post_json!(
            $app,
            $tok,
            "/open-tickets",
            serde_json::json!({
                "branch_id": $branch, "table_id": $table,
                "items": [{ "menu_item_id": $item, "quantity": 1 }]
            })
        );
        assert_eq!(resp.status(), 201, "fire ticket");
        let t: OpenTicketView = test::read_body_json(resp).await;
        t
    }};
}

// ── Swap: the atomic two-table exchange ──────────────────────────────────────

#[sqlx::test]
async fn swap_exchanges_two_tickets_atomically(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let teller = seed_user(&pool, org, "teller").await;
    let waiter = seed_user(&pool, org, "waiter").await;
    let item = seed_menu_item(&pool, org, 1000).await;
    shift_row(&pool, branch, teller).await;
    grant_defaults(&pool).await;
    let t = token(teller, org, UserRole::Teller);
    let w = token(waiter, org, UserRole::Waiter);
    let t1 = seed_table(&pool, org, branch, None, "T1").await;
    let t2 = seed_table(&pool, org, branch, None, "T2").await;
    let t3 = seed_table(&pool, org, branch, None, "T3").await;

    let a = fire_on!(app, w, branch, item, t1);
    let b = fire_on!(app, w, branch, item, t2);

    // The exchange.
    let resp = post_json!(
        app,
        t,
        "/floor/tables/swap",
        serde_json::json!({ "branch_id": branch, "table_a": t1, "table_b": t2 })
    );
    assert_eq!(resp.status(), 200);

    let seat_of = |id: Uuid| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, Option<Uuid>>("SELECT table_id FROM open_tickets WHERE id=$1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap()
        }
    };
    assert_eq!(
        (seat_of(a.id).await, seat_of(b.id).await),
        (Some(t2), Some(t1)),
        "the two parties exchanged tables"
    );
    assert_eq!(table_status(&pool, t1).await, "seated");
    assert_eq!(table_status(&pool, t2).await, "seated");

    // Swapping with an EMPTY table degenerates to a move: the vacated side is
    // FREE, not dirty -- nobody ate there, the party simply moved.
    let resp = post_json!(
        app,
        t,
        "/floor/tables/swap",
        serde_json::json!({ "branch_id": branch, "table_a": t1, "table_b": t3 })
    );
    assert_eq!(resp.status(), 200);
    assert_eq!(seat_of(b.id).await, Some(t3));
    assert_eq!(table_status(&pool, t1).await, "free");
    assert_eq!(table_status(&pool, t3).await, "seated");

    // Two empty tables is a no-op the caller should hear about, rather than a
    // silent success that looks like something happened.
    let t4 = seed_table(&pool, org, branch, None, "T4").await;
    let resp = post_json!(
        app,
        t,
        "/floor/tables/swap",
        serde_json::json!({ "branch_id": branch, "table_a": t1, "table_b": t4 })
    );
    assert_eq!(resp.status(), 400, "both tables empty");
}

// ── Ticket-side arbitration ──────────────────────────────────────────────────

#[sqlx::test]
async fn ticket_fire_drops_occupied_table_and_move_conflicts(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let teller = seed_user(&pool, org, "teller").await;
    let waiter = seed_user(&pool, org, "waiter").await;
    let item = seed_menu_item(&pool, org, 1000).await;
    shift_row(&pool, branch, teller).await;
    grant_defaults(&pool).await;
    let t = token(teller, org, UserRole::Teller);
    let w = token(waiter, org, UserRole::Waiter);
    let t1 = seed_table(&pool, org, branch, None, "T1").await;
    let t2 = seed_table(&pool, org, branch, None, "T2").await;

    // A ticket owns T1.
    let a = fire_on!(app, w, branch, item, t1);
    let _ = a;

    // A fire onto the occupied T1 still succeeds — table-less (never dead-letters).
    let resp = post_json!(
        app,
        w,
        "/open-tickets",
        serde_json::json!({
            "branch_id": branch, "table_id": t1,
            "items": [{ "menu_item_id": item, "quantity": 1 }]
        })
    );
    assert_eq!(resp.status(), 201);
    let ticket: OpenTicketView = test::read_body_json(resp).await;
    assert_eq!(
        ticket.table_id, None,
        "occupied table is dropped, not fatal"
    );

    // Fire onto free T2 seats it.
    let resp = post_json!(
        app,
        w,
        "/open-tickets",
        serde_json::json!({
            "branch_id": branch, "table_id": t2,
            "items": [{ "menu_item_id": item, "quantity": 1 }]
        })
    );
    let ticket2: OpenTicketView = test::read_body_json(resp).await;
    assert_eq!(ticket2.table_id, Some(t2));
    assert_eq!(table_status(&pool, t2).await, "seated");

    // The interactive move onto an OCCUPIED table is a loud 409: an
    // interactive action, unlike an offline fire, can be told 'no'.
    let resp = test::call_service(
        &app,
        test::TestRequest::patch()
            .uri(&format!("/open-tickets/{}/table", ticket2.id))
            .insert_header(("Authorization", format!("Bearer {w}")))
            .set_json(&serde_json::json!({ "table_id": t1 }))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 409);

    // Voiding the T2 ticket hands the table straight back: a voided ticket
    // never served the party, so there is nothing to bus.
    let resp = post_json!(
        app,
        w,
        &format!("/open-tickets/{}/void", ticket2.id),
        serde_json::json!({ "reason": "test" })
    );
    assert_eq!(resp.status(), 200);
    assert_eq!(table_status(&pool, t2).await, "free");
}

/// SETTLING a dine-in ticket is a checkout: the party paid and left their
/// plates, so the table lands `dirty` and waits for a human. This is the
/// contract the POS's post-checkout prompt and its one-tap clear are built on
/// — if settle went back to `free`, both would be lying about the room.
#[sqlx::test]
async fn settling_a_ticket_buses_its_table(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let teller = seed_user(&pool, org, "teller").await;
    let waiter = seed_user(&pool, org, "waiter").await;
    let item = seed_menu_item(&pool, org, 1000).await;
    let shift = open_shift_row(&pool, branch, teller).await;
    seed_cash_method(&pool, org).await;
    grant_defaults(&pool).await;
    for (resource, action) in [
        ("orders", "create"),
        ("payments", "create"),
        ("kitchen_orders", "read"),
        ("kitchen_orders", "update"),
    ] {
        grant(&pool, "teller", resource, action).await;
    }
    let t = token(teller, org, UserRole::Teller);
    let w = token(waiter, org, UserRole::Waiter);
    let t1 = seed_table(&pool, org, branch, None, "T1").await;

    let resp = post_json!(
        app,
        w,
        "/open-tickets",
        serde_json::json!({
            "branch_id": branch, "table_id": t1,
            "items": [{ "menu_item_id": item, "quantity": 1 }]
        })
    );
    assert_eq!(resp.status(), 201);
    let ticket: OpenTicketView = test::read_body_json(resp).await;
    assert_eq!(ticket.table_id, Some(t1));
    assert_eq!(table_status(&pool, t1).await, "seated");

    let resp = post_json!(
        app,
        t,
        &format!("/open-tickets/{}/settle", ticket.id),
        serde_json::json!({ "shift_id": shift, "payment_method": "cash" })
    );
    assert_eq!(resp.status(), 200, "cashier settles the ticket");
    assert_eq!(
        table_status(&pool, t1).await,
        "dirty",
        "a checked-out table needs a bus — it is NOT handed back automatically"
    );

    // Only a human clearing it makes it available again — no server can see
    // that the plates are gone.
    let resp = post_json!(
        app,
        t,
        &format!("/floor/tables/{t1}/clear"),
        serde_json::json!({ "branch_id": branch })
    );
    assert_eq!(resp.status(), 200);
    assert_eq!(table_status(&pool, t1).await, "free");

    // Clearing again is idempotent: a double-tap on the POS prompt is not an
    // error, and treating it as one would teach staff to ignore errors.
    let resp = post_json!(
        app,
        t,
        &format!("/floor/tables/{t1}/clear"),
        serde_json::json!({ "branch_id": branch })
    );
    assert_eq!(resp.status(), 200);
}

// ── Transfer waitlist ────────────────────────────────────────────────────────

#[sqlx::test]
async fn transfer_waitlist_lifecycle(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let teller = seed_user(&pool, org, "teller").await;
    grant_defaults(&pool).await;
    let waiter = seed_user(&pool, org, "waiter").await;
    let t = token(teller, org, UserRole::Teller);
    let w = token(waiter, org, UserRole::Waiter);
    let item = seed_menu_item(&pool, org, 1000).await;
    shift_row(&pool, branch, teller).await;
    let outside = seed_section(&pool, org, branch, "Outside").await;
    let inside = seed_section(&pool, org, branch, "Inside").await;
    let t_out = seed_table(&pool, org, branch, Some(outside), "O1").await;
    let t_in = seed_table(&pool, org, branch, Some(inside), "I1").await;

    // A party seated outside wants "anywhere inside".
    let a = fire_on!(app, w, branch, item, t_out).id;
    let wish = Uuid::new_v4();
    let resp = post_json!(
        app,
        t,
        "/floor/transfers",
        serde_json::json!({
            "id": wish, "branch_id": branch, "occupant_kind": "open_ticket", "occupant_id": a,
            "target_section_id": inside, "note": "crowded out here"
        })
    );
    assert_eq!(resp.status(), 200);
    let view: TransferView = test::read_body_json(resp).await;
    assert_eq!(view.status, "waiting");
    assert_eq!(
        view.from_table_id,
        Some(t_out),
        "current table derived server-side"
    );
    // The queue labels a party by their ticket reference — the thing staff
    // actually say out loud.
    assert!(
        view.occupant_label
            .as_deref()
            .is_some_and(|l| l.starts_with("T-")),
        "expected a ticket ref, got {:?}",
        view.occupant_label
    );

    // Retrying the SAME create dedups; a SECOND wish for the party conflicts.
    let resp = post_json!(
        app,
        t,
        "/floor/transfers",
        serde_json::json!({
            "id": wish, "branch_id": branch, "occupant_kind": "open_ticket", "occupant_id": a,
            "target_section_id": inside
        })
    );
    assert_eq!(resp.status(), 200);
    let resp = post_json!(
        app,
        t,
        "/floor/transfers",
        serde_json::json!({
            "id": Uuid::new_v4(), "branch_id": branch, "occupant_kind": "open_ticket", "occupant_id": a,
            "target_section_id": inside
        })
    );
    assert_eq!(resp.status(), 409);

    // Fulfilling onto a table OUTSIDE the wished section is rejected.
    let resp = post_json!(
        app,
        t,
        &format!("/floor/transfers/{wish}/fulfill"),
        serde_json::json!({ "table_id": t_out })
    );
    assert_eq!(resp.status(), 400);

    // Fulfil onto I1: the party moves, O1 is bused, the wish resolves.
    let resp = post_json!(
        app,
        t,
        &format!("/floor/transfers/{wish}/fulfill"),
        serde_json::json!({ "table_id": t_in })
    );
    assert_eq!(resp.status(), 200);
    let view: TransferView = test::read_body_json(resp).await;
    assert_eq!(view.status, "fulfilled");
    assert_eq!(view.fulfilled_table_id, Some(t_in));
    let ta: Option<Uuid> = sqlx::query_scalar("SELECT table_id FROM open_tickets WHERE id=$1")
        .bind(a)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(ta, Some(t_in));
    assert_eq!(table_status(&pool, t_out).await, "free");
    assert_eq!(table_status(&pool, t_in).await, "seated");

    // Replayed fulfil is idempotent; cancelling a fulfilled wish conflicts.
    let resp = post_json!(
        app,
        t,
        &format!("/floor/transfers/{wish}/fulfill"),
        serde_json::json!({ "table_id": t_in })
    );
    assert_eq!(resp.status(), 200);
    let resp = post_json!(
        app,
        t,
        &format!("/floor/transfers/{wish}/cancel"),
        serde_json::json!({})
    );
    assert_eq!(resp.status(), 409);
}

#[sqlx::test]
async fn assigning_into_the_wished_section_autofulfills(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let teller = seed_user(&pool, org, "teller").await;
    grant_defaults(&pool).await;
    let waiter = seed_user(&pool, org, "waiter").await;
    let t = token(teller, org, UserRole::Teller);
    let w = token(waiter, org, UserRole::Waiter);
    let item = seed_menu_item(&pool, org, 1000).await;
    shift_row(&pool, branch, teller).await;
    let inside = seed_section(&pool, org, branch, "Inside").await;
    let t_in = seed_table(&pool, org, branch, Some(inside), "I1").await;

    // A table-less ("waiting at the door") order queues for inside.
    let a = fire_on!(app, w, branch, item, serde_json::Value::Null).id;
    let wish = Uuid::new_v4();
    let resp = post_json!(
        app,
        t,
        "/floor/transfers",
        serde_json::json!({
            "id": wish, "branch_id": branch, "occupant_kind": "open_ticket", "occupant_id": a,
            "target_section_id": inside
        })
    );
    assert_eq!(resp.status(), 200);
    let view: TransferView = test::read_body_json(resp).await;
    assert_eq!(
        view.from_table_id, None,
        "an outside order has no from-table"
    );

    // A plain table assignment into the wished section resolves the wish.
    let resp = post_json!(
        app,
        t,
        &format!("/floor/transfers/{wish}/fulfill"),
        serde_json::json!({ "table_id": t_in })
    );
    assert_eq!(resp.status(), 200);
    let status: String =
        sqlx::query_scalar("SELECT status FROM table_transfer_requests WHERE id=$1")
            .bind(wish)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status, "fulfilled");

    // Voiding the order cancels a waiting wish: the party left the floor, so
    // the queue must not keep holding a place for them.
    let b = fire_on!(app, w, branch, item, serde_json::Value::Null).id;
    let wish_b = Uuid::new_v4();
    post_json!(
        app,
        t,
        "/floor/transfers",
        serde_json::json!({
            "id": wish_b, "branch_id": branch, "occupant_kind": "open_ticket", "occupant_id": b,
            "target_section_id": inside
        })
    );
    let resp = post_json!(
        app,
        t,
        &format!("/open-tickets/{b}/void"),
        serde_json::json!({ "reason": "left" })
    );
    assert_eq!(resp.status(), 200);
    let status: String =
        sqlx::query_scalar("SELECT status FROM table_transfer_requests WHERE id=$1")
            .bind(wish_b)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status, "cancelled");
}

// ── Replay (offline outbox) ──────────────────────────────────────────────────

#[sqlx::test]
async fn replay_applies_a_queued_swap_and_honours_role_boundaries(pool: PgPool) {
    // Offline table moves still replay through the same core as the live route.
    // Parking is NOT here: a parked order is a client-local draft, so it has
    // nothing to replay -- the offline path for the commonest POS action is now
    // no path at all.
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let teller = seed_user(&pool, org, "teller").await;
    let waiter = seed_user(&pool, org, "waiter").await;
    let item = seed_menu_item(&pool, org, 1000).await;
    shift_row(&pool, branch, teller).await;
    grant_defaults(&pool).await;
    let t = token(teller, org, UserRole::Teller);
    let w = token(waiter, org, UserRole::Waiter);
    let t1 = seed_table(&pool, org, branch, None, "T1").await;
    let t2 = seed_table(&pool, org, branch, None, "T2").await;

    let a = fire_on!(app, w, branch, item, t1);

    // A queued offline swap replays and applies.
    let resp = post_json!(
        app,
        t,
        "/sync/replay",
        serde_json::json!({
            "op": "swap_tables", "teller_id": teller,
            "request": { "branch_id": branch, "table_a": t1, "table_b": t2 }
        })
    );
    assert_eq!(resp.status(), 200, "queued swap replays");
    let seat: Option<Uuid> = sqlx::query_scalar("SELECT table_id FROM open_tickets WHERE id=$1")
        .bind(a.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(seat, Some(t2), "the party moved to the empty table");
    assert_eq!(table_status(&pool, t1).await, "free");
    assert_eq!(table_status(&pool, t2).await, "seated");

    // Replaying the same op again (a lost ack) must not bounce the party back.
    let resp = post_json!(
        app,
        t,
        "/sync/replay",
        serde_json::json!({
            "op": "swap_tables", "teller_id": teller,
            "request": { "branch_id": branch, "table_a": t1, "table_b": t2 }
        })
    );
    assert_eq!(resp.status(), 200);

    // Replay must check the op's EMBEDDED actor, not the caller's token, or a
    // revoked permission could be bypassed by queueing the op offline.
    let stranger = seed_user(&pool, org, "waiter").await;
    sqlx::query(
        "INSERT INTO permissions (user_id, resource, action, granted) \
         VALUES ($1, 'open_tickets'::permission_resource, 'update'::permission_action, false)",
    )
    .bind(stranger)
    .execute(&pool)
    .await
    .unwrap();
    let resp = post_json!(
        app,
        t,
        "/sync/replay",
        serde_json::json!({
            "op": "swap_tables", "teller_id": stranger,
            "request": { "branch_id": branch, "table_a": t1, "table_b": t2 }
        })
    );
    assert_eq!(
        resp.status(),
        403,
        "a revoked permission is not bypassable by replaying offline"
    );
}

// ── Table state (POS operational edits: status walk + zone move) ─────────────

/// `main.rs` once mounted TWO `web::scope("/floor")` — geometry from
/// `reservations::routes`, then swap/clear/transfers from `floor_ops::routes`.
/// actix hands a prefix to the FIRST scope that matches and never falls
/// through, so the second one was dead: every path in it 404'd in production
/// (405 for `/tables/swap`, which `/tables/{id}` matched) while every test
/// passed, because no test ever mounted both.
///
/// They are one scope now. This asserts the whole of it answers — geometry AND
/// operations — through the single `configure` main.rs calls.
#[sqlx::test]
async fn the_whole_floor_scope_is_reachable(pool: PgPool) {
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let teller = seed_user(&pool, org, "teller").await;
    grant_defaults(&pool).await;
    let tok = token(teller, org, UserRole::Teller);
    let table = seed_table(&pool, org, branch, None, "T1").await;

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(secret()))
            .app_data(web::Data::new(BranchEventHub::new()))
            .configure(crate::reservations::routes::configure),
    )
    .await;

    // Geometry.
    let r = get_req!(app, tok, &format!("/floor/sections?branch_id={branch}"));
    assert_eq!(r.status(), 200, "/floor/sections");
    let r = get_req!(app, tok, &format!("/floor/tables?branch_id={branch}"));
    assert_eq!(r.status(), 200, "/floor/tables");

    // Cross-table operations: the half a shadowed prefix used to eat.
    let r = get_req!(app, tok, &format!("/floor/transfers?branch_id={branch}"));
    assert_eq!(r.status(), 200, "/floor/transfers");
    let r = post_json!(
        app,
        tok,
        &format!("/floor/tables/{table}/clear"),
        serde_json::json!({})
    );
    assert_ne!(r.status(), 404, "/floor/tables/{{id}}/clear");
    let r = post_json!(
        app,
        tok,
        "/floor/tables/swap",
        serde_json::json!({ "branch_id": branch, "table_a": table, "table_b": table })
    );
    assert_ne!(r.status(), 404, "/floor/tables/swap");
    assert_ne!(r.status(), 405, "/floor/tables/swap: /tables/{{id}} swallowed it");
}

/// Bussing a table is the one floor transition a server cannot observe, and it
/// had no way home. The POS queues it as `clear_table`; `/sync/replay` had no
/// such op, so the envelope failed to deserialize, came back 400, and the POS
/// classifies a 400 as permanently dead — the op was dropped, and the next
/// floor pull put the table back to `dirty`. Tables stayed dirty forever.
///
/// The shipped v0.2.0 core sends `request: {}` (it has never had a branch to
/// put there), so that exact envelope is what this replays.
#[sqlx::test]
async fn replay_clears_a_bussed_table(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let teller = seed_user(&pool, org, "teller").await;
    let waiter = seed_user(&pool, org, "waiter").await;
    let item = seed_menu_item(&pool, org, 1000).await;
    seed_cash_method(&pool, org).await;
    let shift = open_shift_row(&pool, branch, teller).await;
    grant_defaults(&pool).await;
    for (resource, action) in [
        ("orders", "create"),
        ("payments", "create"),
        ("kitchen_orders", "read"),
        ("kitchen_orders", "update"),
    ] {
        grant(&pool, "teller", resource, action).await;
    }
    let t = token(teller, org, UserRole::Teller);
    let w = token(waiter, org, UserRole::Waiter);
    let table = seed_table(&pool, org, branch, None, "T1").await;

    // Seat a party, then settle: checkout busses the table, it does not free it.
    let tk = fire_on!(app, w, branch, item, table);
    let resp = post_json!(
        app,
        t,
        &format!("/open-tickets/{}/settle", tk.id),
        serde_json::json!({ "shift_id": shift, "payment_method": "cash" })
    );
    assert_eq!(resp.status(), 200, "settle");
    assert_eq!(table_status(&pool, table).await, "dirty");

    // The envelope the POS actually queues — empty request, no branch.
    let resp = post_json!(
        app,
        t,
        "/sync/replay",
        serde_json::json!({ "op": "clear_table", "teller_id": teller, "table_id": table, "request": {} })
    );
    assert_eq!(resp.status(), 200, "queued clear replays");
    assert_eq!(table_status(&pool, table).await, "free");

    // A lost ack replays it again; `free` -> `free` is a no-op, not a 409.
    let resp = post_json!(
        app,
        t,
        "/sync/replay",
        serde_json::json!({ "op": "clear_table", "teller_id": teller, "table_id": table, "request": {} })
    );
    assert_eq!(resp.status(), 200, "replaying a clear is idempotent");
    assert_eq!(table_status(&pool, table).await, "free");
}

/// A clear must never cross an org boundary, even though the op carries no
/// branch of its own: the table is what resolves to one.
#[sqlx::test]
async fn replay_clear_cannot_reach_another_orgs_table(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let teller = seed_user(&pool, org, "teller").await;
    grant_defaults(&pool).await;
    let t = token(teller, org, UserRole::Teller);

    let other = seed_org(&pool).await;
    let other_branch = seed_branch(&pool, other).await;
    let their_table = seed_table(&pool, other, other_branch, None, "T1").await;

    let resp = post_json!(
        app,
        t,
        "/sync/replay",
        serde_json::json!({ "op": "clear_table", "teller_id": teller, "table_id": their_table, "request": {} })
    );
    assert!(
        matches!(resp.status().as_u16(), 403 | 404),
        "cross-org clear rejected, got {}",
        resp.status()
    );
}
