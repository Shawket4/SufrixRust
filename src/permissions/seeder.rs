use sqlx::PgPool;

/// Seed the default role permissions table on startup.
///
/// Uses ON CONFLICT DO NOTHING so any customisations made via the API
/// (`PUT /permissions/roles`) survive server restarts. Rows are only
/// inserted when they don't already exist — i.e. this is a first-run
/// initialiser, not a reset. To reset a role to defaults, delete the
/// rows in role_permissions for that role and restart.
pub async fn seed_role_permissions(pool: &PgPool) -> Result<(), sqlx::Error> {
    // (role, resource, action, granted)
    let defaults: &[(&str, &str, &str, bool)] = &[
        // ── org_admin: full access to everything ──────────────────
        // (generated: all resources × all actions = true)
        ("org_admin", "orgs", "create", true),
        ("org_admin", "orgs", "read", true),
        ("org_admin", "orgs", "update", true),
        ("org_admin", "orgs", "delete", true),
        ("org_admin", "branches", "create", true),
        ("org_admin", "branches", "read", true),
        ("org_admin", "branches", "update", true),
        ("org_admin", "branches", "delete", true),
        ("org_admin", "users", "create", true),
        ("org_admin", "users", "read", true),
        ("org_admin", "users", "update", true),
        ("org_admin", "users", "delete", true),
        ("org_admin", "categories", "create", true),
        ("org_admin", "categories", "read", true),
        ("org_admin", "categories", "update", true),
        ("org_admin", "categories", "delete", true),
        ("org_admin", "menu_items", "create", true),
        ("org_admin", "menu_items", "read", true),
        ("org_admin", "menu_items", "update", true),
        ("org_admin", "menu_items", "delete", true),
        ("org_admin", "addon_groups", "create", true),
        ("org_admin", "addon_groups", "read", true),
        ("org_admin", "addon_groups", "update", true),
        ("org_admin", "addon_groups", "delete", true),
        ("org_admin", "addon_items", "create", true),
        ("org_admin", "addon_items", "read", true),
        ("org_admin", "addon_items", "update", true),
        ("org_admin", "addon_items", "delete", true),
        ("org_admin", "recipes", "create", true),
        ("org_admin", "recipes", "read", true),
        ("org_admin", "recipes", "update", true),
        ("org_admin", "recipes", "delete", true),
        ("org_admin", "inventory", "create", true),
        ("org_admin", "inventory", "read", true),
        ("org_admin", "inventory", "update", true),
        ("org_admin", "inventory", "delete", true),
        ("org_admin", "inventory_transfers", "create", true),
        ("org_admin", "inventory_transfers", "read", true),
        ("org_admin", "inventory_transfers", "update", true),
        ("org_admin", "inventory_transfers", "delete", true),
        ("org_admin", "orders", "create", true),
        ("org_admin", "orders", "read", true),
        ("org_admin", "orders", "update", true),
        ("org_admin", "orders", "delete", true),
        ("org_admin", "order_items", "create", true),
        ("org_admin", "order_items", "read", true),
        ("org_admin", "order_items", "update", true),
        ("org_admin", "order_items", "delete", true),
        ("org_admin", "payments", "create", true),
        ("org_admin", "payments", "read", true),
        ("org_admin", "payments", "update", true),
        ("org_admin", "payments", "delete", true),
        ("org_admin", "payment_methods", "create", true),
        ("org_admin", "payment_methods", "read", true),
        ("org_admin", "payment_methods", "update", true),
        ("org_admin", "payment_methods", "delete", true),
        ("org_admin", "shifts", "create", true),
        ("org_admin", "shifts", "read", true),
        ("org_admin", "shifts", "update", true),
        ("org_admin", "shifts", "delete", true),
        ("org_admin", "stocktakes", "create", true),
        ("org_admin", "stocktakes", "read", true),
        ("org_admin", "stocktakes", "update", true),
        ("org_admin", "stocktakes", "delete", true),
        ("org_admin", "inventory_waste", "create", true),
        ("org_admin", "inventory_waste", "read", true),
        ("org_admin", "inventory_waste", "update", true),
        ("org_admin", "inventory_waste", "delete", true),
        ("org_admin", "suppliers", "create", true),
        ("org_admin", "suppliers", "read", true),
        ("org_admin", "suppliers", "update", true),
        ("org_admin", "suppliers", "delete", true),
        ("org_admin", "purchase_orders", "create", true),
        ("org_admin", "purchase_orders", "read", true),
        ("org_admin", "purchase_orders", "update", true),
        ("org_admin", "purchase_orders", "delete", true),
        ("org_admin", "soft_serve_batches", "create", true),
        ("org_admin", "soft_serve_batches", "read", true),
        ("org_admin", "soft_serve_batches", "update", true),
        ("org_admin", "soft_serve_batches", "delete", true),
        ("org_admin", "discounts", "create", true),
        ("org_admin", "discounts", "read", true),
        ("org_admin", "discounts", "update", true),
        ("org_admin", "discounts", "delete", true),
        ("org_admin", "reports", "read", true),
        ("org_admin", "permissions", "create", true),
        ("org_admin", "permissions", "read", true),
        ("org_admin", "permissions", "update", true),
        ("org_admin", "permissions", "delete", true),
        ("org_admin", "delivery_settings", "create", true),
        ("org_admin", "delivery_settings", "read", true),
        ("org_admin", "delivery_settings", "update", true),
        ("org_admin", "delivery_settings", "delete", true),
        ("org_admin", "delivery_orders", "create", true),
        ("org_admin", "delivery_orders", "read", true),
        ("org_admin", "delivery_orders", "update", true),
        ("org_admin", "delivery_orders", "delete", true),
        // ── branch_manager: operational access, no org-level management ─
        ("branch_manager", "branches", "read", true),
        ("branch_manager", "users", "create", true),
        ("branch_manager", "users", "read", true),
        ("branch_manager", "users", "update", true),
        ("branch_manager", "categories", "read", true),
        ("branch_manager", "menu_items", "read", true),
        ("branch_manager", "addon_groups", "read", true),
        ("branch_manager", "addon_items", "read", true),
        ("branch_manager", "recipes", "read", true),
        ("branch_manager", "inventory", "read", true),
        ("branch_manager", "inventory", "update", true),
        ("branch_manager", "inventory_transfers", "create", true),
        ("branch_manager", "inventory_transfers", "read", true),
        ("branch_manager", "inventory_transfers", "update", true),
        ("branch_manager", "orders", "create", true),
        ("branch_manager", "orders", "read", true),
        ("branch_manager", "orders", "update", true),
        ("branch_manager", "order_items", "create", true),
        ("branch_manager", "order_items", "read", true),
        ("branch_manager", "order_items", "update", true),
        ("branch_manager", "payments", "create", true),
        ("branch_manager", "payments", "read", true),
        ("branch_manager", "payments", "update", true),
        ("branch_manager", "payment_methods", "read", true),
        ("branch_manager", "shifts", "create", true),
        ("branch_manager", "shifts", "read", true),
        ("branch_manager", "shifts", "update", true),
        ("branch_manager", "stocktakes", "create", true),
        ("branch_manager", "stocktakes", "read", true),
        ("branch_manager", "stocktakes", "update", true),
        ("branch_manager", "inventory_waste", "create", true),
        ("branch_manager", "inventory_waste", "read", true),
        ("branch_manager", "suppliers", "read", true),
        ("branch_manager", "purchase_orders", "create", true),
        ("branch_manager", "purchase_orders", "read", true),
        ("branch_manager", "purchase_orders", "update", true),
        ("branch_manager", "soft_serve_batches", "create", true),
        ("branch_manager", "soft_serve_batches", "read", true),
        ("branch_manager", "soft_serve_batches", "update", true),
        ("branch_manager", "discounts", "read", true),
        ("branch_manager", "discounts", "update", true),
        ("branch_manager", "reports", "read", true),
        ("branch_manager", "delivery_settings", "create", true),
        ("branch_manager", "delivery_settings", "read", true),
        ("branch_manager", "delivery_settings", "update", true),
        ("branch_manager", "delivery_settings", "delete", true),
        ("branch_manager", "delivery_orders", "read", true),
        ("branch_manager", "delivery_orders", "update", true),
        // ── teller: POS-level access only ─────────────────────────
        ("teller", "branches", "read", true),
        ("teller", "categories", "read", true),
        ("teller", "menu_items", "read", true),
        ("teller", "addon_groups", "read", true),
        ("teller", "addon_items", "read", true),
        ("teller", "inventory", "read", true),
        ("teller", "orders", "create", true),
        ("teller", "orders", "read", true),
        ("teller", "order_items", "create", true),
        ("teller", "order_items", "read", true),
        ("teller", "payments", "create", true),
        ("teller", "payments", "read", true),
        ("teller", "payment_methods", "read", true),
        ("teller", "orders", "update", true), // needed for void_order
        ("teller", "shifts", "create", true),
        ("teller", "shifts", "read", true),
        ("teller", "shifts", "update", true), // covers cash movements
        ("teller", "discounts", "read", true),
        // Delivery queue: tellers work it (confirm/status/finalize/cancel) and
        // flip the POS open/close override. They cannot manage delivery_settings
        // (fees/hours/enable) — that stays with managers.
        ("teller", "delivery_orders", "read", true),
        ("teller", "delivery_orders", "update", true),
        // KDS + open tickets: a teller on a KDS/till device reads + bumps the
        // kitchen feed and settles waiter tickets into their drawer.
        //
        // `create` too, because a dine-in order rung up at the till has to
        // become an OPEN TICKET like the waiter's. It used to become a parked
        // draft instead — and a parked draft is device-local, so the table it
        // sat at stayed `free` server-side: invisible to the dashboard's floor,
        // and still offered to a guest booking that slot (`occupied_now` reads
        // `branch_tables.status`). An occupied table has to say so out loud.
        ("teller", "kitchen_orders", "read", true),
        ("teller", "kitchen_orders", "update", true),
        ("teller", "open_tickets", "create", true),
        ("teller", "open_tickets", "read", true),
        ("teller", "open_tickets", "update", true),
        // ── kitchen station + routing config (managers) ───────────
        ("org_admin", "kitchen_stations", "create", true),
        ("org_admin", "kitchen_stations", "read", true),
        ("org_admin", "kitchen_stations", "update", true),
        ("org_admin", "kitchen_stations", "delete", true),
        ("org_admin", "kitchen_orders", "read", true),
        ("org_admin", "kitchen_orders", "update", true),
        ("org_admin", "open_tickets", "read", true),
        ("org_admin", "open_tickets", "create", true),
        ("org_admin", "open_tickets", "update", true),
        ("branch_manager", "kitchen_stations", "create", true),
        ("branch_manager", "kitchen_stations", "read", true),
        ("branch_manager", "kitchen_stations", "update", true),
        ("branch_manager", "kitchen_stations", "delete", true),
        ("branch_manager", "kitchen_orders", "read", true),
        ("branch_manager", "kitchen_orders", "update", true),
        ("branch_manager", "open_tickets", "read", true),
        ("branch_manager", "open_tickets", "update", true),
        // ── waiter: menu reads + open-ticket fire (no shift/cash) ──
        ("waiter", "branches", "read", true),
        ("waiter", "categories", "read", true),
        ("waiter", "menu_items", "read", true),
        ("waiter", "addon_groups", "read", true),
        ("waiter", "addon_items", "read", true),
        ("waiter", "discounts", "read", true),
        ("waiter", "open_tickets", "create", true),
        ("waiter", "open_tickets", "read", true),
        ("waiter", "open_tickets", "update", true),
        ("waiter", "kitchen_orders", "read", true),
        // Kitchen Display role: read the feed + bump lines, and read stations (to
        // resolve the device's station). NOTHING on the POS/cash/ticket side.
        ("kitchen", "kitchen_orders", "read", true),
        ("kitchen", "kitchen_orders", "update", true),
        ("kitchen", "kitchen_stations", "read", true),
        // ── reservations + floor plan ─────────────────────────────
        // floor_plan = section/table geometry authoring (dashboard, managers).
        // reservations = booking host ops + live table status (host/teller).
        ("org_admin", "floor_plan", "create", true),
        ("org_admin", "floor_plan", "read", true),
        ("org_admin", "floor_plan", "update", true),
        ("org_admin", "floor_plan", "delete", true),
        ("branch_manager", "floor_plan", "create", true),
        ("branch_manager", "floor_plan", "read", true),
        ("branch_manager", "floor_plan", "update", true),
        ("branch_manager", "floor_plan", "delete", true),
        // Teller is the host: seats/moves/arrives/notifies, reads the floor.
        ("teller", "floor_plan", "read", true),
        // Waiter sees the board, and works table STATE (bussing a dirty
        // table, moving a physical table between zones — the host-op gate on
        // PATCH /floor/tables/{id}/state and its replay op).
        ("waiter", "floor_plan", "read", true),
        // ── staff / attendance / payroll ──────────────────────────
        // Being an employee is a `staff_profiles` row, not a role, so nothing is
        // granted here to make someone staff. These gate the ADMIN surface only;
        // `/staff/me/*` is own-row scoped and needs no permission, which is what
        // lets a teller clock in and read their own payslip while seeing nobody
        // else's salary.
        ("org_admin", "staff", "create", true),
        ("org_admin", "staff", "read", true),
        ("org_admin", "staff", "update", true),
        ("org_admin", "staff", "delete", true),
        ("org_admin", "work_shifts", "create", true),
        ("org_admin", "work_shifts", "read", true),
        ("org_admin", "work_shifts", "update", true),
        ("org_admin", "work_shifts", "delete", true),
        ("org_admin", "attendance", "create", true),
        ("org_admin", "attendance", "read", true),
        ("org_admin", "attendance", "update", true),
        ("org_admin", "attendance", "delete", true),
        ("org_admin", "leave", "create", true),
        ("org_admin", "leave", "read", true),
        ("org_admin", "leave", "update", true),
        ("org_admin", "leave", "delete", true),
        ("org_admin", "payroll", "create", true),
        ("org_admin", "payroll", "read", true),
        ("org_admin", "payroll", "update", true),
        ("org_admin", "payroll", "delete", true),
        // Branch manager runs the roster and approves requests, and may correct
        // an attendance record. Deliberately NOT granted `payroll` (salaries stay
        // with the owner) nor `staff` create/delete (hiring is an org decision);
        // `staff` read is needed to see who is on the roster at all.
        ("branch_manager", "staff", "read", true),
        ("branch_manager", "work_shifts", "read", true),
        ("branch_manager", "work_shifts", "update", true),
        ("branch_manager", "attendance", "create", true),
        ("branch_manager", "attendance", "read", true),
        ("branch_manager", "attendance", "update", true),
        ("branch_manager", "leave", "read", true),
        ("branch_manager", "leave", "update", true),
        // ── transfer waitlist ─────────────────────────────────────
        // Tellers work the whole queue; waiters queue and move their own
        // tickets. There is no `held_orders` resource: a parked order is a
        // client-local draft with no server presence to permit.
        ("org_admin", "table_transfers", "create", true),
        ("org_admin", "table_transfers", "read", true),
        ("org_admin", "table_transfers", "update", true),
        ("org_admin", "table_transfers", "delete", true),
        ("branch_manager", "table_transfers", "create", true),
        ("branch_manager", "table_transfers", "read", true),
        ("branch_manager", "table_transfers", "update", true),
        ("teller", "table_transfers", "create", true),
        ("teller", "table_transfers", "read", true),
        ("teller", "table_transfers", "update", true),
        ("waiter", "table_transfers", "create", true),
        ("waiter", "table_transfers", "read", true),
        ("waiter", "table_transfers", "update", true),
        // ── bookings (floor-layer reservations) ───────────────────
        // Managers author and operate; tellers and waiters read the day's
        // arrivals and operate at service time (seat / no-show) — never create
        // from the POS in v1. The KDS has no business with bookings.
        ("org_admin", "bookings", "create", true),
        ("org_admin", "bookings", "read", true),
        ("org_admin", "bookings", "update", true),
        ("org_admin", "bookings", "delete", true),
        ("branch_manager", "bookings", "create", true),
        ("branch_manager", "bookings", "read", true),
        ("branch_manager", "bookings", "update", true),
        ("branch_manager", "bookings", "delete", true),
        ("teller", "bookings", "read", true),
        ("teller", "bookings", "update", true),
        ("waiter", "bookings", "read", true),
        ("waiter", "bookings", "update", true),
        // ── loyalty (points program) ──────────────────────────────
        // Admins author the program and may correct a balance by hand (which
        // `handlers::adjust` additionally gates on the admin roles, since
        // `permission_action` has no rung above `update`). Tellers identify a
        // member and redeem — they never award points, because earning is
        // computed server-side from the order. Waiters and the KDS have no part
        // in it: the scan happens at checkout, which is the teller's screen.
        ("org_admin", "loyalty", "create", true),
        ("org_admin", "loyalty", "read", true),
        ("org_admin", "loyalty", "update", true),
        ("org_admin", "loyalty", "delete", true),
        ("branch_manager", "loyalty", "create", true),
        ("branch_manager", "loyalty", "read", true),
        ("branch_manager", "loyalty", "update", true),
        ("teller", "loyalty", "read", true),
        ("teller", "loyalty", "update", true),
    ];

    for &(role, resource, action, granted) in defaults {
        sqlx::query(
            r#"
            INSERT INTO role_permissions (role, resource, action, granted)
            VALUES ($1::user_role, $2::permission_resource, $3::permission_action, $4)
            ON CONFLICT (role, resource, action) DO NOTHING
            "#,
        )
        .bind(role)
        .bind(resource)
        .bind(action)
        .bind(granted)
        .execute(pool)
        .await?;
    }

    Ok(())
}
