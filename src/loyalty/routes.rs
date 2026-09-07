//! Loyalty routes.
//!
//! Staff surfaces sit behind `JwtMiddleware`; the customer-facing signup and
//! card endpoints are unauthenticated and each carries its own per-IP limiter,
//! matching how `delivery::routes` treats the public ordering endpoints.

use actix_governor::{Governor, GovernorConfigBuilder};
use actix_web::{middleware::Condition, web};

use crate::auth::middleware::JwtMiddleware;
use crate::loyalty::{award, handlers, public, settings};
use crate::rate_limit::{PeerIpOrLocalhost, rate_limiting_enabled};

pub fn configure(cfg: &mut web::ServiceConfig) {
    // Reading a join page or a card: ~60/min sustained, burst 30 — the same
    // budget the public menu gets.
    let browse_gov = GovernorConfigBuilder::default()
        .key_extractor(PeerIpOrLocalhost)
        .seconds_per_request(1)
        .burst_size(30)
        .finish()
        .expect("Invalid loyalty browse rate limiter");
    // Signing up writes a member row and may issue a pass: ~10/min, burst 5.
    // Tighter than browsing because it is the only public write here.
    let join_gov = GovernorConfigBuilder::default()
        .key_extractor(PeerIpOrLocalhost)
        .seconds_per_request(6)
        .burst_size(5)
        .finish()
        .expect("Invalid loyalty join rate limiter");
    let limited = rate_limiting_enabled();

    cfg
        // ── Staff: the teller's scan, and the admin's program ────────────
        .service(
            web::scope("/loyalty")
                .wrap(JwtMiddleware)
                .route("/settings", web::get().to(settings::get_settings))
                .route("/settings", web::put().to(settings::put_settings))
                .route("/settings", web::delete().to(settings::delete_settings))
                .route("/reward-items", web::get().to(settings::get_reward_items))
                .route("/reward-items", web::put().to(settings::put_reward_items))
                .route("/lookup", web::post().to(handlers::lookup))
                .route("/award", web::post().to(award::award))
                .route("/adjust", web::post().to(handlers::adjust))
                .route("/members", web::get().to(handlers::list_members))
                .route("/members/{id}", web::get().to(handlers::get_member)),
        )
        // ── Public: the counter QR's signup form and the customer's card ──
        .service(
            web::resource("/public/loyalty/join-info")
                .wrap(Condition::new(limited, Governor::new(&browse_gov)))
                .route(web::get().to(public::join_info)),
        )
        .service(
            web::resource("/public/loyalty/join")
                .wrap(Condition::new(limited, Governor::new(&join_gov)))
                .route(web::post().to(public::join)),
        )
        .service(
            web::resource("/public/loyalty/card/{token}")
                .wrap(Condition::new(limited, Governor::new(&browse_gov)))
                .route(web::get().to(public::card)),
        )
        .service(
            web::resource("/public/loyalty/card/{token}/qr.png")
                .wrap(Condition::new(limited, Governor::new(&browse_gov)))
                .route(web::get().to(public::card_qr)),
        )
        .service(
            web::resource("/public/loyalty/pass/{token}/apple.pkpass")
                .wrap(Condition::new(limited, Governor::new(&browse_gov)))
                .route(web::get().to(public::apple_pass)),
        );
}
