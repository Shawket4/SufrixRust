use actix_web::web;

use crate::floor_ops::handlers;

/// Cross-table floor operations, registered INTO the one `/floor` scope that
/// `reservations::routes` owns — deliberately not a scope of its own.
///
/// actix routes a path prefix to the first scope that matches and never falls
/// through to a later one, so a second `web::scope("/floor")` here would be
/// dead: every route below would 404 (or 405, where the geometry scope has a
/// same-shaped route for another method) no matter how it was ordered in
/// `main.rs`. It was, for a while. `floor_ops::tests::the_whole_floor_scope_is_reachable`
/// is the guard.
///
/// A parked order is NOT here: it is a client-local draft, so parking, naming,
/// resuming and discarding one never touch the network. What remains is what is
/// genuinely shared across tills -- swapping two tables' occupants atomically,
/// and the transfer waitlist a host works.
///
/// There is deliberately no route that SETS a table's status. Status is derived
/// from the ticket on the table -- except `clear`, which performs the single
/// transition no server can observe: a bussed table becoming ready.
///
/// These must be registered BEFORE `/tables/{id}`: `{id}` matches the literal
/// `swap` too, and the first resource to match a path is the one that answers.
pub fn register(cfg: &mut web::ServiceConfig) {
    cfg.route("/tables/swap", web::post().to(handlers::swap_tables))
        // The one human act status cannot derive: "the plates are gone".
        .route("/tables/{id}/clear", web::post().to(handlers::clear_table))
        .route("/transfers", web::get().to(handlers::list_floor_transfers))
        .route("/transfers", web::post().to(handlers::create_floor_transfer))
        .route(
            "/transfers/{id}/cancel",
            web::post().to(handlers::cancel_transfer),
        )
        .route(
            "/transfers/{id}/fulfill",
            web::post().to(handlers::fulfill_transfer),
        );
}
