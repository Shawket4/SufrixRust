use actix_web::{App, test, web};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::jwt::{JwtSecret, create_token};
use crate::kitchen::KitchenTicketView;
use crate::models::UserRole;
use crate::orders::handlers::Order;
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
async fn open_shift_row(pool: &PgPool, branch: Uuid, teller: Uuid) -> Uuid {
    sqlx::query_scalar("INSERT INTO shifts (branch_id, teller_id, status, opening_cash) VALUES ($1,$2,'open',0) RETURNING id")
        .bind(branch).bind(teller).fetch_one(pool).await.unwrap()
}
async fn grant(pool: &PgPool, role: &str, resource: &str, action: &str) {
    sqlx::query(
        "INSERT INTO role_permissions (role, resource, action, granted) \
         VALUES ($1::user_role, $2::permission_resource, $3::permission_action, true) ON CONFLICT DO NOTHING",
    )
    .bind(role).bind(resource).bind(action).execute(pool).await.unwrap();
}

macro_rules! app {
    ($pool:expr) => {
        test::init_service(
            App::new()
                .app_data(web::Data::new($pool.clone()))
                .app_data(web::Data::new(secret()))
                .app_data(web::Data::new(BranchEventHub::new()))
                .configure(crate::tickets::routes::configure)
                .configure(crate::kitchen::routes::configure)
                .configure(crate::sync::routes::configure),
        )
        .await
    };
}

/// Full chain: a waiter fires a dine-in ticket → it lands on the KDS → a cook
/// bumps it → the ticket goes ready → a cashier settles it into a paid dine-in
/// order in THEIR shift. Then a double-settle is a clean conflict.
#[sqlx::test]
async fn waiter_fire_bump_settle_end_to_end(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let item = seed_menu_item(&pool, org, 1000).await;
    seed_cash_method(&pool, org).await;
    let waiter = seed_user(&pool, org, "waiter").await;
    let teller = seed_user(&pool, org, "teller").await;
    let shift = open_shift_row(&pool, branch, teller).await;

    grant(&pool, "waiter", "open_tickets", "create").await;
    grant(&pool, "waiter", "open_tickets", "read").await;
    grant(&pool, "teller", "open_tickets", "read").await;
    grant(&pool, "teller", "open_tickets", "update").await;
    grant(&pool, "teller", "orders", "create").await;
    grant(&pool, "teller", "payments", "create").await;
    grant(&pool, "teller", "kitchen_orders", "read").await;
    grant(&pool, "teller", "kitchen_orders", "update").await;

    let waiter_t = token(waiter, org, UserRole::Waiter);
    let teller_t = token(teller, org, UserRole::Teller);

    // 1. Waiter fires a ticket (2× a 1000-piastre item).
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/open-tickets")
            .insert_header(("Authorization", format!("Bearer {waiter_t}")))
            .set_json(&serde_json::json!({
                "branch_id": branch,
                "items": [{ "menu_item_id": item, "quantity": 2 }]
            }))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 201, "waiter fires a ticket");
    let view: OpenTicketView = test::read_body_json(resp).await;
    assert_eq!(view.status, "open");
    assert_eq!(view.items.len(), 1);
    assert_eq!(view.subtotal, 2000);
    let ticket_id = view.id;
    assert_eq!(
        view.opened_by, waiter,
        "waiter is preserved as the order-taker"
    );

    // A kitchen ticket was emitted for this open ticket.
    let kt: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM kitchen_tickets WHERE source_type='open_ticket' AND source_id=$1",
    )
    .bind(ticket_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(kt, 1);

    // 2. The line shows on the KDS feed.
    let feed_resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/kitchen/orders?branch_id={branch}"))
            .insert_header(("Authorization", format!("Bearer {teller_t}")))
            .to_request(),
    )
    .await;
    assert_eq!(feed_resp.status(), 200);
    let feed: Vec<KitchenTicketView> = test::read_body_json(feed_resp).await;
    assert_eq!(feed.len(), 1, "one outstanding kitchen ticket");
    let kitchen_item_id = feed[0].items[0].id;

    // 3. Bump it → the open ticket becomes ready.
    let bump = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/kitchen/items/{kitchen_item_id}/bump"))
            .insert_header(("Authorization", format!("Bearer {teller_t}")))
            .to_request(),
    )
    .await;
    assert_eq!(bump.status(), 204);
    let status: String = sqlx::query_scalar("SELECT status::text FROM open_tickets WHERE id=$1")
        .bind(ticket_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "ready", "all lines bumped → ticket ready");

    // 4. Cashier settles into THEIR shift → a paid dine-in order.
    let settle = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/open-tickets/{ticket_id}/settle"))
            .insert_header(("Authorization", format!("Bearer {teller_t}")))
            .set_json(&serde_json::json!({ "shift_id": shift, "payment_method": "cash" }))
            .to_request(),
    )
    .await;
    assert_eq!(settle.status(), 200, "cashier settles");
    let order: Order = test::read_body_json(settle).await;
    assert_eq!(order.order_type, "dine_in");
    assert_eq!(order.subtotal, 2000, "2 × 1000 items");
    assert!(order.total_amount >= 2000, "total includes any org tax");
    assert_eq!(
        order.shift_id, shift,
        "lands in the settling cashier's shift"
    );
    assert_eq!(order.teller_id, teller);
    // The paid order is stamped with the WAITER who opened the ticket (not the
    // settling cashier) — this is what the dashboard segments/exports by. Direct
    // POS sales leave it null; here it must resolve to the ticket's opener.
    assert_eq!(
        order.waiter_id,
        Some(waiter),
        "settled order carries the ticket's waiter (opened_by)"
    );
    assert!(
        order.waiter_name.is_some(),
        "waiter_name is joined from users for the dashboard"
    );

    let (st, oid): (String, Option<Uuid>) =
        sqlx::query_as("SELECT status::text, order_id FROM open_tickets WHERE id=$1")
            .bind(ticket_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(st, "settled");
    assert_eq!(oid, Some(order.id));

    // 5. Double settle is a clean conflict.
    let again = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/open-tickets/{ticket_id}/settle"))
            .insert_header(("Authorization", format!("Bearer {teller_t}")))
            .set_json(&serde_json::json!({ "shift_id": shift, "payment_method": "cash" }))
            .to_request(),
    )
    .await;
    assert_eq!(again.status(), 409, "already settled");
}

/// Firing requires the branch to be operating (a till open) — no shift → 409.
#[sqlx::test]
async fn fire_requires_open_shift_at_branch(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let item = seed_menu_item(&pool, org, 500).await;
    let waiter = seed_user(&pool, org, "waiter").await;
    grant(&pool, "waiter", "open_tickets", "create").await;
    let waiter_t = token(waiter, org, UserRole::Waiter);

    let resp = test::call_service(&app, test::TestRequest::post()
        .uri("/open-tickets")
        .insert_header(("Authorization", format!("Bearer {waiter_t}")))
        .set_json(&serde_json::json!({ "branch_id": branch, "items": [{ "menu_item_id": item, "quantity": 1 }] }))
        .to_request()).await;
    assert_eq!(resp.status(), 409, "no open shift → cannot fire");
}

/// Offline replay (WS2g): a queued waiter fire → round → cashier settle flushes
/// through `/sync/replay`, attributed to the embedded actor (not the bearer), and
/// each op is idempotent on its client-minted key — a lost-ack retry produces no
/// duplicate ticket / round / order.
#[sqlx::test]
async fn replay_fire_round_settle_idempotent_and_attributed(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let item = seed_menu_item(&pool, org, 1000).await;
    seed_cash_method(&pool, org).await;
    let waiter = seed_user(&pool, org, "waiter").await;
    let teller = seed_user(&pool, org, "teller").await;
    let shift = open_shift_row(&pool, branch, teller).await;
    // The device flushing the backlog signs in as a teller; replay authorizes via
    // each op's EMBEDDED actor — and now ENFORCES that actor's permissions (audit
    // #12), exactly like the live routes — so grant each embedded actor the role
    // default its replayed op requires.
    grant(&pool, "waiter", "open_tickets", "create").await; // fire
    grant(&pool, "waiter", "open_tickets", "update").await; // add round
    grant(&pool, "teller", "open_tickets", "update").await; // settle
    grant(&pool, "teller", "orders", "create").await; // settle materializes the order
    let bearer = token(teller, org, UserRole::Teller);

    let ticket_idem = Uuid::new_v4();
    let round1_idem = Uuid::new_v4();
    let fire = serde_json::json!({
        "op": "fire_open_ticket",
        "teller_id": waiter,
        "request": {
            "branch_id": branch,
            "idempotency_key": ticket_idem,
            "round_idempotency_key": round1_idem,
            "items": [{ "menu_item_id": item, "quantity": 2 }]
        }
    });
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/sync/replay")
            .insert_header(("Authorization", format!("Bearer {bearer}")))
            .set_json(&fire)
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 201, "replayed fire creates the ticket");
    let view: OpenTicketView = test::read_body_json(resp).await;
    let ticket_id = view.id;
    assert_eq!(
        view.opened_by, waiter,
        "attributed to the embedded waiter, not the bearer"
    );
    assert_eq!(view.subtotal, 2000);

    // Lost-ack retry of the SAME fire → dedups (no second ticket).
    let again = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/sync/replay")
            .insert_header(("Authorization", format!("Bearer {bearer}")))
            .set_json(&fire)
            .to_request(),
    )
    .await;
    assert!(again.status().is_success(), "replayed fire is idempotent");
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM open_tickets WHERE idempotency_key=$1")
        .bind(ticket_idem)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 1, "exactly one ticket for the idempotency key");

    // Replay a second round (its own key), twice → dedups.
    let round2_idem = Uuid::new_v4();
    let round = serde_json::json!({
        "op": "add_ticket_round",
        "teller_id": waiter,
        "ticket_id": ticket_id,
        "request": { "idempotency_key": round2_idem, "items": [{ "menu_item_id": item, "quantity": 1 }] }
    });
    for _ in 0..2 {
        let r = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/sync/replay")
                .insert_header(("Authorization", format!("Bearer {bearer}")))
                .set_json(&round)
                .to_request(),
        )
        .await;
        assert!(r.status().is_success(), "replayed round ok");
    }
    let rounds: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM open_ticket_rounds WHERE open_ticket_id=$1")
            .bind(ticket_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(rounds, 2, "round 1 (fire) + round 2, no duplicate");

    // Replay the cashier settle, twice → one paid order in the cashier's shift.
    let settle = serde_json::json!({
        "op": "settle_open_ticket",
        "teller_id": teller,
        "ticket_id": ticket_id,
        "request": { "shift_id": shift, "payment_method": "cash" }
    });
    let s1 = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/sync/replay")
            .insert_header(("Authorization", format!("Bearer {bearer}")))
            .set_json(&settle)
            .to_request(),
    )
    .await;
    assert_eq!(s1.status(), 200, "replayed settle ok");
    let order: Order = test::read_body_json(s1).await;
    assert_eq!(order.shift_id, shift, "lands in the cashier's shift");
    assert_eq!(order.teller_id, teller);
    assert_eq!(order.subtotal, 3000, "2×1000 + 1×1000 across both rounds");

    let s2 = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/sync/replay")
            .insert_header(("Authorization", format!("Bearer {bearer}")))
            .set_json(&settle)
            .to_request(),
    )
    .await;
    assert_eq!(
        s2.status(),
        200,
        "replayed settle is idempotent (lost ack), not a 409"
    );
    let order2: Order = test::read_body_json(s2).await;
    assert_eq!(order2.id, order.id, "same paid order returned");
    let orders: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM orders WHERE idempotency_key=$1")
        .bind(ticket_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(orders, 1, "exactly one order materialized");
}

/// Replay attribution-safety: an op can only be replayed under a role that could
/// have produced it live, and only for an actor in the bearer's org.
#[sqlx::test]
async fn replay_rejects_wrong_actor_role_or_org(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let item = seed_menu_item(&pool, org, 500).await;
    let waiter = seed_user(&pool, org, "waiter").await;
    let teller = seed_user(&pool, org, "teller").await;
    let bearer = token(teller, org, UserRole::Teller);

    // A TELLER cannot be the actor of a fire (firing is waiter-only).
    let fire_by_teller = serde_json::json!({
        "op": "fire_open_ticket",
        "teller_id": teller,
        "request": { "branch_id": branch, "items": [{ "menu_item_id": item, "quantity": 1 }] }
    });
    let r = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/sync/replay")
            .insert_header(("Authorization", format!("Bearer {bearer}")))
            .set_json(&fire_by_teller)
            .to_request(),
    )
    .await;
    assert_eq!(r.status(), 403, "teller may not fire a ticket");

    // A WAITER cannot be the actor of a settle (settling is teller-only).
    let settle_by_waiter = serde_json::json!({
        "op": "settle_open_ticket",
        "teller_id": waiter,
        "ticket_id": Uuid::new_v4(),
        "request": { "shift_id": Uuid::new_v4(), "payment_method": "cash" }
    });
    let r = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/sync/replay")
            .insert_header(("Authorization", format!("Bearer {bearer}")))
            .set_json(&settle_by_waiter)
            .to_request(),
    )
    .await;
    assert_eq!(r.status(), 403, "waiter may not settle");

    // An actor from a DIFFERENT org is rejected before any dispatch.
    let other_org = seed_org(&pool).await;
    let other_waiter = seed_user(&pool, other_org, "waiter").await;
    let cross = serde_json::json!({
        "op": "fire_open_ticket",
        "teller_id": other_waiter,
        "request": { "branch_id": branch, "items": [{ "menu_item_id": item, "quantity": 1 }] }
    });
    let r = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/sync/replay")
            .insert_header(("Authorization", format!("Bearer {bearer}")))
            .set_json(&cross)
            .to_request(),
    )
    .await;
    assert_eq!(r.status(), 403, "actor from another org rejected");
}

/// Phase E offline display: a fire derives its kitchen-ticket + line ids from the
/// round's CLIENT idempotency key, so a device that fired offline predicted the SAME
/// ids (its KDS projection + a later bump reconcile against the server row on sync).
#[sqlx::test]
async fn fire_derives_stable_kitchen_ids_from_round_key(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let item = seed_menu_item(&pool, org, 1000).await;
    let waiter = seed_user(&pool, org, "waiter").await;
    let teller = seed_user(&pool, org, "teller").await;
    let _shift = open_shift_row(&pool, branch, teller).await;
    grant(&pool, "waiter", "open_tickets", "create").await;
    let waiter_t = token(waiter, org, UserRole::Waiter);

    let round_idem = Uuid::new_v4();
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/open-tickets")
            .insert_header(("Authorization", format!("Bearer {waiter_t}")))
            .set_json(&serde_json::json!({
                "branch_id": branch,
                "idempotency_key": Uuid::new_v4(),
                "round_idempotency_key": round_idem,
                "items": [{ "menu_item_id": item, "quantity": 1 }]
            }))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 201);
    let ticket_id = test::read_body_json::<OpenTicketView, _>(resp).await.id;

    let kt = crate::kitchen::derive_kitchen_ticket_id(round_idem);
    let (got_kt, got_item): (Uuid, Uuid) = sqlx::query_as(
        "SELECT kt.id, kti.id FROM kitchen_tickets kt \
         JOIN kitchen_ticket_items kti ON kti.kitchen_ticket_id = kt.id \
         WHERE kt.source_type='open_ticket' AND kt.source_id=$1",
    )
    .bind(ticket_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(got_kt, kt, "kitchen ticket id derived from the round key");
    assert_eq!(
        got_item,
        crate::kitchen::derive_kitchen_item_id(kt, 0),
        "line 0 id derived"
    );
}

/// Offline bump replay (Phase E step 2): a KDS bump queued while offline flushes
/// through `/sync/replay`, attributed to the embedded KITCHEN actor (not the
/// bearer), idempotent on the `item_id`. A bump for a gone/unknown line replays as
/// a clean 204 no-op so it can never wedge the FIFO drain. A waiter may not bump.
#[sqlx::test]
async fn replay_bump_idempotent_and_attributed(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let item = seed_menu_item(&pool, org, 1000).await;
    let waiter = seed_user(&pool, org, "waiter").await;
    let teller = seed_user(&pool, org, "teller").await;
    let kitchen = seed_user(&pool, org, "kitchen").await;
    let _shift = open_shift_row(&pool, branch, teller).await;
    grant(&pool, "waiter", "open_tickets", "create").await;
    // Replay now enforces the embedded actor's permissions (audit #12). The bump
    // lands under the kitchen user, so grant it kitchen_orders/update.
    grant(&pool, "kitchen", "kitchen_orders", "update").await;
    let waiter_t = token(waiter, org, UserRole::Waiter);
    let bearer = token(teller, org, UserRole::Teller);

    // Waiter fires a ticket live → a kitchen ticket + one line.
    let resp = test::call_service(&app, test::TestRequest::post()
        .uri("/open-tickets")
        .insert_header(("Authorization", format!("Bearer {waiter_t}")))
        .set_json(&serde_json::json!({ "branch_id": branch, "items": [{ "menu_item_id": item, "quantity": 1 }] }))
        .to_request()).await;
    assert_eq!(resp.status(), 201);
    let ticket_id = test::read_body_json::<OpenTicketView, _>(resp).await.id;

    // The (server-minted) kitchen line id the KDS would have bumped.
    let kitchen_item_id: Uuid = sqlx::query_scalar(
        "SELECT kti.id FROM kitchen_ticket_items kti \
         JOIN kitchen_tickets kt ON kt.id = kti.kitchen_ticket_id \
         WHERE kt.source_type='open_ticket' AND kt.source_id=$1",
    )
    .bind(ticket_id)
    .fetch_one(&pool)
    .await
    .unwrap();

    // Replay the bump attributed to the KITCHEN device.
    let bump = serde_json::json!({ "op": "bump_kitchen_item", "teller_id": kitchen, "item_id": kitchen_item_id });
    let r = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/sync/replay")
            .insert_header(("Authorization", format!("Bearer {bearer}")))
            .set_json(&bump)
            .to_request(),
    )
    .await;
    assert_eq!(r.status(), 204, "replayed bump ok");
    let (status, bumped_by): (String, Option<Uuid>) = sqlx::query_as(
        "SELECT ot.status::text, kti.bumped_by FROM open_tickets ot, kitchen_ticket_items kti \
         WHERE ot.id=$1 AND kti.id=$2",
    )
    .bind(ticket_id)
    .bind(kitchen_item_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(status, "ready", "only line bumped → ticket ready");
    assert_eq!(
        bumped_by,
        Some(kitchen),
        "attributed to the embedded kitchen actor, not the bearer"
    );

    // Idempotent: re-bump the same line → 204 no-op (still ready, still kitchen).
    let r2 = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/sync/replay")
            .insert_header(("Authorization", format!("Bearer {bearer}")))
            .set_json(&bump)
            .to_request(),
    )
    .await;
    assert_eq!(r2.status(), 204, "re-bump is an idempotent no-op");

    // A bump for an UNKNOWN line replays as a clean no-op (never wedges the drain).
    let ghost = serde_json::json!({ "op": "bump_kitchen_item", "teller_id": kitchen, "item_id": Uuid::new_v4() });
    let rg = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/sync/replay")
            .insert_header(("Authorization", format!("Bearer {bearer}")))
            .set_json(&ghost)
            .to_request(),
    )
    .await;
    assert_eq!(
        rg.status(),
        204,
        "bump for a gone line is a no-op, not an error"
    );

    // A WAITER may not be the actor of a bump (bump is kitchen/teller only).
    let bump_by_waiter = serde_json::json!({ "op": "bump_kitchen_item", "teller_id": waiter, "item_id": kitchen_item_id });
    let rw = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/sync/replay")
            .insert_header(("Authorization", format!("Bearer {bearer}")))
            .set_json(&bump_by_waiter)
            .to_request(),
    )
    .await;
    assert_eq!(rw.status(), 403, "waiter may not bump");

    // Unbump replay → the line reopens, the ticket falls back to open.
    let unbump = serde_json::json!({ "op": "unbump_kitchen_item", "teller_id": kitchen, "item_id": kitchen_item_id });
    let ru = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/sync/replay")
            .insert_header(("Authorization", format!("Bearer {bearer}")))
            .set_json(&unbump)
            .to_request(),
    )
    .await;
    assert_eq!(ru.status(), 204, "replayed unbump ok");
    let status2: String = sqlx::query_scalar("SELECT status::text FROM open_tickets WHERE id=$1")
        .bind(ticket_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status2, "open", "unbumped line → ticket no longer ready");
}

/// Regression — the "nothing happens on the teller side" bug. A waiter device
/// fires offline-first, so the fire reaches the backend via `/sync/replay` (NOT
/// the direct `POST /open-tickets`), even when the waiter is online. The replay
/// path must STILL publish to the branch bus, or a connected teller/KDS gets no
/// live push (and no ping/notification) until it manually reloads. We subscribe
/// to the branch bus AS a connected teller would and assert the fire emits
/// `ticket.fired` (+ `kitchen.fired` for the KDS).
#[sqlx::test]
async fn replay_fire_publishes_realtime(pool: PgPool) {
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let item = seed_menu_item(&pool, org, 1000).await;
    let waiter = seed_user(&pool, org, "waiter").await;
    grant(&pool, "waiter", "open_tickets", "create").await;
    grant(&pool, "waiter", "open_tickets", "read").await;

    // Hold a hub handle to subscribe as a connected teller; the app shares the
    // SAME hub (Arc inside), so a publish from the replay handler reaches `rx`.
    let hub = BranchEventHub::new();
    let mut rx = hub.subscribe(branch);
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(secret()))
            .app_data(web::Data::new(hub.clone()))
            .configure(crate::tickets::routes::configure)
            .configure(crate::kitchen::routes::configure)
            .configure(crate::sync::routes::configure),
    )
    .await;

    let waiter_t = token(waiter, org, UserRole::Waiter);
    let fire = serde_json::json!({
        "op": "fire_open_ticket",
        "teller_id": waiter,
        "request": {
            "branch_id": branch,
            "items": [{ "menu_item_id": item, "quantity": 1 }],
            "idempotency_key": Uuid::new_v4(),
        }
    });
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/sync/replay")
            .insert_header(("Authorization", format!("Bearer {waiter_t}")))
            .set_json(&fire)
            .to_request(),
    )
    .await;
    assert!(
        resp.status().is_success(),
        "replayed fire ok: {}",
        resp.status()
    );

    // Drain what the branch bus received. Before the fix this was empty.
    let mut kinds = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        kinds.push(ev.event_type);
    }
    assert!(
        kinds.iter().any(|k| k == "ticket.fired"),
        "replayed fire must publish ticket.fired (got {kinds:?})"
    );
    assert!(
        kinds.iter().any(|k| k == "kitchen.fired"),
        "replayed fire must publish kitchen.fired for the KDS (got {kinds:?})"
    );
}

/// A reward on a ticket names its LINE, and the server turns that into the
/// position the line lands at.
///
/// The ordering (`round_number, created_at`) is decided in the settle handler,
/// so only the server can know it. A till that guessed an index would take the
/// wrong item off the bill — silently, and in the customer's favour or not.
/// This pins the translation with a ticket whose SECOND round holds the reward,
/// which is exactly where a naive "index = position in the round" would break.
#[sqlx::test]
async fn a_reward_on_a_ticket_covers_the_line_it_names(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let cheap = seed_menu_item(&pool, org, 1_000).await;
    let latte = seed_menu_item(&pool, org, 5_000).await;
    seed_cash_method(&pool, org).await;
    let waiter = seed_user(&pool, org, "waiter").await;
    let teller = seed_user(&pool, org, "teller").await;
    let shift = open_shift_row(&pool, branch, teller).await;

    for (role, res, act) in [
        ("waiter", "open_tickets", "create"),
        ("waiter", "open_tickets", "update"),
        ("teller", "open_tickets", "update"),
        ("teller", "orders", "create"),
        ("teller", "payments", "create"),
        ("teller", "loyalty", "update"),
    ] {
        grant(&pool, role, res, act).await;
    }
    let waiter_t = token(waiter, org, UserRole::Waiter);
    let teller_t = token(teller, org, UserRole::Teller);

    // A stamp card, and the latte is what a stamp buys.
    sqlx::query(
        "INSERT INTO loyalty_settings (org_id, branch_id, enabled, mode, default_reward_cost) \
         VALUES ($1, NULL, true, 'visits', 5)",
    )
    .bind(org)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO loyalty_reward_items (org_id, menu_item_id, cost_currency, cost_amount) \
         VALUES ($1,$2,'visits',5)",
    )
    .bind(org)
    .bind(latte)
    .execute(&pool)
    .await
    .unwrap();
    let member: Uuid = sqlx::query_scalar(
        "INSERT INTO loyalty_customers (org_id, phone, name, member_token) \
         VALUES ($1,'201000000001','Ali','Mticketline000000001') RETURNING id",
    )
    .bind(org)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO loyalty_transactions (org_id, customer_id, branch_id, kind, currency, points) \
         VALUES ($1,$2,$3,'adjust','visits',5)",
    )
    .bind(org)
    .bind(member)
    .bind(branch)
    .execute(&pool)
    .await
    .unwrap();

    // Round 1: the cheap item. Round 2: the latte.
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/open-tickets")
            .insert_header(("Authorization", format!("Bearer {waiter_t}")))
            .set_json(&serde_json::json!({
                "branch_id": branch,
                "items": [{ "menu_item_id": cheap, "quantity": 1 }]
            }))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 201);
    let view: OpenTicketView = test::read_body_json(resp).await;
    let ticket_id = view.id;

    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/open-tickets/{ticket_id}/rounds"))
            .insert_header(("Authorization", format!("Bearer {waiter_t}")))
            .set_json(&serde_json::json!({
                "items": [{ "menu_item_id": latte, "quantity": 1 }]
            }))
            .to_request(),
    )
    .await;
    assert!(resp.status().is_success(), "second round: {}", resp.status());

    // The latte's LINE id — what the till would send.
    let latte_line: Uuid = sqlx::query_scalar(
        "SELECT oti.id FROM open_ticket_items oti \
           JOIN open_ticket_rounds r ON r.id = oti.round_id \
          WHERE oti.open_ticket_id = $1 AND oti.menu_item_id = $2",
    )
    .bind(ticket_id)
    .bind(latte)
    .fetch_one(&pool)
    .await
    .unwrap();

    let settle = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/open-tickets/{ticket_id}/settle"))
            .insert_header(("Authorization", format!("Bearer {teller_t}")))
            .set_json(&serde_json::json!({
                "shift_id": shift,
                "payment_method": "cash",
                "loyalty_customer_id": member,
                "loyalty_redemptions": [{ "ticket_line_id": latte_line }]
            }))
            .to_request(),
    )
    .await;
    assert_eq!(settle.status(), 200, "cashier settles with a reward");
    let order: Order = test::read_body_json(settle).await;

    // The LATTE went free, not the cheap item that sits first in the list.
    assert_eq!(order.subtotal, 1_000, "only the cheap line is charged");
    let free: Vec<(String, i32, bool)> = sqlx::query_as(
        "SELECT item_name, line_total, is_reward FROM order_items \
          WHERE order_id = $1 ORDER BY line_total",
    )
    .bind(order.id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(
        free.iter().any(|(_, total, reward)| *total == 0 && *reward),
        "the covered line is free and marked: {free:?}"
    );
    let balance: i32 =
        sqlx::query_scalar("SELECT visits_balance FROM loyalty_customers WHERE id = $1")
            .bind(member)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(balance, 0, "five stamps spent");
}

/// A positional index on a ticket settle is refused outright.
///
/// The till cannot know the server's ordering, so an index it supplied would be
/// a guess. Rejecting is the only safe answer — the alternative is giving away
/// whichever item happened to land there.
#[sqlx::test]
async fn a_ticket_reward_without_a_line_id_is_refused(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let item = seed_menu_item(&pool, org, 1_000).await;
    seed_cash_method(&pool, org).await;
    let waiter = seed_user(&pool, org, "waiter").await;
    let teller = seed_user(&pool, org, "teller").await;
    let shift = open_shift_row(&pool, branch, teller).await;
    for (role, res, act) in [
        ("waiter", "open_tickets", "create"),
        ("teller", "open_tickets", "update"),
        ("teller", "orders", "create"),
        ("teller", "payments", "create"),
        ("teller", "loyalty", "update"),
    ] {
        grant(&pool, role, res, act).await;
    }
    let waiter_t = token(waiter, org, UserRole::Waiter);
    let teller_t = token(teller, org, UserRole::Teller);

    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/open-tickets")
            .insert_header(("Authorization", format!("Bearer {waiter_t}")))
            .set_json(&serde_json::json!({
                "branch_id": branch,
                "items": [{ "menu_item_id": item, "quantity": 1 }]
            }))
            .to_request(),
    )
    .await;
    let view: OpenTicketView = test::read_body_json(resp).await;

    let settle = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/open-tickets/{}/settle", view.id))
            .insert_header(("Authorization", format!("Bearer {teller_t}")))
            .set_json(&serde_json::json!({
                "shift_id": shift,
                "payment_method": "cash",
                "loyalty_redemptions": [{ "item_index": 0 }]
            }))
            .to_request(),
    )
    .await;
    assert_eq!(settle.status(), 400, "a guessed index is refused");
}
