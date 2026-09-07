use actix_multipart::Multipart;
use actix_web::{HttpMessage, HttpRequest, HttpResponse, web};
use futures::TryStreamExt;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    auth::{
        guards::{require_same_org, require_super_admin},
        jwt::Claims,
    },
    branches::handlers::validate_timezone,
    errors::{AppError, AppErrorResponse},
    permissions::checker::check_permission,
    uploads::handlers::delete_old_image,
};

// ── Models ────────────────────────────────────────────────────

#[derive(Debug, sqlx::FromRow, Serialize, Deserialize, ToSchema)]
pub struct Org {
    pub id: Uuid,
    #[schema(example = "The Rue")]
    pub name: String,
    #[schema(example = "the-rue")]
    pub slug: String,
    #[serde(serialize_with = "crate::uploads::handlers::serialize_opt_url")]
    pub logo_url: Option<String>,
    #[schema(example = "EGP")]
    pub currency_code: String,
    /// Tax rate as a decimal (e.g. `0.14` for 14% VAT).
    /// Stored as `BigDecimal` internally; transmitted as a JSON number.
    #[schema(value_type = f64, example = 0.14)]
    pub tax_rate: sqlx::types::BigDecimal,
    pub receipt_footer: Option<String>,
    /// The card palette derived from `logo_url` when it was uploaded
    /// (`orgs::branding`). Read-only over the API: there is nothing to set, and
    /// nothing a client may set — the point of deriving is that a shop cannot
    /// choose two colours nobody can read.
    pub brand_background: Option<String>,
    pub brand_foreground: Option<String>,
    pub brand_accent: Option<String>,
    pub is_active: bool,
    /// IANA timezone name. The org-level default that branches inherit when
    /// their own timezone is unset. Defaults to `Africa/Cairo`.
    #[schema(example = "Africa/Cairo")]
    pub timezone: String,
}

// ── Request types ─────────────────────────────────────────────

// CreateOrgRequest is consumed from multipart fields, not JSON.
// We keep this struct for the non-file fields parsed out of the form.
#[derive(Default)]
struct CreateOrgFields {
    name: Option<String>,
    slug: Option<String>,
    currency_code: Option<String>,
    tax_rate: Option<f64>,
    receipt_footer: Option<String>,
    timezone: Option<String>,
}

#[derive(Deserialize, ToSchema)]
pub struct UpdateOrgRequest {
    pub name: Option<String>,
    pub slug: Option<String>,
    pub currency_code: Option<String>,
    #[schema(example = 0.14)]
    pub tax_rate: Option<f64>,
    pub receipt_footer: Option<String>,
    pub is_active: Option<bool>,
    /// IANA timezone name (e.g. `Africa/Cairo`). Validated against the
    /// PostgreSQL timezone database. Branches inherit this when their own
    /// timezone is unset.
    #[schema(example = "Africa/Cairo")]
    pub timezone: Option<String>,
    /// `null` clears the logo; absent leaves it unchanged. To set a new
    /// logo, use `PUT /orgs/{id}/logo` (multipart) instead — JSON updates
    /// only accept the clear-to-null case here.
    #[serde(
        default,
        deserialize_with = "crate::menu::handlers::deserialize_double_option"
    )]
    #[schema(nullable, value_type = Option<String>)]
    pub logo_url: Option<Option<String>>,
}

// ── OpenAPI-only multipart schemas ────────────────────────────
//
// These structs exist solely to describe the shape of multipart/form-data
// request bodies in the generated spec. They are never constructed at
// runtime — the handlers parse multipart fields directly. Keeping them
// `pub` and `ToSchema`-derived lets `#[utoipa::path]` reference them.

#[derive(ToSchema)]
#[allow(dead_code)]
pub struct CreateOrgMultipart {
    #[schema(example = "The Rue")]
    pub name: String,

    #[schema(example = "the-rue")]
    pub slug: String,

    #[schema(example = "EGP")]
    pub currency_code: Option<String>,

    #[schema(example = 0.14)]
    pub tax_rate: Option<f64>,

    pub receipt_footer: Option<String>,

    #[schema(example = "Africa/Cairo")]
    pub timezone: Option<String>,

    /// Logo image file. PNG, JPEG, or WebP. Optional — omit the field
    /// entirely to create the org without a logo.
    #[schema(format = Binary, content_media_type = "image/*")]
    pub logo: Option<String>,
}

#[derive(ToSchema)]
#[allow(dead_code)]
pub struct UploadLogoMultipart {
    /// Logo image file. PNG, JPEG, or WebP. Required.
    #[schema(format = Binary, content_media_type = "image/*")]
    pub logo: String,
}

// ── POST /orgs  (super_admin only, multipart/form-data) ──────

#[utoipa::path(
    post,
    path = "/orgs",
    tag = "orgs",
    request_body(
        content = CreateOrgMultipart,
        content_type = "multipart/form-data",
        description = "Multipart form with text fields and an optional logo file."
    ),
    responses(
        (status = 201, description = "Organization created", body = Org),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn create_org(
    req: HttpRequest,
    pool: crate::db::Db,
    mut mp: Multipart,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "orgs", "create").await?;
    require_super_admin(&claims)?;

    let uploads_dir = std::env::var("UPLOADS_DIR").unwrap_or_else(|_| "./uploads".to_string());
    let base_url = std::env::var("UPLOADS_BASE_URL").unwrap_or_default();

    let mut fields = CreateOrgFields::default();
    let mut logo_url: Option<String> = None;

    while let Some(mut field) = mp
        .try_next()
        .await
        .map_err(|e| AppError::BadRequest(format!("Multipart error: {e}")))?
    {
        let name = field.name().unwrap_or("").to_string();

        match name.as_str() {
            "logo" => {
                let mut bytes = Vec::new();
                while let Some(chunk) = field
                    .try_next()
                    .await
                    .map_err(|e| AppError::BadRequest(format!("Upload read error: {e}")))?
                {
                    bytes.extend_from_slice(chunk.as_ref());
                }
                if !bytes.is_empty() {
                    let ct = field
                        .content_type()
                        .map(|m| m.to_string())
                        .unwrap_or_default();
                    let ext = match ct.as_str() {
                        "image/png" => "png",
                        "image/webp" => "webp",
                        _ => "jpg",
                    };
                    let filename = format!("{}.{}", Uuid::new_v4(), ext);
                    let file_path = format!("{}/logos/{}", uploads_dir, filename);
                    std::fs::create_dir_all(format!("{}/logos", uploads_dir))
                        .map_err(|_| AppError::Internal)?;
                    std::fs::write(&file_path, &bytes).map_err(|_| AppError::Internal)?;
                    logo_url = Some(format!(
                        "{}/logos/{}",
                        base_url.trim_end_matches('/'),
                        filename
                    ));
                }
            }
            "name" => fields.name = text_field(&mut field).await?,
            "slug" => fields.slug = text_field(&mut field).await?,
            "currency_code" => fields.currency_code = text_field(&mut field).await?,
            "tax_rate" => {
                if let Some(s) = text_field(&mut field).await? {
                    fields.tax_rate = s.parse::<f64>().ok();
                }
            }
            "receipt_footer" => fields.receipt_footer = text_field(&mut field).await?,
            "timezone" => fields.timezone = text_field(&mut field).await?,
            _ => {
                drain_field(&mut field).await?;
            }
        }
    }

    let name = fields
        .name
        .ok_or_else(|| AppError::BadRequest("name is required".into()))?;
    let slug = fields
        .slug
        .ok_or_else(|| AppError::BadRequest("slug is required".into()))?;

    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM organizations WHERE slug = $1)")
            .bind(&slug)
            .fetch_one(pool.get_ref())
            .await?;

    if exists {
        return Err(AppError::Conflict(format!(
            "Slug '{}' is already taken",
            slug
        )));
    }

    let currency = fields.currency_code.as_deref().unwrap_or("EGP");
    let tax_rate = fields.tax_rate.unwrap_or(0.14);
    if !(0.0..=1.0).contains(&tax_rate) {
        return Err(AppError::BadRequest(
            "tax_rate must be between 0 and 1".into(),
        ));
    }

    let timezone = fields
        .timezone
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or("Africa/Cairo");
    validate_timezone(pool.get_ref(), timezone).await?;

    let mut tx = pool.begin().await?;

    let org = sqlx::query_as::<_, Org>(
        r#"
        INSERT INTO organizations (name, slug, logo_url, currency_code, tax_rate, receipt_footer, timezone)
        VALUES ($1, $2, $3, $4, $5, $6, $7::timezone_name)
        RETURNING id, name, slug, logo_url, currency_code, tax_rate, receipt_footer, brand_background, brand_foreground, brand_accent, is_active, timezone::text AS timezone
        "#,
    )
    .bind(&name)
    .bind(&slug)
    .bind(&logo_url)
    .bind(currency)
    .bind(tax_rate)
    .bind(&fields.receipt_footer)
    .bind(timezone)
    .fetch_one(&mut *tx)
    .await?;

    sqlx::query(
        r#"
        INSERT INTO org_payment_methods (org_id, name, label_translations, color, icon, is_cash)
        VALUES
            ($1, 'cash', '{"en": "Cash", "ar": "نقدي"}', '#10B981', 'money', true),
            ($1, 'card', '{"en": "Card", "ar": "بطاقة"}', '#3B82F6', 'credit_card', false),
            ($1, 'digital_wallet', '{"en": "Digital Wallet", "ar": "محفظة رقمية"}', '#8B5CF6', 'wallet', false),
            ($1, 'mixed', '{"en": "Mixed", "ar": "مختلط"}', '#F59E0B', 'pie_chart', false),
            ($1, 'talabat_online', '{"en": "Talabat Online", "ar": "طلبات أونلاين"}', '#EF4444', 'delivery', false),
            ($1, 'talabat_cash', '{"en": "Talabat Cash", "ar": "طلبات كاش"}', '#F97316', 'delivery', true)
        "#
    )
    .bind(org.id)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(HttpResponse::Created().json(org))
}

// ── GET /orgs  (super_admin only) ────────────────────────────

#[utoipa::path(
    get,
    path = "/orgs",
    tag = "orgs",
    responses(
        (status = 200, description = "List of all organizations", body = Vec<Org>),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn list_orgs(req: HttpRequest, pool: crate::db::Db) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "orgs", "read").await?;
    require_super_admin(&claims)?;

    let orgs = sqlx::query_as::<_, Org>(
        r#"
        SELECT id, name, slug, logo_url, currency_code, tax_rate, receipt_footer, brand_background, brand_foreground, brand_accent, is_active, timezone::text AS timezone
        FROM organizations
        WHERE deleted_at IS NULL
        ORDER BY name
        "#,
    )
    .fetch_all(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(orgs))
}

// ── GET /orgs/:id  (org-scoped read) ─────────────────────────

#[utoipa::path(
    get,
    path = "/orgs/{id}",
    tag = "orgs",
    params(
        ("id" = Uuid, Path, description = "Organization ID")
    ),
    responses(
        (status = 200, description = "The requested organization", body = Org),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn get_org(
    req: HttpRequest,
    pool: crate::db::Db,
    org_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "orgs", "read").await?;
    require_same_org(&claims, Some(*org_id))?;

    let org = fetch_org(pool.get_ref(), *org_id).await?;
    Ok(HttpResponse::Ok().json(org))
}

// ── GET /orgs/:id/offline-auth-bundle  (org-scoped) ──────────
//
// The org's offline-auth bundle: an argon2id PIN verifier per active teller, so
// any teller in the org can unlock the POS OFFLINE on a device that synced this
// bundle while online. Authorization is org-scoped (`require_same_org`) — a
// token must belong to the org it asks for. The verifier is NOT the login
// credential (see `auth::offline`); tellers who never logged in online have a
// `null` hash and can't offline-unlock until they do.

#[derive(Debug, sqlx::FromRow, Serialize, ToSchema)]
pub struct OfflineTellerCredential {
    pub user_id: Uuid,
    pub name: String,
    /// PIN-login role: `teller`, `waiter`, or `kitchen`. The device uses this to
    /// route the offline session (a waiter lands on tickets, a kitchen device on
    /// the KDS) without re-querying the backend.
    pub role: String,
    pub is_active: bool,
    /// argon2id verifier of the user's PIN (derived at online login). `null`
    /// until the user has logged in online at least once.
    pub offline_pin_hash: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct OfflineAuthBundle {
    pub org_id: Uuid,
    pub generated_at: chrono::DateTime<chrono::Utc>,
    /// All PIN-login credentials for the org (tellers, waiters, and kitchen
    /// devices). Field name kept as `tellers` for wire compatibility; it carries
    /// every offline-capable role, distinguished by `role`.
    pub tellers: Vec<OfflineTellerCredential>,
    /// The org's stable LAN-relay secret, hex-encoded. Devices derive a per-branch
    /// HMAC-SHA256 subkey from it to sign every LAN message (Phase E), so only
    /// branch-provisioned devices are trusted on the shared Wi-Fi.
    pub lan_secret: String,
}

#[utoipa::path(
    get,
    path = "/orgs/{id}/offline-auth-bundle",
    tag = "orgs",
    params(("id" = Uuid, Path, description = "Organization ID")),
    responses(
        (status = 200, description = "Org offline-auth bundle", body = OfflineAuthBundle),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn offline_auth_bundle(
    req: HttpRequest,
    pool: crate::db::Db,
    org_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    // Authorization: the caller's token MUST belong to this org. The bundle is
    // fetched by the device's signed-in user to enable offline unlock for the
    // whole org — no extra role gate, but never across orgs.
    require_same_org(&claims, Some(*org_id))?;

    // All PIN-login roles ship in the bundle so a waiter or kitchen device can
    // unlock offline, not just tellers. The role rides along so the device can
    // route the offline session (waiter → tickets, kitchen → KDS).
    let tellers = sqlx::query_as::<_, OfflineTellerCredential>(
        r#"
        SELECT id AS user_id, name, role::text AS role, is_active, offline_pin_hash
        FROM users
        WHERE org_id = $1 AND role IN ('teller', 'waiter', 'kitchen') AND deleted_at IS NULL
        ORDER BY name
        "#,
    )
    .bind(*org_id)
    .fetch_all(pool.get_ref())
    .await?;

    // The org's stable LAN secret, hex-encoded in SQL (no Rust encoding dep).
    let lan_secret: String =
        sqlx::query_scalar("SELECT encode(lan_secret, 'hex') FROM organizations WHERE id = $1")
            .bind(*org_id)
            .fetch_one(pool.get_ref())
            .await?;

    Ok(HttpResponse::Ok().json(OfflineAuthBundle {
        org_id: *org_id,
        generated_at: chrono::Utc::now(),
        tellers,
        lan_secret,
    }))
}

// ── PATCH /orgs/:id  (super_admin only) ──────────────────────
// JSON only — logo swap uses PUT /orgs/{id}/logo.

#[utoipa::path(
    patch,
    path = "/orgs/{id}",
    tag = "orgs",
    params(
        ("id" = Uuid, Path, description = "Organization ID")
    ),
    request_body = UpdateOrgRequest,
    responses(
        (status = 200, description = "Organization updated", body = Org),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn update_org(
    req: HttpRequest,
    pool: crate::db::Db,
    org_id: web::Path<Uuid>,
    body: web::Json<UpdateOrgRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "orgs", "update").await?;
    require_super_admin(&claims)?;

    let existing = fetch_org(pool.get_ref(), *org_id).await?;

    if let Some(slug) = &body.slug {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM organizations WHERE slug = $1 AND id != $2)",
        )
        .bind(slug)
        .bind(*org_id)
        .fetch_one(pool.get_ref())
        .await?;

        if exists {
            return Err(AppError::Conflict(format!(
                "Slug '{}' is already taken",
                slug
            )));
        }
    }

    if let Some(r) = body.tax_rate {
        if !(0.0..=1.0).contains(&r) {
            return Err(AppError::BadRequest(
                "tax_rate must be between 0 and 1".into(),
            ));
        }
    }

    if let Some(tz) = body.timezone.as_deref().filter(|s| !s.is_empty()) {
        validate_timezone(pool.get_ref(), tz).await?;
    }

    let logo_url_is_present = body.logo_url.is_some();
    let logo_url_val = body.logo_url.as_ref().and_then(|o| o.clone());

    let org = sqlx::query_as::<_, Org>(
        r#"
        UPDATE organizations SET
            name           = COALESCE($2, name),
            slug           = COALESCE($3, slug),
            currency_code  = COALESCE($4, currency_code),
            tax_rate       = COALESCE($5, tax_rate),
            receipt_footer = COALESCE($6, receipt_footer),
            is_active      = COALESCE($7, is_active),
            logo_url       = CASE WHEN $9 THEN $8 ELSE logo_url END,
            timezone       = COALESCE(NULLIF($10, '')::timezone_name, timezone),
            updated_at     = NOW()
        WHERE id = $1 AND deleted_at IS NULL
        RETURNING id, name, slug, logo_url, currency_code, tax_rate, receipt_footer, brand_background, brand_foreground, brand_accent, is_active, timezone::text AS timezone
        "#,
    )
    .bind(*org_id)
    .bind(&body.name)
    .bind(&body.slug)
    .bind(&body.currency_code)
    .bind(body.tax_rate)
    .bind(&body.receipt_footer)
    .bind(body.is_active)
    .bind(&logo_url_val)
    .bind(logo_url_is_present)
    .bind(&body.timezone)
    .fetch_optional(pool.get_ref())
    .await?
    .ok_or_else(|| AppError::NotFound("Org not found".into()))?;

    if body.logo_url == Some(None)
        && let Some(old_url) = existing.logo_url
    {
        let uploads_dir = std::env::var("UPLOADS_DIR").unwrap_or_else(|_| "./uploads".to_string());
        let base_url = std::env::var("UPLOADS_BASE_URL").unwrap_or_default();
        delete_old_image(&old_url, &base_url, &uploads_dir, None).await;
    }

    // If this update toggled the active flag, drop the cached org status so the
    // suspension (or reactivation) is enforced on the next request rather than
    // after the cache TTL elapses.
    if body.is_active.is_some()
        && let Some(cache) = req.app_data::<web::Data<crate::auth::org_status::OrgStatusCache>>()
    {
        cache.invalidate(*org_id);
    }

    Ok(HttpResponse::Ok().json(org))
}

// ── PUT /orgs/:id/logo  (super_admin only, multipart) ────────

#[utoipa::path(
    put,
    path = "/orgs/{id}/logo",
    tag = "orgs",
    params(
        ("id" = Uuid, Path, description = "Organization ID")
    ),
    request_body(
        content = UploadLogoMultipart,
        content_type = "multipart/form-data",
        description = "Multipart form with a single `logo` file field."
    ),
    responses(
        (status = 200, description = "Logo replaced; updated organization returned", body = Org),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn upload_org_logo(
    req: HttpRequest,
    pool: crate::db::Db,
    org_id: web::Path<Uuid>,
    mut mp: Multipart,
) -> Result<HttpResponse, AppError> {
    // Disabled in the public demo (uploaded files outlive the sweeper's DB-only GC).
    if crate::demo::config::demo_mode() {
        return Err(AppError::BadRequest(
            "Image uploads are disabled in the demo.".into(),
        ));
    }
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "orgs", "update").await?;
    // The logo is the ONE piece of an organisation a manager owns: it is their
    // shop's mark, it appears on their receipts and their loyalty cards, and
    // waiting on a super admin to change it helps nobody. Everything else about
    // an organisation stays super-admin only — so this is scoped to their OWN
    // org rather than lifting the guard.
    if claims.role != crate::models::UserRole::SuperAdmin && claims.org_id() != Some(*org_id) {
        return Err(AppError::Forbidden(
            "You can only change your own organisation's logo".into(),
        ));
    }

    let existing = fetch_org(pool.get_ref(), *org_id).await?;
    let uploads_dir = std::env::var("UPLOADS_DIR").unwrap_or_else(|_| "./uploads".to_string());
    let base_url = std::env::var("UPLOADS_BASE_URL").unwrap_or_default();

    let mut new_logo_url: Option<String> = None;
    let mut logo_bytes: Vec<u8> = Vec::new();

    while let Some(mut field) = mp
        .try_next()
        .await
        .map_err(|e| AppError::BadRequest(format!("Multipart error: {e}")))?
    {
        if field.name().unwrap_or("") != "logo" {
            drain_field(&mut field).await?;
            continue;
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = field
            .try_next()
            .await
            .map_err(|e| AppError::BadRequest(format!("Upload read error: {e}")))?
        {
            bytes.extend_from_slice(chunk.as_ref());
        }
        if !bytes.is_empty() {
            let ct = field
                .content_type()
                .map(|m| m.to_string())
                .unwrap_or_default();
            let ext = match ct.as_str() {
                "image/png" => "png",
                "image/webp" => "webp",
                _ => "jpg",
            };
            let filename = format!("{}.{}", Uuid::new_v4(), ext);
            let dir = format!("{}/logos", uploads_dir);
            std::fs::create_dir_all(&dir).map_err(|_| AppError::Internal)?;
            std::fs::write(format!("{}/{}", dir, filename), &bytes)
                .map_err(|_| AppError::Internal)?;
            new_logo_url = Some(format!(
                "{}/logos/{}",
                base_url.trim_end_matches('/'),
                filename,
            ));
            logo_bytes = bytes;
        }
    }

    let new_logo_url = new_logo_url
        .ok_or_else(|| AppError::BadRequest("No logo file received in field 'logo'".into()))?;

    // Read the brand palette straight out of the bytes we were just handed: no
    // fetch, so no stale cache and no server-side request to an address someone
    // else supplied. A logo we cannot decode, or one with no colour in it (a
    // plain black mark is common), leaves the columns NULL and the card falls
    // back to Madar's palette — a finished card, not a broken one.
    let palette = image::load_from_memory(&logo_bytes)
        .ok()
        .as_ref()
        .and_then(crate::orgs::branding::palette_from_image);

    let org = sqlx::query_as::<_, Org>(
        r#"
        UPDATE organizations
        SET logo_url = $2, updated_at = NOW(),
            brand_background = $3, brand_foreground = $4, brand_accent = $5,
            brand_logo_source = $2
        WHERE id = $1 AND deleted_at IS NULL
        RETURNING id, name, slug, logo_url, currency_code, tax_rate, receipt_footer, brand_background, brand_foreground, brand_accent, is_active, timezone::text AS timezone
        "#,
    )
    .bind(*org_id)
    .bind(&new_logo_url)
    .bind(palette.as_ref().map(|p| p.background.clone()))
    .bind(palette.as_ref().map(|p| p.foreground.clone()))
    .bind(palette.as_ref().map(|p| p.accent.clone()))
    .fetch_optional(pool.get_ref())
    .await?
    .ok_or_else(|| AppError::NotFound("Org not found".into()))?;

    if let Some(old_url) = existing.logo_url {
        delete_old_image(&old_url, &base_url, &uploads_dir, None).await;
    }

    Ok(HttpResponse::Ok().json(org))
}

// ── DELETE /orgs/:id  (super_admin only) ─────────────────────

#[utoipa::path(
    delete,
    path = "/orgs/{id}",
    tag = "orgs",
    params(
        ("id" = Uuid, Path, description = "Organization ID")
    ),
    responses(
        (status = 204, description = "Organization deleted (soft delete)"),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn delete_org(
    req: HttpRequest,
    pool: crate::db::Db,
    org_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "orgs", "delete").await?;
    require_super_admin(&claims)?;

    let rows_affected = sqlx::query(
        "UPDATE organizations SET deleted_at = NOW(), is_active = false WHERE id = $1 AND deleted_at IS NULL"
    )
    .bind(*org_id)
    .execute(pool.get_ref())
    .await?
    .rows_affected();

    if rows_affected == 0 {
        return Err(AppError::NotFound("Org not found".into()));
    }

    // Evict the cached status so the soft-delete is enforced immediately.
    if let Some(cache) = req.app_data::<web::Data<crate::auth::org_status::OrgStatusCache>>() {
        cache.invalidate(*org_id);
    }

    Ok(HttpResponse::NoContent().finish())
}

// ── Helpers ───────────────────────────────────────────────────

pub(crate) fn extract_claims(req: &HttpRequest) -> Result<Claims, AppError> {
    req.extensions()
        .get::<Claims>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("Missing claims".into()))
}

async fn fetch_org(pool: &PgPool, id: Uuid) -> Result<Org, AppError> {
    sqlx::query_as::<_, Org>(
        "SELECT id, name, slug, logo_url, currency_code, tax_rate, receipt_footer, brand_background, brand_foreground, brand_accent, is_active, timezone::text AS timezone
         FROM organizations
         WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::NotFound("Org not found".into()))
}

async fn drain_field(field: &mut actix_multipart::Field) -> Result<(), AppError> {
    while field
        .try_next()
        .await
        .map_err(|e| AppError::BadRequest(e.to_string()))?
        .is_some()
    {}
    Ok(())
}

async fn text_field(field: &mut actix_multipart::Field) -> Result<Option<String>, AppError> {
    let mut buf = Vec::new();
    while let Some(chunk) = field
        .try_next()
        .await
        .map_err(|e| AppError::BadRequest(e.to_string()))?
    {
        buf.extend_from_slice(chunk.as_ref());
    }
    Ok(if buf.is_empty() {
        None
    } else {
        Some(
            String::from_utf8(buf)
                .map_err(|_| AppError::BadRequest("Invalid UTF-8 in field".into()))?,
        )
    })
}

// ── GET /public/orgs  (Unauthenticated) ──────────────────────

#[derive(Debug, Serialize, ToSchema, sqlx::FromRow)]
pub struct PublicOrg {
    #[schema(example = "The Rue")]
    pub name: String,
    #[schema(
        nullable,
        example = "https://madar-pos.cloud/api/uploads/logos/123.png"
    )]
    pub logo_url: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    #[schema(example = 5)]
    pub branch_count: Option<i64>,
    #[schema(example = "Cairo, Egypt")]
    pub address: Option<String>,
}

#[utoipa::path(
    get,
    path = "/public/orgs",
    tag = "orgs",
    responses(
        (status = 200, description = "List of public organizations", body = Vec<PublicOrg>),
        AppErrorResponse,
    )
)]
pub async fn list_public_orgs(pool: web::Data<PgPool>) -> Result<HttpResponse, AppError> {
    let orgs = sqlx::query_as::<_, PublicOrg>(
        r#"
        SELECT 
            o.name, 
            o.logo_url, 
            o.created_at,
            (SELECT COUNT(*)::bigint FROM branches b WHERE b.org_id = o.id AND b.deleted_at IS NULL) as branch_count,
            (SELECT address FROM branches b WHERE b.org_id = o.id AND b.deleted_at IS NULL AND b.address IS NOT NULL ORDER BY b.created_at ASC LIMIT 1) as address
        FROM organizations o
        WHERE o.deleted_at IS NULL AND o.is_active = true
        ORDER BY o.created_at ASC
        "#,
    )
    .fetch_all(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(orgs))
}
