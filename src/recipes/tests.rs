use actix_web::{App, test, web};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::jwt::JwtSecret;
use crate::models::UserRole;
use crate::recipes::handlers::*;
use crate::recipes::routes;

fn get_secret() -> JwtSecret {
    JwtSecret("secret".to_string())
}

fn generate_token(user_id: Uuid, org_id: Option<Uuid>, role: UserRole) -> String {
    crate::auth::jwt::create_token(&get_secret(), user_id, org_id, role, None, 24).unwrap()
}

fn generate_org_admin_token(user_id: Uuid, org_id: Uuid) -> String {
    generate_token(user_id, Some(org_id), UserRole::OrgAdmin)
}

async fn seed_org(pool: &PgPool) -> Uuid {
    let org_id = Uuid::new_v4();
    let slug = format!("test-org-{}", org_id);
    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, 'Test Org', $2)")
        .bind(org_id)
        .bind(slug)
        .execute(pool)
        .await
        .unwrap();
    org_id
}

async fn seed_user(pool: &PgPool, org_id: Uuid, role: &str) -> Uuid {
    let user_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, org_id, name, email, password_hash, role) VALUES ($1, $2, 'Test User', $3, 'hash', $4::user_role)"
    )
    .bind(user_id)
    .bind(org_id)
    .bind(format!("user-{}@test.com", user_id))
    .bind(role)
    .execute(pool)
    .await
    .unwrap();
    user_id
}

async fn grant_permission(pool: &PgPool, role: &str, resource: &str, action: &str) {
    sqlx::query(
        "INSERT INTO role_permissions (role, resource, action, granted) VALUES ($1::user_role, $2::permission_resource, $3::permission_action, true) ON CONFLICT DO NOTHING"
    )
    .bind(role)
    .bind(resource)
    .bind(action)
    .execute(pool)
    .await
    .unwrap();
}

async fn seed_category(pool: &PgPool, org_id: Uuid, name: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO categories (id, org_id, name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(org_id)
        .bind(name)
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn seed_menu_item(
    pool: &PgPool,
    org_id: Uuid,
    category_id: Uuid,
    name: &str,
    price: i32,
) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO menu_items (id, org_id, category_id, name, base_price) VALUES ($1, $2, $3, $4, $5)")
        .bind(id)
        .bind(org_id)
        .bind(category_id)
        .bind(name)
        .bind(price)
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn seed_addon_item(
    pool: &PgPool,
    org_id: Uuid,
    name: &str,
    addon_type: &str,
    price: i32,
) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO addon_items (id, org_id, name, type, default_price) VALUES ($1, $2, $3, $4, $5)")
        .bind(id)
        .bind(org_id)
        .bind(name)
        .bind(addon_type)
        .bind(price)
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn seed_ingredient(pool: &PgPool, org_id: Uuid, name: &str, unit: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO org_ingredients (id, org_id, name, unit, category_id, description, cost_per_unit)\
         VALUES ($1, $2, $3, $4::inventory_unit, ingredient_category_id($2, 'veggies'), 'Fresh ingredient', 2.50)"
    )
    .bind(id)
    .bind(org_id)
    .bind(name)
    .bind(unit)
    .execute(pool)
    .await
    .unwrap();
    id
}

// ──────────────────────────────────────────────────────────────
// ── Drink Recipes Tests
// ──────────────────────────────────────────────────────────────

#[sqlx::test]
async fn test_drink_recipes_crud(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    let org_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "recipes", "create").await;
    grant_permission(&pool, "org_admin", "recipes", "read").await;
    grant_permission(&pool, "org_admin", "recipes", "update").await;
    grant_permission(&pool, "org_admin", "recipes", "delete").await;

    let cat_id = seed_category(&pool, org_id, "Drinks").await;
    let item_id = seed_menu_item(&pool, org_id, cat_id, "Latte", 500).await;
    let ingredient_id = seed_ingredient(&pool, org_id, "Milk", "ml").await;

    let token = generate_org_admin_token(user_id, org_id);

    // 1. Upsert Drink Recipe
    let req_body = UpsertDrinkRecipeRequest {
        size_label: "large".to_string(),
        org_ingredient_id: Some(ingredient_id),
        ingredient_name: "Milk".to_string(),
        ingredient_unit: "ml".to_string(),
        quantity_used: 250.0,
    };

    let req = test::TestRequest::post()
        .uri(&format!("/recipes/drinks/{}", item_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&req_body)
        .to_request();

    let resp = test::call_service(&app, req).await;
    let status = resp.status();
    let body = test::read_body(resp).await;
    assert!(
        status.is_success(),
        "Status: {}, Response: {:?}",
        status,
        body
    );

    // 2. List Drink Recipes
    let req_list = test::TestRequest::get()
        .uri(&format!("/recipes/drinks/{}", item_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp_list = test::call_service(&app, req_list).await;
    let list_status = resp_list.status();
    let list_body = test::read_body(resp_list).await;
    assert!(list_status.is_success());
    let recipes: Vec<DrinkRecipe> = serde_json::from_slice(&list_body).unwrap();
    assert_eq!(recipes.len(), 1);
    assert_eq!(recipes[0].ingredient_name, "Milk");

    // 3. Delete Drink Recipe
    let req_del = test::TestRequest::delete()
        .uri(&format!(
            "/recipes/drinks/{}/large?ingredient_name=Milk",
            item_id
        ))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp_del = test::call_service(&app, req_del).await;
    assert!(resp_del.status().is_success());
}

#[sqlx::test]
async fn test_drink_recipes_negative_quantity(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    let org_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "recipes", "create").await;

    let cat_id = seed_category(&pool, org_id, "Drinks").await;
    let item_id = seed_menu_item(&pool, org_id, cat_id, "Latte", 500).await;

    let token = generate_org_admin_token(user_id, org_id);

    let req_body = UpsertDrinkRecipeRequest {
        size_label: "large".to_string(),
        org_ingredient_id: None,
        ingredient_name: "Water".to_string(),
        ingredient_unit: "ml".to_string(),
        quantity_used: -50.0,
    };

    let req = test::TestRequest::post()
        .uri(&format!("/recipes/drinks/{}", item_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&req_body)
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 400);
}

#[sqlx::test]
async fn test_drink_recipes_wrong_org(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    let org_id = seed_org(&pool).await;
    let org2_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "recipes", "read").await;

    let cat_id = seed_category(&pool, org2_id, "Drinks").await;
    let item_id = seed_menu_item(&pool, org2_id, cat_id, "Latte", 500).await;

    let token = generate_org_admin_token(user_id, org_id); // User is in org 1

    let req = test::TestRequest::get()
        .uri(&format!("/recipes/drinks/{}", item_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(
        matches!(resp.status().as_u16(), 403 | 404),
        "cross-org must be denied, got {}",
        resp.status()
    );
}

// ──────────────────────────────────────────────────────────────
// ── Addon Ingredients Tests
// ──────────────────────────────────────────────────────────────

#[sqlx::test]
async fn test_addon_ingredients_crud(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    let org_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "recipes", "create").await;
    grant_permission(&pool, "org_admin", "recipes", "read").await;
    grant_permission(&pool, "org_admin", "recipes", "update").await;
    grant_permission(&pool, "org_admin", "recipes", "delete").await;

    let addon_id = seed_addon_item(&pool, org_id, "Vanilla Syrup", "syrup", 50).await;
    let ingredient_id = seed_ingredient(&pool, org_id, "Syrup", "ml").await;

    let token = generate_org_admin_token(user_id, org_id);

    // 1. Upsert Addon Ingredient
    let req_body = UpsertAddonIngredientRequest {
        org_ingredient_id: Some(ingredient_id),
        ingredient_name: "Syrup".to_string(),
        ingredient_unit: "ml".to_string(),
        quantity_used: 15.0,
    };

    let req = test::TestRequest::post()
        .uri(&format!("/recipes/addons/{}", addon_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&req_body)
        .to_request();

    let resp = test::call_service(&app, req).await;
    let status = resp.status();
    let body = test::read_body(resp).await;
    assert!(
        status.is_success(),
        "Status: {}, Response: {:?}",
        status,
        body
    );

    // 2. List Addon Ingredients
    let req_list = test::TestRequest::get()
        .uri(&format!("/recipes/addons/{}", addon_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp_list = test::call_service(&app, req_list).await;
    let list_status = resp_list.status();
    let list_body = test::read_body(resp_list).await;
    assert!(list_status.is_success());
    let recipes: Vec<AddonIngredient> = serde_json::from_slice(&list_body).unwrap();
    assert_eq!(recipes.len(), 1);
    assert_eq!(recipes[0].ingredient_name, "Syrup");

    // 3. Delete Addon Ingredient
    let req_del = test::TestRequest::delete()
        .uri(&format!(
            "/recipes/addons/{}?ingredient_name=Syrup",
            addon_id
        ))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp_del = test::call_service(&app, req_del).await;
    assert!(resp_del.status().is_success());
}

#[sqlx::test]
async fn test_addon_ingredients_wrong_org(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    let org_id = seed_org(&pool).await;
    let org2_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "recipes", "read").await;

    let addon_id = seed_addon_item(&pool, org2_id, "Vanilla Syrup", "syrup", 50).await;

    let token = generate_org_admin_token(user_id, org_id); // User is in org 1

    let req = test::TestRequest::get()
        .uri(&format!("/recipes/addons/{}", addon_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(
        matches!(resp.status().as_u16(), 403 | 404),
        "cross-org must be denied, got {}",
        resp.status()
    );
}

/// V22: a positive recipe quantity that rounds to 0 in the ingredient's base
/// unit (0.4 g into a kg-base ingredient → 0.000 kg) must be rejected, not
/// silently stored as a no-op recipe line (no deduction, no COGS).
#[sqlx::test]
async fn test_drink_recipe_subunit_rounding_to_zero_rejected(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    let org_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "recipes", "create").await;
    let cat_id = seed_category(&pool, org_id, "Drinks").await;
    let item_id = seed_menu_item(&pool, org_id, cat_id, "Latte", 500).await;
    let token = generate_org_admin_token(user_id, org_id);

    // Ingredient base unit is KILOGRAMS.
    let ing = seed_ingredient(&pool, org_id, "Almond Milk", "kg").await;

    // 0.4 g = 0.0004 kg → rounds to 0.000 kg in the numeric(12,3) column.
    let req_body = UpsertDrinkRecipeRequest {
        size_label: "large".to_string(),
        org_ingredient_id: Some(ing),
        ingredient_name: "Almond Milk".to_string(),
        ingredient_unit: "g".to_string(),
        quantity_used: 0.4,
    };
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/recipes/drinks/{}", item_id))
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .set_json(&req_body)
            .to_request(),
    )
    .await;
    assert_eq!(
        resp.status().as_u16(),
        400,
        "sub-unit quantity rounding to 0 must be rejected"
    );

    let stored: Option<sqlx::types::BigDecimal> = sqlx::query_scalar(
        "SELECT quantity_used FROM menu_item_recipes WHERE org_ingredient_id=$1",
    )
    .bind(ing)
    .fetch_optional(&pool)
    .await
    .unwrap();
    assert!(
        stored.is_none(),
        "no recipe row should be stored for a rounds-to-zero quantity"
    );
}

/// Recipe depth: an ml recipe line against a gram-based ingredient converts via
/// density, and the per-ingredient yield grosses up the stored consumption.
#[sqlx::test]
async fn test_recipe_density_and_yield_applied_at_save(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    let org_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "recipes", "create").await;
    let cat_id = seed_category(&pool, org_id, "Drinks").await;
    let item_id = seed_menu_item(&pool, org_id, cat_id, "Fried Dish", 500).await;

    // Ingredient bought by WEIGHT (g), density 0.92 g/ml, 50% usable yield.
    let ing = Uuid::new_v4();
    sqlx::query("INSERT INTO org_ingredients (id, org_id, name, unit, category_id, cost_per_unit, density_g_per_ml, yield_pct)\
                 VALUES ($1, $2, 'Olive Oil', 'g'::inventory_unit, ingredient_category_id($2, 'fats'), 3.0, 0.92, 50)")
        .bind(ing).bind(org_id).execute(&pool).await.unwrap();
    let token = generate_org_admin_token(user_id, org_id);

    // Recipe authored in millilitres.
    let req_body = UpsertDrinkRecipeRequest {
        size_label: "one_size".to_string(),
        org_ingredient_id: Some(ing),
        ingredient_name: "Olive Oil".to_string(),
        ingredient_unit: "ml".to_string(),
        quantity_used: 1000.0, // 1000 ml
    };
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/recipes/drinks/{}", item_id))
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .set_json(&req_body)
            .to_request(),
    )
    .await;
    assert!(resp.status().is_success());

    // 1000 ml × 0.92 = 920 g usable; grossed up by 50% yield → 1840 g stored.
    let (unit, qty): (String, f64) = sqlx::query_as(
        "SELECT ingredient_unit, quantity_used::float8 FROM menu_item_recipes WHERE org_ingredient_id=$1"
    ).bind(ing).fetch_one(&pool).await.unwrap();
    assert_eq!(unit, "g", "stored in the ingredient's base unit");
    assert_eq!(
        qty, 1840.0,
        "density bridge + yield gross-up applied at save"
    );
}

// ──────────────────────────────────────────────────────────────
// ── Preparation steps + the preset library
// ──────────────────────────────────────────────────────────────

use crate::recipes::steps::{self, RecipeStep, RecipeStepPreset};

const SHIPPED: &str = "static/step-animations";

/// The library is derived from the files that shipped, so every steps test
/// starts by reconciling the real folder — the same call `main` makes on boot.
async fn load_library(pool: &PgPool) {
    steps::reconcile(pool, std::path::Path::new(SHIPPED))
        .await
        .unwrap();
}

#[sqlx::test]
async fn reconcile_derives_the_library_and_retires_what_stops_shipping(pool: PgPool) {
    load_library(&pool).await;
    let presets = steps::list_presets(&pool, false).await.unwrap();
    assert!(
        presets.len() >= 60,
        "every shipped animation became a preset"
    );
    let milk = presets
        .iter()
        .find(|p| p.slug == "steam_milk")
        .expect("steam_milk");
    assert_eq!(milk.name, "Steam milk");
    assert!(!milk.name_ar.is_empty() && milk.note.is_some());
    assert!(
        milk.animation_url
            .starts_with("/static/step-animations/steam_milk.json?v=")
    );
    assert_eq!(milk.animation_sha256.len(), 64);
    assert!(milk.bytes > 0);
    // Offered in manifest order, not alphabetically.
    let orders: Vec<i16> = presets.iter().map(|p| p.sort_order).collect();
    assert!(
        orders.windows(2).all(|w| w[0] <= w[1]),
        "sorted by the manifest's order"
    );

    // Running it again changes nothing — boot happens on every deploy.
    load_library(&pool).await;
    assert_eq!(
        steps::list_presets(&pool, false).await.unwrap().len(),
        presets.len()
    );

    // A folder holding only ONE of them retires the rest without deleting them:
    // an item still using a retired preset keeps its name.
    let dir = tempfile::tempdir().unwrap();
    std::fs::copy(format!("{SHIPPED}/stir.json"), dir.path().join("stir.json")).unwrap();
    std::fs::write(
        dir.path().join("manifest.json"),
        r#"{"stir":{"name":"Stir","name_ar":"تقليب","order":10}}"#.as_bytes(),
    )
    .unwrap();
    steps::reconcile(&pool, dir.path()).await.unwrap();
    let live = steps::list_presets(&pool, false).await.unwrap();
    assert_eq!(live.len(), 1, "only the shipped one is offered");
    assert_eq!(live[0].slug, "stir");
    let kept = steps::list_presets(&pool, true).await.unwrap();
    assert_eq!(
        kept.len(),
        presets.len(),
        "retired presets are kept, never deleted"
    );

    // Shipping them again brings them back.
    load_library(&pool).await;
    assert_eq!(
        steps::list_presets(&pool, false).await.unwrap().len(),
        presets.len()
    );
}

#[sqlx::test]
async fn steps_are_presets_or_typed_names_and_ride_the_menu_payload(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
            .configure(crate::menu::routes::configure),
    )
    .await;
    load_library(&pool).await;
    let org_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    for a in ["create", "read", "update", "delete"] {
        grant_permission(&pool, "org_admin", "recipes", a).await;
        grant_permission(&pool, "org_admin", "menu_items", a).await;
    }
    let cat_id = seed_category(&pool, org_id, "Drinks").await;
    let item_id = seed_menu_item(&pool, org_id, cat_id, "Iced Latte", 500).await;
    let token = generate_org_admin_token(user_id, org_id);
    let auth = ("Authorization", format!("Bearer {token}"));

    // The dashboard's gallery.
    let presets: Vec<RecipeStepPreset> = test::call_and_read_body_json(
        &app,
        test::TestRequest::get()
            .uri("/recipes/step-presets")
            .insert_header(auth.clone())
            .to_request(),
    )
    .await;
    assert!(presets.iter().any(|p| p.slug == "scoop_ice"));

    // Three presets and one typed step.
    let body = serde_json::json!({ "steps": [
        { "kind": "preset", "preset_slug": "scoop_ice" },
        { "kind": "preset", "preset_slug": "pour_liquid" },
        { "kind": "preset", "preset_slug": "pull_shot" },
        { "kind": "custom", "title": "  Serve with the branded straw  " }
    ]});
    let resp = test::call_service(
        &app,
        test::TestRequest::put()
            .uri(&format!("/recipes/steps/{item_id}"))
            .insert_header(auth.clone())
            .set_json(&body)
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let saved: Vec<RecipeStep> = test::read_body_json(resp).await;
    assert_eq!(
        saved.iter().map(|s| s.position).collect::<Vec<_>>(),
        vec![1, 2, 3, 4]
    );

    // A preset step is named by the LIBRARY and carries its animation.
    assert_eq!(saved[0].kind, "preset");
    assert_eq!(saved[0].name, "Add ice");
    assert!(!saved[0].name_ar.is_empty());
    assert!(
        saved[0]
            .animation_url
            .as_deref()
            .is_some_and(|u| u.contains("scoop_ice.json?v="))
    );
    assert!(saved[0].animation_sha256.is_some());

    // A typed step is named by whoever typed it, and has no animation.
    assert_eq!(saved[3].kind, "custom");
    assert_eq!(saved[3].name, "Serve with the branded straw", "trimmed");
    assert_eq!(
        saved[3].name_ar, saved[3].name,
        "one language fills both rather than showing blank"
    );
    assert!(saved[3].animation_url.is_none() && saved[3].preset_slug.is_none());

    // The POS reads them inside the menu payload it already syncs — the list
    // endpoint, not just the single item.
    let list: serde_json::Value = test::call_and_read_body_json(
        &app,
        test::TestRequest::get()
            .uri(&format!("/menu-items?org_id={org_id}&full=true"))
            .insert_header(auth.clone())
            .to_request(),
    )
    .await;
    let steps = &list[0]["recipe_steps"];
    assert_eq!(steps.as_array().map(|a| a.len()), Some(4));
    assert_eq!(steps[1]["preset_slug"], "pour_liquid");
    assert!(
        steps[1]["animation_url"]
            .as_str()
            .is_some_and(|u| u.contains("pour_liquid"))
    );
    assert_eq!(steps[3]["animation_url"], serde_json::Value::Null);

    // A retired preset keeps its name on the item but loses its animation.
    sqlx::query("UPDATE recipe_step_presets SET is_active = false WHERE slug = 'pull_shot'")
        .execute(&pool)
        .await
        .unwrap();
    let after: Vec<RecipeStep> = test::call_and_read_body_json(
        &app,
        test::TestRequest::get()
            .uri(&format!("/recipes/steps/{item_id}"))
            .insert_header(auth.clone())
            .to_request(),
    )
    .await;
    assert_eq!(
        after[2].name, "Pull espresso shot",
        "the step still reads correctly"
    );
    assert!(
        after[2].animation_url.is_none(),
        "but nothing is served for it"
    );
    load_library(&pool).await;
}

#[sqlx::test]
async fn a_rejected_step_list_leaves_the_saved_one_alone(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;
    load_library(&pool).await;
    let org_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    for a in ["read", "update"] {
        grant_permission(&pool, "org_admin", "recipes", a).await;
    }
    let cat_id = seed_category(&pool, org_id, "Drinks").await;
    let item_id = seed_menu_item(&pool, org_id, cat_id, "Latte", 500).await;
    let token = generate_org_admin_token(user_id, org_id);
    let auth = ("Authorization", format!("Bearer {token}"));
    let put = |body: serde_json::Value| {
        test::TestRequest::put()
            .uri(&format!("/recipes/steps/{item_id}"))
            .insert_header(auth.clone())
            .set_json(body)
            .to_request()
    };

    let good = serde_json::json!({ "steps": [{ "kind": "preset", "preset_slug": "steam_milk" }] });
    assert_eq!(test::call_service(&app, put(good)).await.status(), 200);

    for bad in [
        serde_json::json!({ "steps": [{ "kind": "preset", "preset_slug": "levitate" }] }),
        serde_json::json!({ "steps": [{ "kind": "preset" }] }),
        serde_json::json!({ "steps": [{ "kind": "custom" }] }),
        serde_json::json!({ "steps": [{ "kind": "custom", "title": "   " }] }),
        serde_json::json!({ "steps": [{ "kind": "video", "title": "x" }] }),
    ] {
        let resp = test::call_service(&app, put(bad.clone())).await;
        assert_eq!(resp.status(), 400, "{bad}");
    }
    let still: Vec<RecipeStep> = test::call_and_read_body_json(
        &app,
        test::TestRequest::get()
            .uri(&format!("/recipes/steps/{item_id}"))
            .insert_header(auth.clone())
            .to_request(),
    )
    .await;
    assert_eq!(
        still.len(),
        1,
        "every rejection left the saved list untouched"
    );
    assert_eq!(still[0].preset_slug.as_deref(), Some("steam_milk"));

    // An empty list clears them.
    assert_eq!(
        test::call_service(&app, put(serde_json::json!({ "steps": [] })))
            .await
            .status(),
        200
    );
    let cleared: Vec<RecipeStep> = test::call_and_read_body_json(
        &app,
        test::TestRequest::get()
            .uri(&format!("/recipes/steps/{item_id}"))
            .insert_header(auth.clone())
            .to_request(),
    )
    .await;
    assert!(cleared.is_empty());

    // Another org cannot read or write this item's steps.
    let other_org = seed_org(&pool).await;
    let other_admin = seed_user(&pool, other_org, "org_admin").await;
    let other = generate_org_admin_token(other_admin, other_org);
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/recipes/steps/{item_id}"))
            .insert_header(("Authorization", format!("Bearer {other}")))
            .to_request(),
    )
    .await;
    // 404 rather than 403: the pool is tenant-scoped, so another org does not
    // even learn the item exists. Same expectation as the recipe-line tests.
    assert!(
        matches!(resp.status().as_u16(), 403 | 404),
        "{}",
        resp.status()
    );
}
