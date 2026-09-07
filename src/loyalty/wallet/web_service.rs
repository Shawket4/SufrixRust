//! The pass web service Apple's devices talk to.
//!
//! A pass carries a `webServiceURL` and an `authenticationToken`; from then on
//! the DEVICE drives updates. It registers itself, we push a content-free
//! notification when the balance moves, and the device comes back for the new
//! pass. Google needs none of this — there the object is the record.
//!
//! The four paths and their shapes are Apple's, not ours
//! (developer.apple.com — "Wallet Passes"), so the routes read oddly next to the
//! rest of the API and must stay exactly as they are:
//!
//! ```text
//! POST   /v1/devices/{device}/registrations/{passType}/{serial}
//! DELETE /v1/devices/{device}/registrations/{passType}/{serial}
//! GET    /v1/devices/{device}/registrations/{passType}?passesUpdatedSince=…
//! GET    /v1/passes/{passType}/{serial}
//! POST   /v1/log
//! ```
//!
//! Authentication is the pass's own token, sent as `Authorization: ApplePass
//! <token>`. It is per member and minted at signup, so a leaked token exposes
//! one card's balance and nothing else.

use actix_web::{HttpRequest, HttpResponse, web};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use super::apple;
use crate::errors::AppError;
use crate::loyalty::model::{self, MemberRow};

/// The member a request's `Authorization: ApplePass …` header identifies.
///
/// Compared in constant time: a byte-by-byte early exit leaks how much of a
/// guessed token was right, and these tokens are long-lived.
async fn authenticated_member(
    pool: &PgPool,
    req: &HttpRequest,
    serial: &str,
) -> Result<MemberRow, AppError> {
    let presented = req
        .headers()
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("ApplePass "))
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| AppError::Unauthorized("Missing pass token".into()))?;

    // The serial IS the member id (see `apple::pass_json`).
    let id = Uuid::parse_str(serial).map_err(|_| AppError::NotFound("No such pass".into()))?;
    let member = model::find_by_id(pool, id)
        .await?
        .ok_or_else(|| AppError::NotFound("No such pass".into()))?;

    let expected = member
        .apple_auth_token
        .as_deref()
        .ok_or_else(|| AppError::Unauthorized("This pass has no token".into()))?;
    if !constant_time_eq(presented.as_bytes(), expected.as_bytes()) {
        return Err(AppError::Unauthorized("Bad pass token".into()));
    }
    Ok(member)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[derive(Deserialize)]
pub struct PushTokenBody {
    #[serde(rename = "pushToken")]
    pub push_token: String,
}

/// A device asks to be told when this pass changes.
///
/// 201 the first time, 200 when it was already registered — Apple distinguishes
/// them and retries on anything else.
pub async fn register(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    path: web::Path<(String, String, String)>,
    body: web::Json<PushTokenBody>,
) -> Result<HttpResponse, AppError> {
    let (device_library_id, _pass_type, serial) = path.into_inner();
    let member = authenticated_member(pool.get_ref(), &req, &serial).await?;

    let inserted = sqlx::query(
        "INSERT INTO loyalty_pass_devices (device_library_id, customer_id, org_id, push_token) \
         VALUES ($1,$2,$3,$4) \
         ON CONFLICT (device_library_id, customer_id) \
         DO UPDATE SET push_token = EXCLUDED.push_token",
    )
    .bind(&device_library_id)
    .bind(member.id)
    .bind(member.org_id)
    .bind(&body.push_token)
    .execute(pool.get_ref())
    .await?;

    Ok(if inserted.rows_affected() == 1 {
        HttpResponse::Created().finish()
    } else {
        HttpResponse::Ok().finish()
    })
}

/// The customer deleted the pass, or the device is being wiped.
pub async fn unregister(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    path: web::Path<(String, String, String)>,
) -> Result<HttpResponse, AppError> {
    let (device_library_id, _pass_type, serial) = path.into_inner();
    let member = authenticated_member(pool.get_ref(), &req, &serial).await?;
    sqlx::query(
        "DELETE FROM loyalty_pass_devices WHERE device_library_id = $1 AND customer_id = $2",
    )
    .bind(&device_library_id)
    .bind(member.id)
    .execute(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().finish())
}

#[derive(Serialize)]
pub struct SerialsResponse {
    #[serde(rename = "lastUpdated")]
    pub last_updated: String,
    #[serde(rename = "serialNumbers")]
    pub serial_numbers: Vec<String>,
}

#[derive(Deserialize)]
pub struct UpdatedSince {
    #[serde(rename = "passesUpdatedSince")]
    pub passes_updated_since: Option<String>,
}

/// Which of this device's passes have changed since it last asked.
///
/// `lastUpdated` is an opaque tag to Apple; we use the newest `pass_updated_at`
/// we are reporting, so the device's next question is answered from where this
/// one left off. **204, not an empty list, when nothing changed** — an empty
/// 200 makes some iOS versions re-fetch every pass on a loop.
pub async fn serials(
    pool: web::Data<PgPool>,
    path: web::Path<(String, String)>,
    query: web::Query<UpdatedSince>,
) -> Result<HttpResponse, AppError> {
    let (device_library_id, _pass_type) = path.into_inner();
    let since = query
        .passes_updated_since
        .as_deref()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&chrono::Utc));

    let rows: Vec<(Uuid, Option<chrono::DateTime<chrono::Utc>>)> = sqlx::query_as(
        "SELECT c.id, c.pass_updated_at FROM loyalty_pass_devices d \
           JOIN loyalty_customers c ON c.id = d.customer_id \
          WHERE d.device_library_id = $1 AND c.deleted_at IS NULL \
            AND ($2::timestamptz IS NULL OR c.pass_updated_at > $2)",
    )
    .bind(&device_library_id)
    .bind(since)
    .fetch_all(pool.get_ref())
    .await?;

    if rows.is_empty() {
        return Ok(HttpResponse::NoContent().finish());
    }
    let last = rows
        .iter()
        .filter_map(|(_, t)| *t)
        .max()
        .unwrap_or_else(chrono::Utc::now);
    Ok(HttpResponse::Ok().json(SerialsResponse {
        last_updated: last.to_rfc3339(),
        serial_numbers: rows.into_iter().map(|(id, _)| id.to_string()).collect(),
    }))
}

/// The device fetching the pass itself, after a push or a registration.
///
/// Honours `If-Modified-Since` with a 304, because a device that has just been
/// told "something changed" asks for every pass it holds, and re-signing a pass
/// that has not moved is wasted work on both ends.
pub async fn latest_pass(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    path: web::Path<(String, String)>,
) -> Result<HttpResponse, AppError> {
    let (_pass_type, serial) = path.into_inner();
    let member = authenticated_member(pool.get_ref(), &req, &serial).await?;

    if let (Some(updated), Some(header)) = (
        member.pass_updated_at,
        req.headers()
            .get("If-Modified-Since")
            .and_then(|v| v.to_str().ok()),
    ) && let Ok(since) = chrono::DateTime::parse_from_rfc2822(header)
        && updated <= since.with_timezone(&chrono::Utc)
    {
        return Ok(HttpResponse::NotModified().finish());
    }

    let bytes = apple::build_pass_for(pool.get_ref(), &member).await?;
    let mut resp = HttpResponse::Ok();
    resp.content_type("application/vnd.apple.pkpass");
    if let Some(updated) = member.pass_updated_at {
        resp.append_header(("Last-Modified", updated.to_rfc2822()));
    }
    Ok(resp.body(bytes))
}

/// Apple's device-side diagnostics. Worth keeping: when a pass will not update,
/// this is the only place iOS says why.
pub async fn log(body: web::Json<serde_json::Value>) -> HttpResponse {
    tracing::warn!(payload = %body.0, "Apple Wallet device log");
    HttpResponse::Ok().finish()
}

/// Apple's paths, mounted where the pass's `webServiceURL` points.
///
/// Unauthenticated at the middleware level — each handler checks the pass's own
/// `ApplePass` token instead, because the caller is a customer's phone and has
/// no Madar session.
///
/// **`/wallet`, deliberately not `/loyalty/passes`.** Actix hands a path prefix
/// to the FIRST scope that matches and never falls through, so anything under
/// `/loyalty/...` would be swallowed by the JWT-wrapped `/loyalty` scope — the
/// routes would 404 and, if they somehow matched, would demand a session a
/// customer's phone does not have. A prefix that cannot collide is worth more
/// than a tidy-looking one; `/floor` learned this the expensive way.
pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/wallet/v1")
            .route(
                "/devices/{device}/registrations/{pass_type}/{serial}",
                web::post().to(register),
            )
            .route(
                "/devices/{device}/registrations/{pass_type}/{serial}",
                web::delete().to(unregister),
            )
            .route(
                "/devices/{device}/registrations/{pass_type}",
                web::get().to(serials),
            )
            .route("/passes/{pass_type}/{serial}", web::get().to(latest_pass))
            .route("/log", web::post().to(log)),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pass web service must not live under a prefix another scope owns.
    ///
    /// This is the `/floor` bug in miniature: two `web::scope`s sharing a
    /// prefix, the second one dead from the day it was written, and no test
    /// catching it because no test mounted both. So this one mounts both.
    #[sqlx::test]
    async fn the_pass_web_service_is_reachable_alongside_the_loyalty_scope(pool: PgPool) {
        use actix_web::{App, test};
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(pool))
                .app_data(web::Data::new(crate::auth::jwt::JwtSecret("secret".into())))
                // Registered in the same order as `main.rs`.
                .configure(crate::loyalty::routes::configure)
                .configure(configure),
        )
        .await;

        // No token, so this must be 401 — the point is that it REACHES the
        // handler at all. A 404 would mean another scope swallowed the path.
        let req = test::TestRequest::get()
            .uri("/wallet/v1/passes/pass.cloud.madar-pos.loyalty/some-serial")
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_ne!(
            resp.status(),
            actix_web::http::StatusCode::NOT_FOUND,
            "the pass web service is unreachable — a scope above it is eating the prefix"
        );
        assert_eq!(resp.status(), actix_web::http::StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn token_comparison_does_not_leak_the_prefix_by_timing() {
        assert!(constant_time_eq(b"abc123", b"abc123"));
        assert!(!constant_time_eq(b"abc123", b"abc124"));
        // A wrong length is rejected without comparing — nothing to leak.
        assert!(!constant_time_eq(b"abc", b"abc123"));
        assert!(constant_time_eq(b"", b""));
    }
}
