use actix_web::{App, test, web};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::jwt::JwtSecret;
use crate::models::UserRole;
use crate::permissions::handlers::{
    Permission, PermissionMatrix, RolePermission, UpsertPermissionRequest,
    UpsertRolePermissionRequest,
};
use crate::permissions::routes;

fn get_secret() -> JwtSecret {
    JwtSecret("secret".to_string())
}

fn generate_token(user_id: Uuid, org_id: Option<Uuid>, role: UserRole) -> String {
    crate::auth::jwt::create_token(&get_secret(), user_id, org_id, role, None, 24).unwrap()
}

async fn seed_org(pool: &PgPool) -> Uuid {
    let org_id = Uuid::new_v4();
    let name = format!("Test Org {}", org_id);
    let slug = format!("test-org-{}", org_id);
    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, $2, $3)")
        .bind(org_id)
        .bind(&name)
        .bind(&slug)
        .execute(pool)
        .await
        .unwrap();
    org_id
}

async fn seed_user(pool: &PgPool, org_id: Uuid, name: &str, role: UserRole, email: &str) -> Uuid {
    let user_id = Uuid::new_v4();
    let role_str = match role {
        UserRole::SuperAdmin => "super_admin",
        UserRole::OrgAdmin => "org_admin",
        UserRole::BranchManager => "branch_manager",
        UserRole::Teller => "teller",
        UserRole::Waiter => "waiter",
        UserRole::Kitchen => "kitchen",
    };
    sqlx::query(
        "INSERT INTO users (id, org_id, name, role, email, password_hash) VALUES ($1, $2, $3, $4::user_role, $5, 'h')"
    )
    .bind(user_id)
    .bind(org_id)
    .bind(name)
    .bind(role_str)
    .bind(email)
    .execute(pool)
    .await
    .unwrap();
    user_id
}

async fn seed_super_admin(pool: &PgPool, name: &str, email: &str) -> Uuid {
    let user_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, org_id, name, role, email, password_hash) VALUES ($1, NULL, $2, 'super_admin'::user_role, $3, 'h')"
    )
    .bind(user_id)
    .bind(name)
    .bind(email)
    .execute(pool)
    .await
    .unwrap();
    user_id
}

#[sqlx::test]
async fn test_get_role_permissions_success(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    crate::permissions::seeder::seed_role_permissions(&pool)
        .await
        .unwrap();

    let org_id = seed_org(&pool).await;
    let user_id = seed_user(
        &pool,
        org_id,
        "Admin User",
        UserRole::OrgAdmin,
        "admin@t.com",
    )
    .await;
    let token = generate_token(user_id, Some(org_id), UserRole::OrgAdmin);

    let req = test::TestRequest::get()
        .uri("/permissions/roles")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    let perms: Vec<RolePermission> = test::read_body_json(resp).await;
    assert!(!perms.is_empty());
    // org_admin should have full access, check that at least orgs-create is there
    let has_orgs_create = perms.iter().any(|p| {
        p.role == "org_admin" && p.resource == "orgs" && p.action == "create" && p.granted
    });
    assert!(has_orgs_create);
}

#[sqlx::test]
async fn test_get_role_permissions_forbidden(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    crate::permissions::seeder::seed_role_permissions(&pool)
        .await
        .unwrap();

    let org_id = seed_org(&pool).await;
    let user_id = seed_user(
        &pool,
        org_id,
        "Teller User",
        UserRole::Teller,
        "teller@t.com",
    )
    .await;
    let token = generate_token(user_id, Some(org_id), UserRole::Teller);

    let req = test::TestRequest::get()
        .uri("/permissions/roles")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::FORBIDDEN);
}

#[sqlx::test]
async fn test_upsert_role_permission_success(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    let admin_id = seed_super_admin(&pool, "Super Admin", "super@t.com").await;
    let token = generate_token(admin_id, None, UserRole::SuperAdmin);

    let req = test::TestRequest::put()
        .uri("/permissions/roles")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&UpsertRolePermissionRequest {
            role: "branch_manager".to_string(),
            resource: "permissions".to_string(),
            action: "create".to_string(),
            granted: true,
        })
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    let role_perm: RolePermission = test::read_body_json(resp).await;
    assert_eq!(role_perm.role, "branch_manager");
    assert_eq!(role_perm.resource, "permissions");
    assert_eq!(role_perm.action, "create");
    assert!(role_perm.granted);
}

#[sqlx::test]
async fn test_upsert_role_permission_forbidden(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    let org_id = seed_org(&pool).await;
    let admin_id = seed_user(
        &pool,
        org_id,
        "Admin User",
        UserRole::OrgAdmin,
        "admin@t.com",
    )
    .await;
    let token = generate_token(admin_id, Some(org_id), UserRole::OrgAdmin);

    let req = test::TestRequest::put()
        .uri("/permissions/roles")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&UpsertRolePermissionRequest {
            role: "branch_manager".to_string(),
            resource: "permissions".to_string(),
            action: "create".to_string(),
            granted: true,
        })
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::FORBIDDEN);
}

#[sqlx::test]
async fn test_get_permission_matrix_success(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    crate::permissions::seeder::seed_role_permissions(&pool)
        .await
        .unwrap();

    let org_id = seed_org(&pool).await;
    let admin_id = seed_user(
        &pool,
        org_id,
        "Admin User",
        UserRole::OrgAdmin,
        "admin@t.com",
    )
    .await;
    let target_user_id = seed_user(
        &pool,
        org_id,
        "Teller User",
        UserRole::Teller,
        "teller@t.com",
    )
    .await;
    let token = generate_token(admin_id, Some(org_id), UserRole::OrgAdmin);

    // Insert user override
    sqlx::query(
        "INSERT INTO permissions (user_id, resource, action, granted) VALUES ($1, 'orders'::permission_resource, 'delete'::permission_action, true)"
    )
    .bind(target_user_id)
    .execute(&pool)
    .await
    .unwrap();

    let req = test::TestRequest::get()
        .uri(&format!("/permissions/matrix/{}", target_user_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    let matrix: Vec<PermissionMatrix> = test::read_body_json(resp).await;
    assert!(!matrix.is_empty());

    // Check custom override
    let orders_delete = matrix
        .iter()
        .find(|m| m.resource == "orders" && m.action == "delete")
        .unwrap();
    assert_eq!(orders_delete.user_override, Some(true));
    assert!(orders_delete.effective);

    // Check standard default
    let orders_create = matrix
        .iter()
        .find(|m| m.resource == "orders" && m.action == "create")
        .unwrap();
    assert_eq!(orders_create.role_default, Some(true));
    assert_eq!(orders_create.user_override, None);
    assert!(orders_create.effective);
}

#[sqlx::test]
async fn test_get_permission_matrix_different_org(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    crate::permissions::seeder::seed_role_permissions(&pool)
        .await
        .unwrap();

    let org_a = seed_org(&pool).await;
    let org_b = seed_org(&pool).await;

    let admin_a = seed_user(&pool, org_a, "Admin A", UserRole::OrgAdmin, "admina@t.com").await;
    let teller_b = seed_user(&pool, org_b, "Teller B", UserRole::Teller, "tellerb@t.com").await;
    let token = generate_token(admin_a, Some(org_a), UserRole::OrgAdmin);

    let req = test::TestRequest::get()
        .uri(&format!("/permissions/matrix/{}", teller_b))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::FORBIDDEN);
}

#[sqlx::test]
async fn test_get_permission_matrix_user_not_found(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    crate::permissions::seeder::seed_role_permissions(&pool)
        .await
        .unwrap();

    let admin_id = seed_super_admin(&pool, "Super Admin", "super@t.com").await;
    let token = generate_token(admin_id, None, UserRole::SuperAdmin);

    let random_uuid = Uuid::new_v4();

    let req = test::TestRequest::get()
        .uri(&format!("/permissions/matrix/{}", random_uuid))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::NOT_FOUND);
}

#[sqlx::test]
async fn test_get_user_permissions_success(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    crate::permissions::seeder::seed_role_permissions(&pool)
        .await
        .unwrap();

    let org_id = seed_org(&pool).await;
    let admin_id = seed_user(
        &pool,
        org_id,
        "Admin User",
        UserRole::OrgAdmin,
        "admin@t.com",
    )
    .await;
    let target_user_id = seed_user(
        &pool,
        org_id,
        "Teller User",
        UserRole::Teller,
        "teller@t.com",
    )
    .await;
    let token = generate_token(admin_id, Some(org_id), UserRole::OrgAdmin);

    // Seed override
    sqlx::query(
        "INSERT INTO permissions (user_id, resource, action, granted) VALUES ($1, 'orders'::permission_resource, 'delete'::permission_action, true)"
    )
    .bind(target_user_id)
    .execute(&pool)
    .await
    .unwrap();

    let req = test::TestRequest::get()
        .uri(&format!("/permissions/user/{}", target_user_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    let perms: Vec<Permission> = test::read_body_json(resp).await;
    assert_eq!(perms.len(), 1);
    assert_eq!(perms[0].user_id, target_user_id);
    assert_eq!(perms[0].resource, "orders");
    assert_eq!(perms[0].action, "delete");
    assert!(perms[0].granted);
}

#[sqlx::test]
async fn test_get_user_permissions_different_org(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    crate::permissions::seeder::seed_role_permissions(&pool)
        .await
        .unwrap();

    let org_a = seed_org(&pool).await;
    let org_b = seed_org(&pool).await;

    let admin_a = seed_user(&pool, org_a, "Admin A", UserRole::OrgAdmin, "admina@t.com").await;
    let teller_b = seed_user(&pool, org_b, "Teller B", UserRole::Teller, "tellerb@t.com").await;
    let token = generate_token(admin_a, Some(org_a), UserRole::OrgAdmin);

    let req = test::TestRequest::get()
        .uri(&format!("/permissions/user/{}", teller_b))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::FORBIDDEN);
}

#[sqlx::test]
async fn test_upsert_user_permission_success(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    crate::permissions::seeder::seed_role_permissions(&pool)
        .await
        .unwrap();

    let org_id = seed_org(&pool).await;
    let admin_id = seed_user(
        &pool,
        org_id,
        "Admin User",
        UserRole::OrgAdmin,
        "admin@t.com",
    )
    .await;
    let target_user_id = seed_user(
        &pool,
        org_id,
        "Teller User",
        UserRole::Teller,
        "teller@t.com",
    )
    .await;
    let token = generate_token(admin_id, Some(org_id), UserRole::OrgAdmin);

    let req = test::TestRequest::put()
        .uri(&format!("/permissions/user/{}", target_user_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&UpsertPermissionRequest {
            resource: "orders".to_string(),
            action: "delete".to_string(),
            granted: true,
        })
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    let perm: Permission = test::read_body_json(resp).await;
    assert_eq!(perm.user_id, target_user_id);
    assert_eq!(perm.resource, "orders");
    assert_eq!(perm.action, "delete");
    assert!(perm.granted);

    // Verify it is actually saved in DB
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM permissions WHERE user_id = $1 AND resource = 'orders' AND action = 'delete' AND granted = true")
        .bind(target_user_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
}

#[sqlx::test]
async fn test_upsert_user_permission_different_org(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    crate::permissions::seeder::seed_role_permissions(&pool)
        .await
        .unwrap();

    let org_a = seed_org(&pool).await;
    let org_b = seed_org(&pool).await;

    let admin_a = seed_user(&pool, org_a, "Admin A", UserRole::OrgAdmin, "admina@t.com").await;
    let teller_b = seed_user(&pool, org_b, "Teller B", UserRole::Teller, "tellerb@t.com").await;
    let token = generate_token(admin_a, Some(org_a), UserRole::OrgAdmin);

    let req = test::TestRequest::put()
        .uri(&format!("/permissions/user/{}", teller_b))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&UpsertPermissionRequest {
            resource: "orders".to_string(),
            action: "delete".to_string(),
            granted: true,
        })
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::FORBIDDEN);
}

#[sqlx::test]
async fn test_upsert_user_permission_invalid_enum(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    crate::permissions::seeder::seed_role_permissions(&pool)
        .await
        .unwrap();

    let org_id = seed_org(&pool).await;
    let admin_id = seed_user(
        &pool,
        org_id,
        "Admin User",
        UserRole::OrgAdmin,
        "admin@t.com",
    )
    .await;
    let target_user_id = seed_user(
        &pool,
        org_id,
        "Teller User",
        UserRole::Teller,
        "teller@t.com",
    )
    .await;
    let token = generate_token(admin_id, Some(org_id), UserRole::OrgAdmin);

    let req = test::TestRequest::put()
        .uri(&format!("/permissions/user/{}", target_user_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&UpsertPermissionRequest {
            resource: "invalid_resource".to_string(),
            action: "delete".to_string(),
            granted: true,
        })
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(!resp.status().is_success()); // Expect failure since it cannot cast "invalid_resource" to permission_resource enum
}

#[sqlx::test]
async fn test_delete_user_permission_success(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    crate::permissions::seeder::seed_role_permissions(&pool)
        .await
        .unwrap();

    let org_id = seed_org(&pool).await;
    let admin_id = seed_user(
        &pool,
        org_id,
        "Admin User",
        UserRole::OrgAdmin,
        "admin@t.com",
    )
    .await;
    let target_user_id = seed_user(
        &pool,
        org_id,
        "Teller User",
        UserRole::Teller,
        "teller@t.com",
    )
    .await;
    let token = generate_token(admin_id, Some(org_id), UserRole::OrgAdmin);

    // Seed override
    sqlx::query(
        "INSERT INTO permissions (user_id, resource, action, granted) VALUES ($1, 'orders'::permission_resource, 'delete'::permission_action, true)"
    )
    .bind(target_user_id)
    .execute(&pool)
    .await
    .unwrap();

    let req = test::TestRequest::delete()
        .uri(&format!(
            "/permissions/user/{}/orders/delete",
            target_user_id
        ))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::NO_CONTENT);

    // Verify it is gone from DB
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM permissions WHERE user_id = $1 AND resource = 'orders' AND action = 'delete'")
        .bind(target_user_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[sqlx::test]
async fn test_delete_user_permission_different_org(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    crate::permissions::seeder::seed_role_permissions(&pool)
        .await
        .unwrap();

    let org_a = seed_org(&pool).await;
    let org_b = seed_org(&pool).await;

    let admin_a = seed_user(&pool, org_a, "Admin A", UserRole::OrgAdmin, "admina@t.com").await;
    let teller_b = seed_user(&pool, org_b, "Teller B", UserRole::Teller, "tellerb@t.com").await;
    let token = generate_token(admin_a, Some(org_a), UserRole::OrgAdmin);

    let req = test::TestRequest::delete()
        .uri(&format!("/permissions/user/{}/orders/delete", teller_b))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::FORBIDDEN);
}

/// V28: a user's still-valid token stops working once their account is
/// deactivated or soft-deleted (no waiting for the JWT to expire).
#[sqlx::test]
async fn test_disabled_user_token_is_rejected(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;
    crate::permissions::seeder::seed_role_permissions(&pool)
        .await
        .unwrap();
    let org_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "Admin", UserRole::OrgAdmin, "dis@t.com").await;
    let token = generate_token(user_id, Some(org_id), UserRole::OrgAdmin);

    // Active → allowed.
    let req = test::TestRequest::get()
        .uri("/permissions/roles")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();
    assert!(
        test::call_service(&app, req).await.status().is_success(),
        "active account should work"
    );

    // Deactivated → same token rejected.
    sqlx::query("UPDATE users SET is_active = false WHERE id = $1")
        .bind(user_id)
        .execute(&pool)
        .await
        .unwrap();
    let req = test::TestRequest::get()
        .uri("/permissions/roles")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();
    assert_eq!(
        test::call_service(&app, req).await.status(),
        403,
        "deactivated account token must be rejected"
    );

    // Re-activated but soft-deleted → still rejected.
    sqlx::query("UPDATE users SET is_active = true, deleted_at = now() WHERE id = $1")
        .bind(user_id)
        .execute(&pool)
        .await
        .unwrap();
    let req = test::TestRequest::get()
        .uri("/permissions/roles")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();
    assert_eq!(
        test::call_service(&app, req).await.status(),
        403,
        "soft-deleted account token must be rejected"
    );
}

/// The `kitchen` role's security boundary: it may read the KDS feed + bump lines
/// and read stations, but is DENIED everything on the POS / cash / ticket side.
/// This is the whole point of the dedicated role — a kitchen tablet's token can't
/// ring a sale, take a payment, or settle/fire a ticket.
#[sqlx::test]
async fn kitchen_role_can_bump_but_not_touch_the_pos(pool: PgPool) {
    use crate::auth::jwt::Claims;
    use crate::permissions::checker::check_permission;

    crate::permissions::seeder::seed_role_permissions(&pool)
        .await
        .unwrap();
    let org_id = seed_org(&pool).await;
    let user_id = seed_user(
        &pool,
        org_id,
        "Grill Screen",
        UserRole::Kitchen,
        "kds@t.com",
    )
    .await;

    let claims = Claims {
        sub: user_id.to_string(),
        org_id: Some(org_id.to_string()),
        role: UserRole::Kitchen,
        branch_id: None,
        exp: 9_999_999_999,
        iat: 0,
    };

    // GRANTED — the kitchen workflow.
    for (res, act) in [
        ("kitchen_orders", "read"),
        ("kitchen_orders", "update"),
        ("kitchen_stations", "read"),
    ] {
        assert!(
            check_permission(&pool, &claims, res, act).await.is_ok(),
            "kitchen should be allowed {res}:{act}"
        );
    }
    // DENIED — the POS / cash / ticket side a kitchen device must never reach.
    for (res, act) in [
        ("orders", "create"),
        ("payments", "create"),
        ("open_tickets", "create"),
        ("open_tickets", "update"),
        ("shifts", "create"),
        ("kitchen_stations", "create"),
    ] {
        assert!(
            check_permission(&pool, &claims, res, act).await.is_err(),
            "kitchen must be denied {res}:{act}"
        );
    }
}

/// The matrix and the DB enum must agree.
///
/// `GET /auth/permissions` iterates `RESOURCES`, so a label present in the enum
/// but missing from the list is a grant no client can see — the endpoint keeps
/// enforcing it while every app hides the feature. Ten resources drifted this
/// way before this test existed (staff, delivery, reservations, floor plan).
#[sqlx::test]
async fn resources_match_the_db_enum(pool: PgPool) {
    let labels: Vec<String> =
        sqlx::query_scalar("SELECT unnest(enum_range(NULL::permission_resource))::text")
            .fetch_all(&pool)
            .await
            .unwrap();

    let listed: std::collections::HashSet<&str> =
        crate::permissions::RESOURCES.iter().copied().collect();
    let retired: std::collections::HashSet<&str> = crate::permissions::RETIRED_RESOURCES
        .iter()
        .copied()
        .collect();

    let missing: Vec<&String> = labels
        .iter()
        .filter(|l| !listed.contains(l.as_str()) && !retired.contains(l.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "these permission_resource labels are missing from RESOURCES (add them, \
         or list them in RETIRED_RESOURCES if deliberately dropped): {missing:?}"
    );

    let unknown: Vec<&&str> = listed
        .iter()
        .filter(|r| !labels.iter().any(|l| l == *r))
        .collect();
    assert!(
        unknown.is_empty(),
        "RESOURCES names labels the DB enum does not have: {unknown:?}"
    );
}

/// A dine-in order rung up at the till must be able to become an OPEN TICKET.
///
/// The alternative — the parked draft the POS used to make — is device-local, so
/// the table it sits at stays `free` in `branch_tables`. That is the row
/// `bookings::availability::occupied_now` reads and the row the dashboard's
/// floor renders, so a table with a party at it would be offered to a guest
/// booking that slot and show empty to the manager. Without this grant the
/// teller cannot open the ticket that makes the table say it is taken.
#[sqlx::test]
async fn teller_can_open_a_ticket_on_a_table(pool: PgPool) {
    use crate::auth::jwt::Claims;
    use crate::permissions::checker::check_permission;

    crate::permissions::seeder::seed_role_permissions(&pool)
        .await
        .unwrap();
    let org_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "Till 1", UserRole::Teller, "till@t.com").await;

    let claims = Claims {
        sub: user_id.to_string(),
        org_id: Some(org_id.to_string()),
        role: UserRole::Teller,
        branch_id: None,
        exp: 9_999_999_999,
        iat: 0,
    };

    for (res, act) in [
        ("open_tickets", "create"),
        ("open_tickets", "read"),
        ("open_tickets", "update"),
        // …and the floor ops that ticket then needs: move it, queue it for a
        // better section, bus its table.
        ("table_transfers", "create"),
        ("table_transfers", "read"),
        ("table_transfers", "update"),
    ] {
        assert!(
            check_permission(&pool, &claims, res, act).await.is_ok(),
            "teller should be allowed {res}:{act}"
        );
    }
}
