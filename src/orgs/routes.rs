use crate::{auth::middleware::JwtMiddleware, orgs::handlers, qr_card::handlers as qr_handlers};
use actix_web::web;

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/orgs")
            .wrap(JwtMiddleware)
            .route("", web::post().to(handlers::create_org))
            .route("", web::get().to(handlers::list_orgs))
            .route("/{id}", web::get().to(handlers::get_org))
            .route("/{id}", web::patch().to(handlers::update_org))
            .route("/{id}", web::delete().to(handlers::delete_org))
            .route("/{id}/logo", web::put().to(handlers::upload_org_logo))
            .route(
                "/{id}/offline-auth-bundle",
                web::get().to(handlers::offline_auth_bundle),
            )
            .route(
                "/{id}/onboarding",
                web::get().to(crate::orgs::onboarding::get_onboarding),
            )
            .route(
                "/{id}/onboarding/complete",
                web::post().to(crate::orgs::onboarding::complete_onboarding),
            )
            .route("/{id}/qr", web::get().to(qr_handlers::org_qr))
            .route(
                "/{id}/booking-qr",
                web::get().to(qr_handlers::org_booking_qr),
            ),
    );

    cfg.service(web::scope("/public/orgs").route("", web::get().to(handlers::list_public_orgs)));
}
