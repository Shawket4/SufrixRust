//! Tests for the QR card module.
//!
//! `render` — pure rendering tests (9 original, no DB, no Shlink).
//! `http`   — HTTP-layer tests with `#[sqlx::test]` and a fake ShortLinkProvider.

// ── Pure render tests ─────────────────────────────────────────────────────────

#[cfg(test)]
mod render {
    use image::GenericImageView;

    use crate::qr_card::{
        PAPER, QrCardOptions, TEAL, TEAL_LIGHT, render, render_qr_card_png, render_qr_card_svg,
        render_qr_receipt_png,
    };

    const SHORT: &str = "https://sfx.link/Ab3xK";

    fn opts(short_url: &str) -> QrCardOptions {
        QrCardOptions {
            short_url: short_url.to_string(),
            ..Default::default()
        }
    }

    fn decode_png(png: &[u8]) -> String {
        let img = image::load_from_memory(png).expect("valid png").to_luma8();
        let mut prepared = rqrr::PreparedImage::prepare(img);
        let grids = prepared.detect_grids();
        assert!(!grids.is_empty(), "no QR grid detected");
        let (_meta, content) = grids[0].decode().expect("QR decodes");
        content
    }

    fn dims(png: &[u8]) -> (u32, u32) {
        image::load_from_memory(png)
            .expect("valid png")
            .dimensions()
    }

    #[test]
    fn card_scans_at_300_and_600_dpi() {
        for dpi in [300u32, 600] {
            let png = render_qr_card_png(&QrCardOptions { dpi, ..opts(SHORT) }).expect("render");
            assert_eq!(decode_png(&png), SHORT, "dpi {dpi}");
        }
    }

    #[test]
    fn card_scans_with_caption_latin_and_arabic() {
        for caption in ["Table 5", "امسح للقائمة"] {
            let png = render_qr_card_png(&QrCardOptions {
                caption: Some(caption.to_string()),
                ..opts(SHORT)
            })
            .expect("render");
            assert_eq!(decode_png(&png), SHORT, "caption {caption}");
        }
    }

    #[test]
    fn a6_dimensions_are_exact() {
        let png = render_qr_card_png(&QrCardOptions {
            dpi: 300,
            ..opts(SHORT)
        })
        .expect("render");
        assert_eq!(dims(&png), (1240, 1748), "A6 trim @ 300 DPI");

        let png600 = render_qr_card_png(&QrCardOptions {
            dpi: 600,
            ..opts(SHORT)
        })
        .expect("render");
        assert_eq!(
            dims(&png600),
            (render::px(105.0, 600), render::px(148.0, 600))
        );
        assert_eq!(dims(&png600), (2480, 3496));
    }

    #[test]
    fn a6_dimensions_with_bleed() {
        let png = render_qr_card_png(&QrCardOptions {
            bleed_mm: 3.0,
            crop_marks: true,
            dpi: 600,
            ..opts(SHORT)
        })
        .expect("render");
        assert_eq!(dims(&png), (render::px(111.0, 600), render::px(154.0, 600)));
        assert_eq!(decode_png(&png), SHORT);
    }

    #[test]
    fn render_is_deterministic() {
        let a = render_qr_card_png(&opts(SHORT)).expect("render");
        let b = render_qr_card_png(&opts(SHORT)).expect("render");
        assert_eq!(a, b, "same options must produce identical bytes");

        let svg_a = render_qr_card_svg(&opts(SHORT)).expect("svg");
        let svg_b = render_qr_card_svg(&opts(SHORT)).expect("svg");
        assert_eq!(svg_a, svg_b);
    }

    #[test]
    fn pathological_input_errors_without_panic() {
        let long = "a".repeat(10_000);
        let cases = ["", long.as_str(), "héllo–ünïcодé™"];
        for c in cases {
            assert!(
                render_qr_card_png(&opts(c)).is_err(),
                "expected Err for {c:?}"
            );
            assert!(
                render_qr_card_svg(&opts(c)).is_err(),
                "expected Err for {c:?}"
            );
            assert!(
                render_qr_receipt_png(c, 8).is_err(),
                "expected Err for receipt {c:?}"
            );
        }
    }

    #[test]
    fn svg_contains_brand_tokens() {
        let svg = render_qr_card_svg(&opts(SHORT)).expect("svg");
        assert!(svg.contains(TEAL));
        assert!(svg.contains(PAPER));
        assert!(svg.contains(TEAL_LIGHT));
        assert!(svg.starts_with("<svg"));
    }

    #[test]
    fn receipt_qr_scans_and_is_square() {
        let png = render_qr_receipt_png(SHORT, 8).expect("render");
        let (w, h) = dims(&png);
        assert_eq!(w, h, "receipt QR is square");
        assert_eq!(decode_png(&png), SHORT);
    }

    #[test]
    fn emit_golden_render() {
        let png = render_qr_card_png(&QrCardOptions {
            caption: Some("Table 5".to_string()),
            ..opts(SHORT)
        })
        .expect("render");
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/target/qr_card_golden.png");
        std::fs::write(path, &png).expect("write golden");
    }

    // ── Marketing path validation (pure, no DB) ───────────────────────────────

    #[test]
    fn marketing_path_validation() {
        use crate::qr_card::handlers::validate_marketing_path_pub;
        assert!(
            validate_marketing_path_pub("http://evil.com").is_err(),
            "absolute URL rejected"
        );
        assert!(
            validate_marketing_path_pub("//evil.com/x").is_err(),
            "protocol-relative rejected"
        );
        assert!(
            validate_marketing_path_pub("/menu?promo=dec").is_ok(),
            "clean relative path accepted"
        );
        assert!(
            validate_marketing_path_pub("no-leading-slash").is_err(),
            "missing leading slash rejected"
        );
    }
}

// ── HTTP layer tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod http {
    use std::sync::Arc;

    use actix_web::{App, test, web};
    use sqlx::PgPool;
    use uuid::Uuid;

    use crate::auth::jwt::JwtSecret;
    use crate::models::UserRole;
    use crate::qr_card::db::BranchTable;
    use crate::qr_card::handlers::QrResponse;
    use crate::qr_card::shlink::ShortLinkProvider;
    use crate::qr_card::shlink::fake::FakeShortLinkProvider;

    fn get_secret() -> JwtSecret {
        JwtSecret("secret".to_string())
    }

    fn token(user_id: Uuid, org_id: Uuid, role: UserRole) -> String {
        crate::auth::jwt::create_token(&get_secret(), user_id, Some(org_id), role, None, 24)
            .unwrap()
    }

    fn org_admin_token(user_id: Uuid, org_id: Uuid) -> String {
        token(user_id, org_id, UserRole::OrgAdmin)
    }

    async fn seed_org(pool: &PgPool) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query!(
            "INSERT INTO organizations (id, name, slug) VALUES ($1, 'QR Test Org', $2)",
            id,
            format!("qr-{id}")
        )
        .execute(pool)
        .await
        .unwrap();
        id
    }

    async fn seed_branch(pool: &PgPool, org_id: Uuid) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query!(
            "INSERT INTO branches (id, org_id, name) VALUES ($1, $2, 'QR Test Branch')",
            id,
            org_id
        )
        .execute(pool)
        .await
        .unwrap();
        id
    }

    async fn grant(pool: &PgPool, role: &str, resource: &str, action: &str) {
        sqlx::query(
            "INSERT INTO role_permissions (role, resource, action, granted)
             VALUES ($1::user_role, $2::permission_resource, $3::permission_action, true)
             ON CONFLICT DO NOTHING",
        )
        .bind(role)
        .bind(resource)
        .bind(action)
        .execute(pool)
        .await
        .unwrap();
    }

    fn make_app(
        pool: PgPool,
        fake: Arc<dyn ShortLinkProvider>,
    ) -> actix_web::App<
        impl actix_web::dev::ServiceFactory<
            actix_web::dev::ServiceRequest,
            Config = (),
            Response = actix_web::dev::ServiceResponse,
            Error = actix_web::Error,
            InitError = (),
        >,
    > {
        App::new()
            .app_data(web::Data::new(pool))
            .app_data(web::Data::new(get_secret()))
            .app_data(web::Data::new(fake))
            .configure(crate::qr_card::routes::configure)
            .configure(crate::branches::routes::configure)
            .configure(crate::orgs::routes::configure)
    }

    // ── Table CRUD ────────────────────────────────────────────────────────────

    #[sqlx::test]
    async fn test_create_and_list_tables(pool: PgPool) {
        let org_id = seed_org(&pool).await;
        let branch_id = seed_branch(&pool, org_id).await;
        grant(&pool, "org_admin", "branches", "update").await;
        grant(&pool, "org_admin", "branches", "read").await;

        let fake = Arc::new(FakeShortLinkProvider::new()) as Arc<dyn ShortLinkProvider>;
        let app = test::init_service(make_app(pool.clone(), fake)).await;
        let tok = org_admin_token(Uuid::new_v4(), org_id);

        let req = test::TestRequest::post()
            .uri(&format!("/branches/{branch_id}/tables"))
            .insert_header(("Authorization", format!("Bearer {tok}")))
            .set_json(&serde_json::json!({ "label": "Table 1" }))
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 201, "create table");
        let tbl: BranchTable = test::read_body_json(resp).await;
        assert_eq!(tbl.label, "Table 1");
        assert_eq!(tbl.branch_id, branch_id);

        let req = test::TestRequest::get()
            .uri(&format!("/branches/{branch_id}/tables"))
            .insert_header(("Authorization", format!("Bearer {tok}")))
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_success());
        let tables: Vec<BranchTable> = test::read_body_json(resp).await;
        assert_eq!(tables.len(), 1);
    }

    #[sqlx::test]
    async fn test_table_label_uniqueness(pool: PgPool) {
        let org_id = seed_org(&pool).await;
        let branch_id = seed_branch(&pool, org_id).await;
        grant(&pool, "org_admin", "branches", "update").await;

        let fake = Arc::new(FakeShortLinkProvider::new()) as Arc<dyn ShortLinkProvider>;
        let app = test::init_service(make_app(pool.clone(), fake)).await;
        let tok = org_admin_token(Uuid::new_v4(), org_id);

        let r1 = test::call_service(
            &app,
            test::TestRequest::post()
                .uri(&format!("/branches/{branch_id}/tables"))
                .insert_header(("Authorization", format!("Bearer {tok}")))
                .set_json(&serde_json::json!({ "label": "Table 2" }))
                .to_request(),
        )
        .await;
        assert_eq!(r1.status(), 201);

        let r2 = test::call_service(
            &app,
            test::TestRequest::post()
                .uri(&format!("/branches/{branch_id}/tables"))
                .insert_header(("Authorization", format!("Bearer {tok}")))
                .set_json(&serde_json::json!({ "label": "Table 2" }))
                .to_request(),
        )
        .await;
        assert_eq!(r2.status(), 409, "duplicate label must be 409");
    }

    #[sqlx::test]
    async fn test_delete_table(pool: PgPool) {
        let org_id = seed_org(&pool).await;
        let branch_id = seed_branch(&pool, org_id).await;
        grant(&pool, "org_admin", "branches", "update").await;
        grant(&pool, "org_admin", "branches", "read").await;

        let fake = Arc::new(FakeShortLinkProvider::new()) as Arc<dyn ShortLinkProvider>;
        let app = test::init_service(make_app(pool.clone(), fake)).await;
        let tok = org_admin_token(Uuid::new_v4(), org_id);

        let resp = test::call_service(
            &app,
            test::TestRequest::post()
                .uri(&format!("/branches/{branch_id}/tables"))
                .insert_header(("Authorization", format!("Bearer {tok}")))
                .set_json(&serde_json::json!({ "label": "To Delete" }))
                .to_request(),
        )
        .await;
        let tbl: BranchTable = test::read_body_json(resp).await;

        let resp = test::call_service(
            &app,
            test::TestRequest::delete()
                .uri(&format!("/branches/{branch_id}/tables/{}", tbl.id))
                .insert_header(("Authorization", format!("Bearer {tok}")))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 204);

        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(&format!("/branches/{branch_id}/tables"))
                .insert_header(("Authorization", format!("Bearer {tok}")))
                .to_request(),
        )
        .await;
        let tables: Vec<BranchTable> = test::read_body_json(resp).await;
        assert!(tables.is_empty());
    }

    // ── Short-link dedup ──────────────────────────────────────────────────────

    #[sqlx::test]
    async fn test_branch_qr_dedup(pool: PgPool) {
        // Safety: test process is single-threaded at this point.
        unsafe { std::env::set_var("PUBLIC_ORDER_BASE_URL", "https://example.com") };
        let org_id = seed_org(&pool).await;
        let branch_id = seed_branch(&pool, org_id).await;
        grant(&pool, "org_admin", "branches", "read").await;

        let fake = Arc::new(FakeShortLinkProvider::new()) as Arc<dyn ShortLinkProvider>;
        let app = test::init_service(make_app(pool.clone(), fake)).await;
        let tok = org_admin_token(Uuid::new_v4(), org_id);

        let call = || {
            test::TestRequest::get()
                .uri(&format!("/branches/{branch_id}/qr?card=false"))
                .insert_header(("Authorization", format!("Bearer {tok}")))
                .to_request()
        };

        let r1: QrResponse = test::read_body_json(test::call_service(&app, call()).await).await;
        let r2: QrResponse = test::read_body_json(test::call_service(&app, call()).await).await;
        assert_eq!(
            r1.short_code, r2.short_code,
            "dedup must return same short_code"
        );
    }

    // ── Auth gating ───────────────────────────────────────────────────────────

    #[sqlx::test]
    async fn test_tables_require_auth(pool: PgPool) {
        let org_id = seed_org(&pool).await;
        let branch_id = seed_branch(&pool, org_id).await;

        let fake = Arc::new(FakeShortLinkProvider::new()) as Arc<dyn ShortLinkProvider>;
        let app = test::init_service(make_app(pool.clone(), fake)).await;

        let req = test::TestRequest::get()
            .uri(&format!("/branches/{branch_id}/tables"))
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 401);
    }

    #[sqlx::test]
    async fn test_tables_require_permission(pool: PgPool) {
        let org_id = seed_org(&pool).await;
        let branch_id = seed_branch(&pool, org_id).await;

        let fake = Arc::new(FakeShortLinkProvider::new()) as Arc<dyn ShortLinkProvider>;
        let app = test::init_service(make_app(pool.clone(), fake)).await;
        let tok = org_admin_token(Uuid::new_v4(), org_id);

        let req = test::TestRequest::get()
            .uri(&format!("/branches/{branch_id}/tables"))
            .insert_header(("Authorization", format!("Bearer {tok}")))
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 403);
    }

    // ── Marketing path validation ─────────────────────────────────────────────

    #[sqlx::test]
    async fn test_marketing_link_rejects_bad_path(pool: PgPool) {
        // Safety: test process is single-threaded at this point.
        unsafe { std::env::set_var("PUBLIC_ORDER_BASE_URL", "https://example.com") };
        let org_id = seed_org(&pool).await;
        grant(&pool, "org_admin", "branches", "read").await;

        let fake = Arc::new(FakeShortLinkProvider::new()) as Arc<dyn ShortLinkProvider>;
        let app = test::init_service(make_app(pool.clone(), fake)).await;
        let tok = org_admin_token(Uuid::new_v4(), org_id);

        for bad in ["http://evil.com/x", "//evil.com/x", "no-slash"] {
            let resp = test::call_service(
                &app,
                test::TestRequest::post()
                    .uri("/qr/links")
                    .insert_header(("Authorization", format!("Bearer {tok}")))
                    .set_json(&serde_json::json!({ "label": "bad", "path": bad }))
                    .to_request(),
            )
            .await;
            assert_eq!(resp.status(), 400, "bad path {bad:?} must be 400");
        }

        let resp = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/qr/links")
                .insert_header(("Authorization", format!("Bearer {tok}")))
                .set_json(&serde_json::json!({ "label": "good", "path": "/menu?p=1" }))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 201, "valid path must succeed");
    }

    // ── org QR ────────────────────────────────────────────────────────────────

    #[sqlx::test]
    async fn test_org_qr_happy_path(pool: PgPool) {
        unsafe { std::env::set_var("PUBLIC_ORDER_BASE_URL", "https://example.com") };
        let org_id = seed_org(&pool).await;
        grant(&pool, "org_admin", "branches", "read").await;

        let fake = Arc::new(FakeShortLinkProvider::new()) as Arc<dyn ShortLinkProvider>;
        let app = test::init_service(make_app(pool.clone(), fake)).await;
        let tok = org_admin_token(Uuid::new_v4(), org_id);

        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(&format!("/orgs/{org_id}/qr?card=false"))
                .insert_header(("Authorization", format!("Bearer {tok}")))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 200, "org QR should succeed");
        let qr: QrResponse = test::read_body_json(resp).await;
        assert_eq!(qr.kind, "org_order");
        // org_id must be in the path segment, not a query param
        assert!(
            qr.long_url.contains(&format!("/order/{}", org_id)),
            "long_url must use /order/<org_id> path"
        );
        assert!(!qr.short_url.is_empty());
        assert!(qr.qr_data_url.starts_with("data:image/"));
    }

    #[sqlx::test]
    async fn test_org_qr_dedup(pool: PgPool) {
        unsafe { std::env::set_var("PUBLIC_ORDER_BASE_URL", "https://example.com") };
        let org_id = seed_org(&pool).await;
        grant(&pool, "org_admin", "branches", "read").await;

        let fake = Arc::new(FakeShortLinkProvider::new()) as Arc<dyn ShortLinkProvider>;
        let app = test::init_service(make_app(pool.clone(), fake)).await;
        let tok = org_admin_token(Uuid::new_v4(), org_id);

        let call = || {
            test::TestRequest::get()
                .uri(&format!("/orgs/{org_id}/qr?card=false"))
                .insert_header(("Authorization", format!("Bearer {tok}")))
                .to_request()
        };
        let r1: QrResponse = test::read_body_json(test::call_service(&app, call()).await).await;
        let r2: QrResponse = test::read_body_json(test::call_service(&app, call()).await).await;
        assert_eq!(
            r1.short_code, r2.short_code,
            "org QR must be deduplicated across calls"
        );
    }

    #[sqlx::test]
    async fn test_org_qr_requires_auth(pool: PgPool) {
        let org_id = seed_org(&pool).await;
        let fake = Arc::new(FakeShortLinkProvider::new()) as Arc<dyn ShortLinkProvider>;
        let app = test::init_service(make_app(pool.clone(), fake)).await;

        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(&format!("/orgs/{org_id}/qr?card=false"))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 401);
    }

    #[sqlx::test]
    async fn test_org_qr_requires_permission(pool: PgPool) {
        let org_id = seed_org(&pool).await;
        // No branches:read grant — permission check must block.
        let fake = Arc::new(FakeShortLinkProvider::new()) as Arc<dyn ShortLinkProvider>;
        let app = test::init_service(make_app(pool.clone(), fake)).await;
        let tok = org_admin_token(Uuid::new_v4(), org_id);

        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(&format!("/orgs/{org_id}/qr?card=false"))
                .insert_header(("Authorization", format!("Bearer {tok}")))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 403, "missing branches:read must yield 403");
    }

    #[sqlx::test]
    async fn test_org_qr_rejects_cross_org(pool: PgPool) {
        unsafe { std::env::set_var("PUBLIC_ORDER_BASE_URL", "https://example.com") };
        let org_a = seed_org(&pool).await;
        let org_b = seed_org(&pool).await;
        grant(&pool, "org_admin", "branches", "read").await;

        let fake = Arc::new(FakeShortLinkProvider::new()) as Arc<dyn ShortLinkProvider>;
        let app = test::init_service(make_app(pool.clone(), fake)).await;
        // Token is for org_a but the path targets org_b.
        let tok = org_admin_token(Uuid::new_v4(), org_a);

        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(&format!("/orgs/{org_b}/qr?card=false"))
                .insert_header(("Authorization", format!("Bearer {tok}")))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 403, "cross-org org QR must be 403");
    }

    // ── in-mall branch QR ─────────────────────────────────────────────────────

    #[sqlx::test]
    async fn test_branch_qr_in_mall_generates_in_mall_kind(pool: PgPool) {
        unsafe { std::env::set_var("PUBLIC_ORDER_BASE_URL", "https://example.com") };
        let org_id = seed_org(&pool).await;
        let branch_id = seed_branch(&pool, org_id).await;
        grant(&pool, "org_admin", "branches", "read").await;

        let fake = Arc::new(FakeShortLinkProvider::new()) as Arc<dyn ShortLinkProvider>;
        let app = test::init_service(make_app(pool.clone(), fake)).await;
        let tok = org_admin_token(Uuid::new_v4(), org_id);

        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(&format!(
                    "/branches/{branch_id}/qr?card=false\
                     &place_name=Shop+12&floor=Ground&unit_number=Unit+4"
                ))
                .insert_header(("Authorization", format!("Bearer {tok}")))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let qr: QrResponse = test::read_body_json(resp).await;
        assert_eq!(
            qr.kind, "branch_order_in_mall",
            "all three in-mall params must produce branch_order_in_mall kind"
        );
    }

    #[sqlx::test]
    async fn test_branch_qr_in_mall_long_url_carries_prefill_params(pool: PgPool) {
        unsafe { std::env::set_var("PUBLIC_ORDER_BASE_URL", "https://example.com") };
        let org_id = seed_org(&pool).await;
        let branch_id = seed_branch(&pool, org_id).await;
        grant(&pool, "org_admin", "branches", "read").await;

        let fake = Arc::new(FakeShortLinkProvider::new()) as Arc<dyn ShortLinkProvider>;
        let app = test::init_service(make_app(pool.clone(), fake)).await;
        let tok = org_admin_token(Uuid::new_v4(), org_id);

        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(&format!(
                    "/branches/{branch_id}/qr?card=false\
                     &place_name=Kiosk+5&floor=First+Floor&unit_number=K5"
                ))
                .insert_header(("Authorization", format!("Bearer {tok}")))
                .to_request(),
        )
        .await;
        let qr: QrResponse = test::read_body_json(resp).await;
        assert!(
            qr.long_url.contains("channel=in_mall"),
            "long_url must lock channel to in_mall; got: {}",
            qr.long_url
        );
        assert!(
            qr.long_url.contains("place_name="),
            "long_url must carry place_name"
        );
        assert!(qr.long_url.contains("floor="), "long_url must carry floor");
        assert!(
            qr.long_url.contains("unit_number="),
            "long_url must carry unit_number"
        );
        assert!(
            qr.long_url.contains(&branch_id.to_string()),
            "long_url must include branch_id"
        );
        assert!(
            qr.long_url.contains(&org_id.to_string()),
            "long_url must include org_id in path"
        );
    }

    /// Providing only place_name (missing floor and unit_number) must silently
    /// fall back to a standard branch_order URL, not return an error.
    #[sqlx::test]
    async fn test_branch_qr_in_mall_partial_params_fall_back_to_standard(pool: PgPool) {
        unsafe { std::env::set_var("PUBLIC_ORDER_BASE_URL", "https://example.com") };
        let org_id = seed_org(&pool).await;
        let branch_id = seed_branch(&pool, org_id).await;
        grant(&pool, "org_admin", "branches", "read").await;

        let fake = Arc::new(FakeShortLinkProvider::new()) as Arc<dyn ShortLinkProvider>;
        let app = test::init_service(make_app(pool.clone(), fake)).await;
        let tok = org_admin_token(Uuid::new_v4(), org_id);

        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                // only place_name, no floor or unit_number
                .uri(&format!(
                    "/branches/{branch_id}/qr?card=false&place_name=Shop+5"
                ))
                .insert_header(("Authorization", format!("Bearer {tok}")))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let qr: QrResponse = test::read_body_json(resp).await;
        assert_eq!(
            qr.kind, "branch_order",
            "incomplete in-mall params must fall back to standard branch_order kind"
        );
        assert!(
            !qr.long_url.contains("channel=in_mall"),
            "standard URL must not contain channel=in_mall"
        );
    }

    #[sqlx::test]
    async fn test_branch_qr_in_mall_different_locations_get_distinct_codes(pool: PgPool) {
        unsafe { std::env::set_var("PUBLIC_ORDER_BASE_URL", "https://example.com") };
        let org_id = seed_org(&pool).await;
        let branch_id = seed_branch(&pool, org_id).await;
        grant(&pool, "org_admin", "branches", "read").await;

        let fake = Arc::new(FakeShortLinkProvider::new()) as Arc<dyn ShortLinkProvider>;
        let app = test::init_service(make_app(pool.clone(), fake)).await;
        let tok = org_admin_token(Uuid::new_v4(), org_id);

        let r1: QrResponse = test::read_body_json(
            test::call_service(
                &app,
                test::TestRequest::get()
                    .uri(&format!(
                        "/branches/{branch_id}/qr?card=false\
                         &place_name=Location+A&floor=1&unit_number=U1"
                    ))
                    .insert_header(("Authorization", format!("Bearer {tok}")))
                    .to_request(),
            )
            .await,
        )
        .await;

        let r2: QrResponse = test::read_body_json(
            test::call_service(
                &app,
                test::TestRequest::get()
                    .uri(&format!(
                        "/branches/{branch_id}/qr?card=false\
                         &place_name=Location+B&floor=2&unit_number=U2"
                    ))
                    .insert_header(("Authorization", format!("Bearer {tok}")))
                    .to_request(),
            )
            .await,
        )
        .await;

        assert_ne!(
            r1.short_code, r2.short_code,
            "different in-mall locations must produce distinct short codes"
        );
    }

    #[sqlx::test]
    async fn test_branch_qr_in_mall_same_location_deduped(pool: PgPool) {
        unsafe { std::env::set_var("PUBLIC_ORDER_BASE_URL", "https://example.com") };
        let org_id = seed_org(&pool).await;
        let branch_id = seed_branch(&pool, org_id).await;
        grant(&pool, "org_admin", "branches", "read").await;

        let fake = Arc::new(FakeShortLinkProvider::new()) as Arc<dyn ShortLinkProvider>;
        let app = test::init_service(make_app(pool.clone(), fake)).await;
        let tok = org_admin_token(Uuid::new_v4(), org_id);

        let call = || {
            test::TestRequest::get()
                .uri(&format!(
                    "/branches/{branch_id}/qr?card=false\
                     &place_name=Kiosk&floor=Ground&unit_number=K1"
                ))
                .insert_header(("Authorization", format!("Bearer {tok}")))
                .to_request()
        };
        let r1: QrResponse = test::read_body_json(test::call_service(&app, call()).await).await;
        let r2: QrResponse = test::read_body_json(test::call_service(&app, call()).await).await;
        assert_eq!(
            r1.short_code, r2.short_code,
            "same in-mall location must return the same short code"
        );
    }

    /// Standard branch QR and an in-mall QR for the same branch must have
    /// completely different short codes — they are separate dedup entries.
    #[sqlx::test]
    async fn test_branch_qr_standard_and_in_mall_are_separate(pool: PgPool) {
        unsafe { std::env::set_var("PUBLIC_ORDER_BASE_URL", "https://example.com") };
        let org_id = seed_org(&pool).await;
        let branch_id = seed_branch(&pool, org_id).await;
        grant(&pool, "org_admin", "branches", "read").await;

        let fake = Arc::new(FakeShortLinkProvider::new()) as Arc<dyn ShortLinkProvider>;
        let app = test::init_service(make_app(pool.clone(), fake)).await;
        let tok = org_admin_token(Uuid::new_v4(), org_id);

        let standard: QrResponse = test::read_body_json(
            test::call_service(
                &app,
                test::TestRequest::get()
                    .uri(&format!("/branches/{branch_id}/qr?card=false"))
                    .insert_header(("Authorization", format!("Bearer {tok}")))
                    .to_request(),
            )
            .await,
        )
        .await;

        let in_mall: QrResponse = test::read_body_json(
            test::call_service(
                &app,
                test::TestRequest::get()
                    .uri(&format!(
                        "/branches/{branch_id}/qr?card=false\
                         &place_name=Shop&floor=1&unit_number=S1"
                    ))
                    .insert_header(("Authorization", format!("Bearer {tok}")))
                    .to_request(),
            )
            .await,
        )
        .await;

        assert_ne!(standard.short_code, in_mall.short_code);
        assert_eq!(standard.kind, "branch_order");
        assert_eq!(in_mall.kind, "branch_order_in_mall");
    }

    // ── Booking QR (the reservations app, a different host entirely) ──────────

    /// Turn bookings on for a branch, the way the settings dialog does.
    async fn enable_bookings(pool: &PgPool, org_id: Uuid, branch_id: Uuid) {
        sqlx::query(
            "INSERT INTO branch_booking_settings (org_id, branch_id, enabled) \
             VALUES ($1, $2, true) \
             ON CONFLICT (branch_id) DO UPDATE SET enabled = true",
        )
        .bind(org_id)
        .bind(branch_id)
        .execute(pool)
        .await
        .unwrap();
    }

    #[sqlx::test]
    async fn booking_qr_points_at_the_reservations_app(pool: PgPool) {
        // Safety: test process is single-threaded at this point.
        unsafe {
            std::env::set_var("PUBLIC_RESERVATIONS_BASE_URL", "https://reservations.example.com")
        };
        let org_id = seed_org(&pool).await;
        let branch_id = seed_branch(&pool, org_id).await;
        enable_bookings(&pool, org_id, branch_id).await;
        grant(&pool, "org_admin", "bookings", "read").await;

        let fake = Arc::new(FakeShortLinkProvider::new()) as Arc<dyn ShortLinkProvider>;
        let app = test::init_service(make_app(pool.clone(), fake)).await;
        let tok = org_admin_token(Uuid::new_v4(), org_id);

        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(&format!("/branches/{branch_id}/booking-qr?card=false"))
                .insert_header(("Authorization", format!("Bearer {tok}")))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let qr: QrResponse = test::read_body_json(resp).await;
        assert_eq!(qr.kind, "branch_booking");
        // The reservations bundle routes on `/{org}/{branch}` — NOT `/order/...`,
        // and not the ordering host.
        assert_eq!(
            qr.long_url,
            format!("https://reservations.example.com/{org_id}/{branch_id}")
        );

        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(&format!("/orgs/{org_id}/booking-qr?card=false"))
                .insert_header(("Authorization", format!("Bearer {tok}")))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let qr: QrResponse = test::read_body_json(resp).await;
        assert_eq!(qr.kind, "org_booking");
        assert_eq!(qr.long_url, format!("https://reservations.example.com/{org_id}"));
    }

    /// A card leading to "we don't take bookings" is worse than no card, and a
    /// branch's settings row defaults `enabled` to false — so this is the state
    /// of every branch until someone turns bookings on.
    #[sqlx::test]
    async fn booking_qr_refuses_a_branch_with_bookings_off(pool: PgPool) {
        // Safety: test process is single-threaded at this point.
        unsafe {
            std::env::set_var("PUBLIC_RESERVATIONS_BASE_URL", "https://reservations.example.com")
        };
        let org_id = seed_org(&pool).await;
        let branch_id = seed_branch(&pool, org_id).await;
        grant(&pool, "org_admin", "bookings", "read").await;

        let fake = Arc::new(FakeShortLinkProvider::new()) as Arc<dyn ShortLinkProvider>;
        let app = test::init_service(make_app(pool.clone(), fake)).await;
        let tok = org_admin_token(Uuid::new_v4(), org_id);

        for uri in [
            format!("/branches/{branch_id}/booking-qr"),
            format!("/orgs/{org_id}/booking-qr"),
        ] {
            let resp = test::call_service(
                &app,
                test::TestRequest::get()
                    .uri(&uri)
                    .insert_header(("Authorization", format!("Bearer {tok}")))
                    .to_request(),
            )
            .await;
            assert_eq!(resp.status(), 409, "{uri} with bookings off");
        }
    }

}
