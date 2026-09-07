use crate::{
    auth::middleware::JwtMiddleware, branches::handlers, qr_card::handlers as qr_handlers,
};
use actix_web::web;

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/branches")
            .wrap(JwtMiddleware)
            .route("", web::get().to(handlers::list_branches))
            .route("", web::post().to(handlers::create_branch))
            .route("/{id}", web::get().to(handlers::get_branch))
            .route("/{id}", web::put().to(handlers::update_branch))
            .route("/{id}", web::delete().to(handlers::delete_branch))
            // QR
            .route("/{id}/qr", web::get().to(qr_handlers::branch_qr))
            .route(
                "/{id}/booking-qr",
                web::get().to(qr_handlers::branch_booking_qr),
            )
            // Tables
            .route("/{id}/tables", web::get().to(qr_handlers::list_tables))
            .route("/{id}/tables", web::post().to(qr_handlers::create_table))
            .route(
                "/{id}/tables/{tid}",
                web::delete().to(qr_handlers::delete_table),
            )
            .route(
                "/{id}/tables/{tid}/qr",
                web::get().to(qr_handlers::table_qr),
            ),
    );

    // Controlled timezone vocabulary (the timezone_name enum) for the dashboard
    // select. Authenticated, no specific permission — it's static config.
    cfg.service(
        web::scope("/timezones")
            .wrap(JwtMiddleware)
            .route("", web::get().to(handlers::list_timezones)),
    );
}
