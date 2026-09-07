//! Madar backend library crate.
//!
//! All modules live here so they can be shared between the main server
//! binary (`src/main.rs`) and ancillary binaries like the OpenAPI
//! exporter (`src/bin/export_openapi.rs`). The previous `main.rs` owned
//! these module declarations directly; moving them to the library lets
//! `cargo run --bin export-openapi` reach `ApiDoc` without spinning up
//! the HTTP server.

pub mod ai;
pub mod analytics;
pub mod auth;
pub mod bookings;
pub mod branches;
pub mod bundles;
pub mod cache;
pub mod clock;
pub mod costing;
pub mod db;
pub mod delivery;
pub mod demo;
pub mod discounts;
pub mod errors;
pub mod floor_ops;
pub mod geo;
pub mod insights;
pub mod integrations;
pub mod inventory;
pub mod kitchen;
pub mod loyalty;
pub mod menu;
pub mod menu_unification;
pub mod models;
pub mod observability;
pub mod openapi;
pub mod orders;
pub mod orgs;
pub mod payment_methods;
pub mod permissions;
pub mod purchasing;
pub mod qr_card;
pub mod rate_limit;
pub mod realtime;
pub mod recipes;
pub mod reports;
pub mod reservations;
pub mod shifts;
pub mod staff;
pub mod stocktakes;
pub mod sync;
pub mod tickets;
pub mod tills;
pub mod translation;
pub mod units;
pub mod uploads;
pub mod users;

#[cfg(test)]
pub mod e2e_tests;

#[cfg(test)]
pub mod rls_tests;
