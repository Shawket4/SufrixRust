//! QR code generation endpoints.
//!
//! Every endpoint builds the canonical long URL server-side, creates/looks up a
//! Shlink short URL (server-to-server), then renders the QR of the *short* URL
//! and returns JSON with an inline base64 data-URL.  Clients never supply a
//! pre-made short URL — that would bypass analytics and unguessability.

use std::sync::Arc;

use actix_web::{HttpMessage, HttpRequest, HttpResponse, web};
use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::{
    auth::{guards::require_same_org, jwt::Claims},
    errors::{AppError, AppErrorResponse},
    permissions::checker::check_permission,
};

use super::{
    QrCardOptions,
    db::{self, BranchTable, CreateTableRequest},
    render_qr_card_png, render_qr_card_svg, render_qr_receipt_png,
    shlink::ShortLinkProvider,
};

// ── Response DTOs ─────────────────────────────────────────────────────────────

/// JSON returned from every QR-generation endpoint.
#[derive(Serialize, Deserialize, ToSchema)]
pub struct QrResponse {
    pub kind: String,
    pub long_url: String,
    pub short_url: String,
    pub short_code: String,
    /// `data:image/png;base64,…` (or `data:image/svg+xml;base64,…` when
    /// `svg=true`).  Paste into a browser `<img src="…">` to verify.
    pub qr_data_url: String,
}

// ── Render options (shared across QR endpoints) ───────────────────────────────

fn default_true() -> bool {
    true
}
fn default_dpi() -> u32 {
    600
}
fn default_module_px() -> u32 {
    16
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct QrRenderQuery {
    /// `true` (default) → branded A6 card PNG; `false` → plain receipt QR PNG.
    #[serde(default = "default_true")]
    pub card: bool,
    /// Dynamic caption line beneath the tagline (A6 card only).
    pub caption: Option<String>,
    /// Raster DPI for the A6 card (clamped 72–2400). Default 600.
    #[serde(default = "default_dpi")]
    pub dpi: u32,
    /// Print bleed in mm (A6 card only). Default 0.
    #[serde(default)]
    pub bleed_mm: f32,
    /// Draw crop marks (A6 card, only meaningful when `bleed_mm > 0`).
    #[serde(default)]
    pub crop_marks: bool,
    /// Return the A6 card as SVG (`data:image/svg+xml;base64,…`). Default false.
    #[serde(default)]
    pub svg: bool,
    /// Pixels per module for the plain receipt QR (1–40). Default 16.
    #[serde(default = "default_module_px")]
    pub module_px: u32,
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn extract_claims(req: &HttpRequest) -> Result<Claims, AppError> {
    req.extensions()
        .get::<Claims>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("Missing claims".into()))
}

/// Render the QR of `short_url` according to the query flags and return the
/// `data:…` string.
fn render_data_url(short_url: &str, q: &QrRenderQuery) -> Result<String, AppError> {
    if !q.card {
        let png = render_qr_receipt_png(short_url, q.module_px)?;
        return Ok(format!("data:image/png;base64,{}", B64.encode(&png)));
    }
    let opts = QrCardOptions {
        short_url: short_url.to_string(),
        caption: q.caption.clone(),
        dpi: q.dpi,
        bleed_mm: q.bleed_mm,
        crop_marks: q.crop_marks,
    };
    if q.svg {
        let svg = render_qr_card_svg(&opts)?;
        return Ok(format!(
            "data:image/svg+xml;base64,{}",
            B64.encode(svg.as_bytes())
        ));
    }
    let png = render_qr_card_png(&opts)?;
    Ok(format!("data:image/png;base64,{}", B64.encode(&png)))
}

/// Build `{PUBLIC_ORDER_BASE_URL}/order/{org_id}?branch={branch_id}`.
/// The dashboard route is `/order/$orgId` — org lives in the path segment.
fn branch_order_url(org_id: Uuid, branch_id: Uuid) -> Result<String, AppError> {
    let base = std::env::var("PUBLIC_ORDER_BASE_URL")
        .map_err(|_| AppError::ServiceUnavailable("PUBLIC_ORDER_BASE_URL not configured".into()))?;
    Ok(format!(
        "{}/order/{}?branch={}",
        base.trim_end_matches('/'),
        org_id,
        branch_id
    ))
}

/// Build `{PUBLIC_ORDER_BASE_URL}/order/{org_id}?branch={b}&table={t}`.
fn table_order_url(org_id: Uuid, branch_id: Uuid, table_id: Uuid) -> Result<String, AppError> {
    let base = std::env::var("PUBLIC_ORDER_BASE_URL")
        .map_err(|_| AppError::ServiceUnavailable("PUBLIC_ORDER_BASE_URL not configured".into()))?;
    Ok(format!(
        "{}/order/{}?branch={}&table={}",
        base.trim_end_matches('/'),
        org_id,
        branch_id,
        table_id
    ))
}

/// Validate a relative marketing path — must start with `/`, no scheme or
/// host part.  Exposed for unit tests as `validate_marketing_path_pub`.
pub fn validate_marketing_path_pub(path: &str) -> Result<(), AppError> {
    validate_marketing_path(path)
}

fn validate_marketing_path(path: &str) -> Result<(), AppError> {
    if !path.starts_with('/') {
        return Err(AppError::BadRequest(
            "path must be a relative URL starting with /".into(),
        ));
    }
    // Reject protocol-relative `//host` and anything with `:`
    if path.starts_with("//") || path.contains(':') {
        return Err(AppError::BadRequest(
            "path must not contain a scheme or host".into(),
        ));
    }
    Ok(())
}

fn marketing_url(path: &str) -> Result<String, AppError> {
    validate_marketing_path(path)?;
    let base = std::env::var("PUBLIC_ORDER_BASE_URL")
        .map_err(|_| AppError::ServiceUnavailable("PUBLIC_ORDER_BASE_URL not configured".into()))?;
    Ok(format!("{}{}", base.trim_end_matches('/'), path))
}

/// Build `{base}/order/{org_id}?branch={b}&channel=in_mall&place_name={p}&floor={f}&unit_number={u}`.
fn in_mall_order_url(
    org_id: Uuid,
    branch_id: Uuid,
    place_name: &str,
    floor: &str,
    unit_number: &str,
) -> Result<String, AppError> {
    let base = std::env::var("PUBLIC_ORDER_BASE_URL")
        .map_err(|_| AppError::ServiceUnavailable("PUBLIC_ORDER_BASE_URL not configured".into()))?;
    let p = urlencoding::encode(place_name);
    let f = urlencoding::encode(floor);
    let u = urlencoding::encode(unit_number);
    Ok(format!(
        "{}/order/{}?branch={}&channel=in_mall&place_name={}&floor={}&unit_number={}",
        base.trim_end_matches('/'),
        org_id,
        branch_id,
        p,
        f,
        u
    ))
}

/// The reservations bundle's origin. A DIFFERENT host from the ordering one —
/// `reservations.madar-pos.cloud` vs `order.madar-pos.cloud` — because the two
/// are separately built, separately deployed apps that share no code.
///
/// Unset degrades the same way `PUBLIC_ORDER_BASE_URL` does: a 503 the operator
/// can read, rather than a QR card pointing at nowhere. (`bookings::whatsapp`
/// reads the same variable and degrades more softly — it drops the manage link
/// from the message rather than failing the booking.)
fn reservations_base() -> Result<String, AppError> {
    std::env::var("PUBLIC_RESERVATIONS_BASE_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim_end_matches('/').to_string())
        .ok_or_else(|| {
            AppError::ServiceUnavailable("PUBLIC_RESERVATIONS_BASE_URL not configured".into())
        })
}

/// Base of the public loyalty site (`loyalty.madar-pos.cloud`). Degrades like
/// the two above: a 503 the operator can read, rather than a printed card that
/// leads nowhere.
fn loyalty_base() -> Result<String, AppError> {
    std::env::var("PUBLIC_LOYALTY_BASE_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim_end_matches('/').to_string())
        .ok_or_else(|| {
            AppError::ServiceUnavailable("PUBLIC_LOYALTY_BASE_URL not configured".into())
        })
}

/// Build `{PUBLIC_LOYALTY_BASE_URL}/join/{branch_id}` — the counter's join form.
fn branch_loyalty_url(branch_id: Uuid) -> Result<String, AppError> {
    Ok(format!("{}/join/{}", loyalty_base()?, branch_id))
}

/// Build `{PUBLIC_RESERVATIONS_BASE_URL}/{org_id}` — the guest picks the branch.
fn org_booking_url(org_id: Uuid) -> Result<String, AppError> {
    Ok(format!("{}/{}", reservations_base()?, org_id))
}

/// Build `{PUBLIC_RESERVATIONS_BASE_URL}/{org_id}/{branch_id}` — one branch,
/// straight to its slot picker.
fn branch_booking_url(org_id: Uuid, branch_id: Uuid) -> Result<String, AppError> {
    Ok(format!("{}/{}/{}", reservations_base()?, org_id, branch_id))
}

/// Build `{base}/order/{org_id}` — org-wide branch picker.
/// Org is the path segment; no branch pre-selection so the customer sees the picker.
fn org_order_url(org_id: Uuid) -> Result<String, AppError> {
    let base = std::env::var("PUBLIC_ORDER_BASE_URL")
        .map_err(|_| AppError::ServiceUnavailable("PUBLIC_ORDER_BASE_URL not configured".into()))?;
    Ok(format!("{}/order/{}", base.trim_end_matches('/'), org_id))
}

/// Fetch the branch, checking it belongs to the caller's org.
async fn load_branch_checked(
    pool: &PgPool,
    claims: &Claims,
    branch_id: Uuid,
) -> Result<(Uuid, Uuid), AppError> {
    let row: Option<(Uuid, Uuid)> =
        sqlx::query_as("SELECT id, org_id FROM branches WHERE id = $1 AND deleted_at IS NULL")
            .bind(branch_id)
            .fetch_optional(pool)
            .await?;
    let (id, org_id) = row.ok_or_else(|| AppError::NotFound("Branch not found".into()))?;
    require_same_org(claims, Some(org_id))?;
    Ok((id, org_id))
}

// ── GET /branches/{id}/qr ─────────────────────────────────────────────────────

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct SlugQuery {
    pub slug: Option<String>,
}

/// Optional in-mall pre-fill query params. When all three fields are present the
/// generated URL locks `channel=in_mall` and pre-fills the location for the
/// customer. When omitted, a standard branch-ordering URL is generated instead.
#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct InMallQuery {
    /// Shop or company name inside the mall (e.g. "Starbucks Kiosk 3").
    pub place_name: Option<String>,
    /// Floor (e.g. "Ground Floor").
    pub floor: Option<String>,
    /// Unit or office number (e.g. "Unit 42").
    pub unit_number: Option<String>,
}

#[utoipa::path(
    get,
    path = "/branches/{id}/qr",
    tag = "qr",
    params(
        ("id" = Uuid, Path, description = "Branch ID"),
        QrRenderQuery,
        SlugQuery,
        InMallQuery,
    ),
    responses(
        (status = 200, description = "Branch online-ordering QR (standard or in-mall)", body = QrResponse),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn branch_qr(
    req: HttpRequest,
    pool: crate::db::Db,
    provider: web::Data<Arc<dyn ShortLinkProvider>>,
    id: web::Path<Uuid>,
    q: web::Query<QrRenderQuery>,
    slug_q: web::Query<SlugQuery>,
    in_mall_q: web::Query<InMallQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "branches", "read").await?;
    let (branch_id, org_id) = load_branch_checked(pool.get_ref(), &claims, *id).await?;

    // When all three in-mall location fields are provided, generate a pre-filled
    // in-mall URL and use a separate dedup key so each location gets its own code.
    let in_mall = match (
        &in_mall_q.place_name,
        &in_mall_q.floor,
        &in_mall_q.unit_number,
    ) {
        (Some(p), Some(f), Some(u)) if !p.is_empty() && !f.is_empty() && !u.is_empty() => {
            Some((p.clone(), f.clone(), u.clone()))
        }
        _ => None,
    };

    let (kind, target_ref, long_url, auto_caption) = if let Some((place, floor, unit)) = &in_mall {
        let url = in_mall_order_url(org_id, branch_id, place, floor, unit)?;
        let target = format!("{}:in_mall:{}:{}:{}", branch_id, place, floor, unit);
        let caption = place.clone();
        ("branch_order_in_mall", target, url, Some(caption))
    } else {
        let url = branch_order_url(org_id, branch_id)?;
        ("branch_order", branch_id.to_string(), url, None)
    };

    let q_with_caption = if q.caption.is_none() {
        if let Some(cap) = auto_caption {
            QrRenderQuery {
                caption: Some(cap),
                ..q.into_inner()
            }
        } else {
            q.into_inner()
        }
    } else {
        q.into_inner()
    };

    let row = db::get_or_create_short_link(
        pool.get_ref(),
        provider.get_ref().as_ref(),
        org_id,
        Some(branch_id),
        kind,
        &target_ref,
        &long_url,
        slug_q.slug.as_deref(),
        in_mall.as_ref().map(|(p, _, _)| p.as_str()),
    )
    .await?;

    let qr_data_url = render_data_url(&row.short_url, &q_with_caption)?;
    Ok(HttpResponse::Ok().json(QrResponse {
        kind: kind.into(),
        long_url: row.long_url,
        short_url: row.short_url,
        short_code: row.short_code,
        qr_data_url,
    }))
}

// ── GET /orgs/{id}/qr ────────────────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/orgs/{id}/qr",
    tag = "qr",
    params(
        ("id" = Uuid, Path, description = "Organisation ID"),
        QrRenderQuery,
    ),
    responses(
        (status = 200, description = "Org-wide branch-picker QR", body = QrResponse),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn org_qr(
    req: HttpRequest,
    pool: crate::db::Db,
    provider: web::Data<Arc<dyn ShortLinkProvider>>,
    id: web::Path<Uuid>,
    q: web::Query<QrRenderQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "branches", "read").await?;
    let org_id = *id;
    require_same_org(&claims, Some(org_id))?;

    let long_url = org_order_url(org_id)?;
    let row = db::get_or_create_short_link(
        pool.get_ref(),
        provider.get_ref().as_ref(),
        org_id,
        None,
        "org_order",
        &org_id.to_string(),
        &long_url,
        None,
        None,
    )
    .await?;

    let qr_data_url = render_data_url(&row.short_url, &q)?;
    Ok(HttpResponse::Ok().json(QrResponse {
        kind: "org_order".into(),
        long_url: row.long_url,
        short_url: row.short_url,
        short_code: row.short_code,
        qr_data_url,
    }))
}

// ── GET /branches/{id}/booking-qr ────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/branches/{id}/booking-qr",
    tag = "qr",
    params(
        ("id" = Uuid, Path, description = "Branch ID"),
        QrRenderQuery,
        SlugQuery,
    ),
    responses(
        (status = 200, description = "Branch table-booking QR", body = QrResponse),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn branch_booking_qr(
    req: HttpRequest,
    pool: crate::db::Db,
    provider: web::Data<Arc<dyn ShortLinkProvider>>,
    id: web::Path<Uuid>,
    q: web::Query<QrRenderQuery>,
    slug_q: web::Query<SlugQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "bookings", "read").await?;
    let (branch_id, org_id) = load_branch_checked(pool.get_ref(), &claims, *id).await?;

    // Refuse to print a card that leads to a page saying "we don't take
    // bookings". The settings row defaults `enabled` to false, so this is the
    // normal state of a branch nobody has turned bookings on for.
    let enabled: Option<bool> =
        sqlx::query_scalar("SELECT enabled FROM branch_booking_settings WHERE branch_id = $1")
            .bind(branch_id)
            .fetch_optional(pool.get_ref())
            .await?;
    if enabled != Some(true) {
        return Err(AppError::Conflict(
            "Bookings are switched off for this branch".into(),
        ));
    }

    let long_url = branch_booking_url(org_id, branch_id)?;
    let row = db::get_or_create_short_link(
        pool.get_ref(),
        provider.get_ref().as_ref(),
        org_id,
        Some(branch_id),
        "branch_booking",
        &branch_id.to_string(),
        &long_url,
        slug_q.slug.as_deref(),
        None,
    )
    .await?;

    let qr_data_url = render_data_url(&row.short_url, &q)?;
    Ok(HttpResponse::Ok().json(QrResponse {
        kind: "branch_booking".into(),
        long_url: row.long_url,
        short_url: row.short_url,
        short_code: row.short_code,
        qr_data_url,
    }))
}

// ── GET /orgs/{id}/booking-qr ────────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/orgs/{id}/booking-qr",
    tag = "qr",
    params(
        ("id" = Uuid, Path, description = "Organisation ID"),
        QrRenderQuery,
    ),
    responses(
        (status = 200, description = "Org-wide branch-picker booking QR", body = QrResponse),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn org_booking_qr(
    req: HttpRequest,
    pool: crate::db::Db,
    provider: web::Data<Arc<dyn ShortLinkProvider>>,
    id: web::Path<Uuid>,
    q: web::Query<QrRenderQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "bookings", "read").await?;
    let org_id = *id;
    require_same_org(&claims, Some(org_id))?;

    // The org card leads to a branch picker, so it needs at least one branch
    // that actually takes bookings — otherwise the picker is empty.
    let any: bool = sqlx::query_scalar(
        "SELECT EXISTS( \
           SELECT 1 FROM branch_booking_settings s \
           JOIN branches b ON b.id = s.branch_id \
           WHERE b.org_id = $1 AND b.deleted_at IS NULL AND s.enabled)",
    )
    .bind(org_id)
    .fetch_one(pool.get_ref())
    .await?;
    if !any {
        return Err(AppError::Conflict(
            "No branch in this organisation takes bookings yet".into(),
        ));
    }

    let long_url = org_booking_url(org_id)?;
    let row = db::get_or_create_short_link(
        pool.get_ref(),
        provider.get_ref().as_ref(),
        org_id,
        None,
        "org_booking",
        &org_id.to_string(),
        &long_url,
        None,
        None,
    )
    .await?;

    let qr_data_url = render_data_url(&row.short_url, &q)?;
    Ok(HttpResponse::Ok().json(QrResponse {
        kind: "org_booking".into(),
        long_url: row.long_url,
        short_url: row.short_url,
        short_code: row.short_code,
        qr_data_url,
    }))
}

// ── POST /branches/{id}/tables ────────────────────────────────────────────────

#[utoipa::path(
    post,
    path = "/branches/{id}/tables",
    tag = "qr",
    params(("id" = Uuid, Path, description = "Branch ID")),
    request_body = CreateTableRequest,
    responses(
        (status = 201, description = "Table created", body = BranchTable),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn create_table(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
    body: web::Json<CreateTableRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "branches", "update").await?;
    let (branch_id, org_id) = load_branch_checked(pool.get_ref(), &claims, *id).await?;

    let table = sqlx::query_as::<_, BranchTable>(
        "INSERT INTO branch_tables (org_id, branch_id, label)
         VALUES ($1, $2, $3)
         RETURNING id, org_id, branch_id, label, is_active, created_at, updated_at",
    )
    .bind(org_id)
    .bind(branch_id)
    .bind(&body.label)
    .fetch_one(pool.get_ref())
    .await?;

    Ok(HttpResponse::Created().json(table))
}

// ── GET /branches/{id}/tables ─────────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/branches/{id}/tables",
    tag = "qr",
    params(("id" = Uuid, Path, description = "Branch ID")),
    responses(
        (status = 200, description = "Tables for this branch", body = Vec<BranchTable>),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn list_tables(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "branches", "read").await?;
    let (branch_id, _) = load_branch_checked(pool.get_ref(), &claims, *id).await?;

    let tables = sqlx::query_as::<_, BranchTable>(
        "SELECT id, org_id, branch_id, label, is_active, created_at, updated_at
         FROM branch_tables
         WHERE branch_id = $1
         ORDER BY label",
    )
    .bind(branch_id)
    .fetch_all(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(tables))
}

// ── DELETE /branches/{id}/tables/{tid} ────────────────────────────────────────

#[derive(Deserialize)]
pub struct TablePath {
    pub id: Uuid,
    pub tid: Uuid,
}

#[utoipa::path(
    delete,
    path = "/branches/{id}/tables/{tid}",
    tag = "qr",
    params(
        ("id" = Uuid, Path, description = "Branch ID"),
        ("tid" = Uuid, Path, description = "Table ID"),
    ),
    responses(
        (status = 204, description = "Table deleted"),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn delete_table(
    req: HttpRequest,
    pool: crate::db::Db,
    path: web::Path<TablePath>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "branches", "update").await?;
    let (branch_id, _) = load_branch_checked(pool.get_ref(), &claims, path.id).await?;

    let table = db::fetch_table(pool.get_ref(), path.tid).await?;
    if table.branch_id != branch_id {
        return Err(AppError::NotFound("Table not found".into()));
    }

    sqlx::query("DELETE FROM branch_tables WHERE id = $1")
        .bind(path.tid)
        .execute(pool.get_ref())
        .await?;

    Ok(HttpResponse::NoContent().finish())
}

// ── GET /branches/{id}/tables/{tid}/qr ───────────────────────────────────────

#[derive(Deserialize)]
pub struct TableQrPath {
    pub id: Uuid,
    pub tid: Uuid,
}

#[utoipa::path(
    get,
    path = "/branches/{id}/tables/{tid}/qr",
    tag = "qr",
    params(
        ("id" = Uuid, Path, description = "Branch ID"),
        ("tid" = Uuid, Path, description = "Table ID"),
        QrRenderQuery,
    ),
    responses(
        (status = 200, description = "Table online-ordering QR", body = QrResponse),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn table_qr(
    req: HttpRequest,
    pool: crate::db::Db,
    provider: web::Data<Arc<dyn ShortLinkProvider>>,
    path: web::Path<TableQrPath>,
    q: web::Query<QrRenderQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "branches", "read").await?;
    let (branch_id, org_id) = load_branch_checked(pool.get_ref(), &claims, path.id).await?;

    let table = db::fetch_table(pool.get_ref(), path.tid).await?;
    if table.branch_id != branch_id {
        return Err(AppError::NotFound("Table not found".into()));
    }

    let long_url = table_order_url(org_id, branch_id, path.tid)?;
    let caption = q.caption.clone().unwrap_or_else(|| table.label.clone());
    let q_with_caption = QrRenderQuery {
        caption: Some(caption),
        ..q.into_inner()
    };

    let row = db::get_or_create_short_link(
        pool.get_ref(),
        provider.get_ref().as_ref(),
        org_id,
        Some(branch_id),
        "table_order",
        &path.tid.to_string(),
        &long_url,
        None,
        Some(&table.label),
    )
    .await?;

    let qr_data_url = render_data_url(&row.short_url, &q_with_caption)?;
    Ok(HttpResponse::Ok().json(QrResponse {
        kind: "table_order".into(),
        long_url: row.long_url,
        short_url: row.short_url,
        short_code: row.short_code,
        qr_data_url,
    }))
}

// ── GET /delivery-orders/{id}/qr ─────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/delivery-orders/{id}/qr",
    tag = "qr",
    params(
        ("id" = Uuid, Path, description = "Delivery order ID"),
        QrRenderQuery,
    ),
    responses(
        (status = 200, description = "Order tracking QR (always a random short code)", body = QrResponse),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn delivery_order_qr(
    req: HttpRequest,
    pool: crate::db::Db,
    provider: web::Data<Arc<dyn ShortLinkProvider>>,
    id: web::Path<Uuid>,
    q: web::Query<QrRenderQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "delivery_orders", "read").await?;

    let row: Option<(Uuid, Uuid)> =
        sqlx::query_as("SELECT id, org_id FROM delivery_orders WHERE id = $1")
            .bind(*id)
            .fetch_optional(pool.get_ref())
            .await?;
    let (order_id, org_id) =
        row.ok_or_else(|| AppError::NotFound("Delivery order not found".into()))?;
    require_same_org(&claims, Some(org_id))?;

    let long_url = crate::delivery::whatsapp::tracking_url(order_id).ok_or_else(|| {
        AppError::ServiceUnavailable("PUBLIC_ORDER_BASE_URL not configured".into())
    })?;

    // order_track always uses a random short code (no customSlug) — guessable
    // slugs would let anyone enumerate order tracking pages.
    let row = db::get_or_create_short_link(
        pool.get_ref(),
        provider.get_ref().as_ref(),
        org_id,
        None,
        "order_track",
        &order_id.to_string(),
        &long_url,
        None, // ← never a custom slug
        None,
    )
    .await?;

    let qr_data_url = render_data_url(&row.short_url, &q)?;
    Ok(HttpResponse::Ok().json(QrResponse {
        kind: "order_track".into(),
        long_url: row.long_url,
        short_url: row.short_url,
        short_code: row.short_code,
        qr_data_url,
    }))
}

// ── POST /qr/links (marketing) ────────────────────────────────────────────────

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateMarketingLinkRequest {
    #[schema(example = "Promo Dec")]
    pub label: String,
    #[schema(example = "/menu?promo=december")]
    pub path: String,
    pub custom_slug: Option<String>,
}

#[utoipa::path(
    post,
    path = "/qr/links",
    tag = "qr",
    request_body = CreateMarketingLinkRequest,
    responses(
        (status = 201, description = "Marketing QR link created", body = QrResponse),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn create_marketing_link(
    req: HttpRequest,
    pool: crate::db::Db,
    provider: web::Data<Arc<dyn ShortLinkProvider>>,
    body: web::Json<CreateMarketingLinkRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "branches", "read").await?;

    let org_id = claims
        .org_id()
        .ok_or_else(|| AppError::Forbidden("Org context required".into()))?;

    let long_url = marketing_url(&body.path)?;

    let row = db::get_or_create_short_link(
        pool.get_ref(),
        provider.get_ref().as_ref(),
        org_id,
        None,
        "marketing",
        &body.path,
        &long_url,
        body.custom_slug.as_deref(),
        Some(&body.label),
    )
    .await?;

    let opts = QrCardOptions {
        short_url: row.short_url.clone(),
        ..Default::default()
    };
    let png = render_qr_card_png(&opts)?;
    let qr_data_url = format!("data:image/png;base64,{}", B64.encode(&png));

    Ok(HttpResponse::Created().json(QrResponse {
        kind: "marketing".into(),
        long_url: row.long_url,
        short_url: row.short_url,
        short_code: row.short_code,
        qr_data_url,
    }))
}

// ── GET /qr/links ─────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct MarketingLink {
    pub id: Uuid,
    pub kind: String,
    pub target_ref: String,
    pub long_url: String,
    pub short_code: String,
    pub short_url: String,
    pub label: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[utoipa::path(
    get,
    path = "/qr/links",
    tag = "qr",
    responses(
        (status = 200, description = "All marketing short links for the org", body = Vec<MarketingLink>),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn list_marketing_links(
    req: HttpRequest,
    pool: crate::db::Db,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "branches", "read").await?;

    let org_id = claims
        .org_id()
        .ok_or_else(|| AppError::Forbidden("Org context required".into()))?;

    let links = sqlx::query_as::<_, MarketingLink>(
        "SELECT id, kind, target_ref, long_url, short_code, short_url, label, created_at
         FROM qr_short_links
         WHERE org_id = $1 AND kind = 'marketing'
         ORDER BY created_at DESC",
    )
    .bind(org_id)
    .fetch_all(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(links))
}

// ── GET /branches/{id}/loyalty-qr ────────────────────────────────────────────

/// The counter's join QR: a static, per-branch card that opens the public
/// signup form. Static on purpose — it is printed once and stood on a counter,
/// so it must keep working with no reprint. The per-CUSTOMER QR is a different
/// thing entirely: it lives on their Wallet pass and carries their member token.
#[utoipa::path(
    get,
    path = "/branches/{id}/loyalty-qr",
    tag = "qr",
    params(
        ("id" = Uuid, Path, description = "Branch ID"),
        QrRenderQuery,
        SlugQuery,
    ),
    responses(
        (status = 200, description = "Branch loyalty join QR", body = QrResponse),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn branch_loyalty_qr(
    req: HttpRequest,
    pool: crate::db::Db,
    provider: web::Data<Arc<dyn ShortLinkProvider>>,
    id: web::Path<Uuid>,
    q: web::Query<QrRenderQuery>,
    slug_q: web::Query<SlugQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "loyalty", "read").await?;
    let (branch_id, org_id) = load_branch_checked(pool.get_ref(), &claims, *id).await?;

    // Refuse to print a card that leads to a page saying "we run no program
    // here" — the same guard the booking QR has, for the same reason.
    let settings =
        crate::loyalty::settings::load_effective(pool.get_ref(), org_id, branch_id).await?;
    if !settings.enabled {
        return Err(AppError::Conflict(
            "The loyalty program is switched off for this branch".into(),
        ));
    }

    let long_url = branch_loyalty_url(branch_id)?;
    let row = db::get_or_create_short_link(
        pool.get_ref(),
        provider.get_ref().as_ref(),
        org_id,
        Some(branch_id),
        "branch_loyalty",
        &branch_id.to_string(),
        &long_url,
        slug_q.slug.as_deref(),
        None,
    )
    .await?;

    let qr_data_url = render_data_url(&row.short_url, &q)?;
    Ok(HttpResponse::Ok().json(QrResponse {
        kind: "branch_loyalty".into(),
        long_url: row.long_url,
        short_url: row.short_url,
        short_code: row.short_code,
        qr_data_url,
    }))
}
