-- Loyalty / points program. Customers carry no app: their identity and balance
-- live in an Apple Wallet / Google Wallet pass, whose barcode is an opaque
-- member token. See LOYALTY_V1.md for the decisions behind this shape.
--
-- Two invariants are guarded in the database rather than in a handler, because
-- more than one code path writes points (live checkout, `/sync/replay` of an
-- offline till, an admin adjustment):
--   * a customer's balance is maintained by trigger from the ledger, so it can
--     never drift from the rows that explain it, whoever inserts them;
--   * one order can earn at most once (partial unique index), so replaying a
--     queued order from an offline till cannot double-award.

-- ── Program settings: org default + per-branch override ──────────────────────
-- Same scoping as `attendance_settings`: `branch_id IS NULL` is the org-wide
-- default and a branch row overrides it for that branch.
CREATE TABLE loyalty_settings (
    id                       uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id                   uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    branch_id                uuid REFERENCES branches(id) ON DELETE CASCADE,

    -- The program switch. A branch with no row inherits the org default.
    enabled                  boolean NOT NULL DEFAULT false,
    program_name             text NOT NULL DEFAULT 'Rewards',
    program_name_ar          text,

    -- WHAT a customer collects here. One or the other, never both: a card that
    -- counted two things at once would need two progress lines on the pass and
    -- two answers to "how close am I", and no counter wants that conversation.
    --   'points' — earned from money spent, by the rule below.
    --   'visits' — one stamp per sale, however large ("5 orders, free coffee").
    mode                     text NOT NULL DEFAULT 'points',

    -- Earn rule. Points are NEVER typed by a cashier: the server multiplies the
    -- order's own amounts by this rule when the order settles. One point per
    -- `earn_piastres_per_point` piastres — 1000 = a point per 10 EGP.
    earn_piastres_per_point  integer NOT NULL DEFAULT 1000,
    -- Earn on what the customer actually paid (subtotal - discount) rather than
    -- the pre-discount subtotal.
    earn_on_discounted       boolean NOT NULL DEFAULT true,
    -- Add tax to the basis. Off by default: tax is remitted, not revenue.
    earn_include_tax         boolean NOT NULL DEFAULT false,
    -- Tips are deliberately absent and never earn — that is the staff's money,
    -- not a sale.

    -- The cost offered by default when an admin adds a reward, in whatever this
    -- scope's `mode` collects. Each reward may override it, so one catalogue can
    -- hold "free espresso, 5 visits" beside "free cake, 10 visits". Also the
    -- pass's fallback target when no rewards have been curated yet.
    default_reward_cost      integer NOT NULL DEFAULT 100,

    -- Signup OTP, mirroring `branch_booking_settings.require_otp`. Reuses the
    -- existing `delivery_otp` table and the 90-day device-trust token.
    require_otp              boolean NOT NULL DEFAULT true,

    -- Wallet pass branding, per org (one Madar-owned issuer serves every
    -- tenant, so the tenant's identity has to come from data).
    pass_background_color    text,
    pass_foreground_color    text,
    pass_label_color         text,
    pass_logo_url            text,
    terms                    text,
    terms_ar                 text,

    created_at               timestamptz NOT NULL DEFAULT now(),
    updated_at               timestamptz NOT NULL DEFAULT now(),

    CONSTRAINT loyalty_settings_rate_pos      CHECK (earn_piastres_per_point > 0),
    CONSTRAINT loyalty_settings_threshold_pos CHECK (default_reward_cost > 0),
    CONSTRAINT loyalty_settings_mode          CHECK (mode IN ('points', 'visits')),
    -- A branch row must belong to the same org as the branch it overrides; the
    -- handler sets both, this stops a mismatched pair from ever landing.
    CONSTRAINT loyalty_settings_colors_hex CHECK (
        (pass_background_color IS NULL OR pass_background_color ~ '^#[0-9A-Fa-f]{6}$') AND
        (pass_foreground_color IS NULL OR pass_foreground_color ~ '^#[0-9A-Fa-f]{6}$') AND
        (pass_label_color      IS NULL OR pass_label_color      ~ '^#[0-9A-Fa-f]{6}$')
    )
);
-- One row per scope. The COALESCE trick is how `attendance_settings` makes a
-- nullable branch participate in a unique key.
CREATE UNIQUE INDEX loyalty_settings_scope_key ON loyalty_settings (
    org_id,
    COALESCE(branch_id, '00000000-0000-0000-0000-000000000000'::uuid)
);
ALTER TABLE loyalty_settings ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON loyalty_settings FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));
GRANT ALL ON TABLE loyalty_settings TO sufrix;

-- ── The reward catalogue ─────────────────────────────────────────────────────
-- Which menu items a customer may claim at the threshold. Scoped exactly like
-- the settings: a branch with no rows of its own inherits the org's list, so an
-- org can curate one catalogue and a branch can depart from it.
--
-- Item selection governs REDEMPTION only. Nothing item-level is consulted when
-- earning — points come from the order's totals — so the till never needs to
-- know which lines were eligible.
CREATE TABLE loyalty_reward_items (
    id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id        uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    branch_id     uuid REFERENCES branches(id) ON DELETE CASCADE,
    menu_item_id  uuid NOT NULL REFERENCES menu_items(id) ON DELETE CASCADE,
    -- What this particular reward costs, and in which currency. Per item so one
    -- catalogue can price a coffee at 5 visits and a cake at 10 — the thing a
    -- single program-wide threshold could not express.
    cost_currency text    NOT NULL DEFAULT 'points',
    cost_amount   integer NOT NULL DEFAULT 100,
    sort_order    integer NOT NULL DEFAULT 0,
    created_at    timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT loyalty_reward_items_currency CHECK (cost_currency IN ('points', 'visits')),
    CONSTRAINT loyalty_reward_items_cost_pos CHECK (cost_amount > 0)
);
CREATE UNIQUE INDEX loyalty_reward_items_scope_key ON loyalty_reward_items (
    org_id,
    COALESCE(branch_id, '00000000-0000-0000-0000-000000000000'::uuid),
    menu_item_id
);
CREATE INDEX idx_loyalty_reward_items_org ON loyalty_reward_items (org_id);
ALTER TABLE loyalty_reward_items ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON loyalty_reward_items FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));
GRANT ALL ON TABLE loyalty_reward_items TO sufrix;

-- ── Members ──────────────────────────────────────────────────────────────────
-- The first real customer record in the schema: everywhere else (delivery
-- orders, bookings, open tickets) name and phone are denormalised onto the row.
-- One balance per ORG — a member earns and spends at every branch of a tenant.
CREATE TABLE loyalty_customers (
    id                uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id            uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    -- Normalised by `delivery::normalize_phone`, the same way ordering and
    -- bookings normalise theirs, so one person is one member.
    phone             text NOT NULL,
    name              text NOT NULL,
    -- What the pass barcode encodes. An opaque 32-byte random token, NOT the
    -- id: a member QR must not be guessable or enumerable from another's.
    member_token      text NOT NULL,
    -- Both maintained by trigger from `loyalty_transactions`. Never write
    -- directly. A member carries both because an org may switch mode, or run
    -- points at one branch and stamps at another — the balance they earned under
    -- the old rules is still theirs.
    points_balance    integer NOT NULL DEFAULT 0,
    visits_balance    integer NOT NULL DEFAULT 0,
    -- Lifetime earned, for tiering and reports later. Also trigger-maintained.
    lifetime_points   integer NOT NULL DEFAULT 0,
    lifetime_visits   integer NOT NULL DEFAULT 0,
    -- The branch whose counter QR recruited them. Reporting only.
    joined_branch_id  uuid REFERENCES branches(id) ON DELETE SET NULL,
    -- Pass language: 'en' or 'ar'.
    locale            text NOT NULL DEFAULT 'en',

    -- Wallet plumbing, all NULL until credentials are configured (pass issuing
    -- is degrade-safe and skipped when the env vars are unset).
    apple_serial      text,
    -- Bearer token Apple's pass web service authenticates updates with.
    apple_auth_token  text,
    google_object_id  text,
    pass_updated_at   timestamptz,

    enrolled_at       timestamptz NOT NULL DEFAULT now(),
    updated_at        timestamptz NOT NULL DEFAULT now(),
    deleted_at        timestamptz,

    CONSTRAINT loyalty_customers_balance_nonneg  CHECK (points_balance  >= 0),
    CONSTRAINT loyalty_customers_visits_nonneg   CHECK (visits_balance  >= 0),
    CONSTRAINT loyalty_customers_lifetime_nonneg CHECK (lifetime_points >= 0),
    CONSTRAINT loyalty_customers_lifetime_visits CHECK (lifetime_visits >= 0),
    CONSTRAINT loyalty_customers_locale CHECK (locale IN ('en', 'ar'))
);
-- One member per phone per tenant. Partial so a deleted member frees the phone.
CREATE UNIQUE INDEX loyalty_customers_org_phone_key
    ON loyalty_customers (org_id, phone) WHERE deleted_at IS NULL;
-- The scan path. Globally unique: a token identifies a member on its own, with
-- no org supplied, because that is all a scanned QR carries.
CREATE UNIQUE INDEX loyalty_customers_token_key ON loyalty_customers (member_token);
CREATE UNIQUE INDEX loyalty_customers_apple_serial_key
    ON loyalty_customers (apple_serial) WHERE apple_serial IS NOT NULL;
CREATE INDEX idx_loyalty_customers_org ON loyalty_customers (org_id) WHERE deleted_at IS NULL;
ALTER TABLE loyalty_customers ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON loyalty_customers FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));
GRANT ALL ON TABLE loyalty_customers TO sufrix;

-- ── The points ledger ────────────────────────────────────────────────────────
CREATE TYPE loyalty_txn_kind AS ENUM ('earn', 'redeem', 'adjust');

CREATE TABLE loyalty_transactions (
    id                  uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id              uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    customer_id         uuid NOT NULL REFERENCES loyalty_customers(id) ON DELETE CASCADE,
    branch_id           uuid NOT NULL REFERENCES branches(id) ON DELETE CASCADE,
    kind                loyalty_txn_kind NOT NULL,
    -- Which balance this row moves. A row moves exactly one.
    currency            text NOT NULL DEFAULT 'points',
    -- Signed, in `currency`: positive earns, negative redeems. An adjustment may
    -- be either.
    points              integer NOT NULL,

    -- Earn provenance. `basis_piastres` is the amount the rule was applied to
    -- and the rate it used, snapshotted so re-tuning the rule later never
    -- rewrites what a past visit was worth.
    order_id            uuid REFERENCES orders(id) ON DELETE SET NULL,
    basis_piastres      integer,
    rate_piastres_per_point integer,

    -- Redemption provenance: which item was handed over, and which line of the
    -- order it covered. The line index makes each redemption on a mixed order
    -- distinct, so "two free coffees on one bill" is two rows that cannot
    -- collapse into one on a retry.
    reward_menu_item_id uuid REFERENCES menu_items(id) ON DELETE SET NULL,
    order_line_index    integer,

    -- The teller or admin responsible. NULL when the system awarded it.
    created_by          uuid REFERENCES users(id) ON DELETE SET NULL,
    note                text,
    created_at          timestamptz NOT NULL DEFAULT now(),

    CONSTRAINT loyalty_txn_currency CHECK (currency IN ('points', 'visits')),
    CONSTRAINT loyalty_txn_points_nonzero CHECK (points <> 0),
    CONSTRAINT loyalty_txn_earn_positive  CHECK (kind <> 'earn'   OR points > 0),
    CONSTRAINT loyalty_txn_redeem_negative CHECK (kind <> 'redeem' OR points < 0)
);
-- Idempotency, and the reason an offline till's replayed order cannot award
-- twice: an order earns at most once, whichever path inserts the row.
CREATE UNIQUE INDEX loyalty_transactions_earn_order_key
    ON loyalty_transactions (order_id) WHERE kind = 'earn' AND order_id IS NOT NULL;
-- One redemption per covered line per order: the idempotency that lets a retried
-- or replayed checkout land the same free coffee exactly once.
CREATE UNIQUE INDEX loyalty_transactions_redeem_line_key
    ON loyalty_transactions (order_id, order_line_index)
    WHERE kind = 'redeem' AND order_id IS NOT NULL AND order_line_index IS NOT NULL;
CREATE INDEX idx_loyalty_txn_customer ON loyalty_transactions (customer_id, created_at DESC);
CREATE INDEX idx_loyalty_txn_branch   ON loyalty_transactions (branch_id, created_at DESC);
ALTER TABLE loyalty_transactions ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON loyalty_transactions FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));
GRANT ALL ON TABLE loyalty_transactions TO sufrix;

-- Balance follows the ledger, always. Handlers insert a transaction and read
-- the customer back; nothing updates `points_balance` by hand. The CHECK on the
-- customer row turns an over-redemption into a failed transaction rather than a
-- negative balance.
CREATE OR REPLACE FUNCTION loyalty_apply_txn() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    UPDATE loyalty_customers
       SET points_balance  = points_balance
             + CASE WHEN NEW.currency = 'points' THEN NEW.points ELSE 0 END,
           visits_balance  = visits_balance
             + CASE WHEN NEW.currency = 'visits' THEN NEW.points ELSE 0 END,
           lifetime_points = lifetime_points
             + CASE WHEN NEW.currency = 'points' THEN GREATEST(NEW.points, 0) ELSE 0 END,
           lifetime_visits = lifetime_visits
             + CASE WHEN NEW.currency = 'visits' THEN GREATEST(NEW.points, 0) ELSE 0 END,
           updated_at      = now()
     WHERE id = NEW.customer_id;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'loyalty customer % not found', NEW.customer_id;
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER loyalty_apply_txn AFTER INSERT ON loyalty_transactions
    FOR EACH ROW EXECUTE FUNCTION loyalty_apply_txn();

-- ── Apple Wallet device registrations ────────────────────────────────────────
-- Apple pushes a pass update by notifying every device registered for that
-- pass; the device then calls back for the new .pkpass. Google needs none of
-- this (an update is a PATCH to the Wallet API), so this table is Apple-only.
CREATE TABLE loyalty_pass_devices (
    device_library_id  text NOT NULL,
    customer_id        uuid NOT NULL REFERENCES loyalty_customers(id) ON DELETE CASCADE,
    org_id             uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    push_token         text NOT NULL,
    created_at         timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (device_library_id, customer_id)
);
CREATE INDEX idx_loyalty_pass_devices_customer ON loyalty_pass_devices (customer_id);
ALTER TABLE loyalty_pass_devices ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON loyalty_pass_devices FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));
GRANT ALL ON TABLE loyalty_pass_devices TO sufrix;

-- ── The order link ───────────────────────────────────────────────────────────
-- Set when a teller scans a member's pass during checkout. The ledger's
-- `order_id` records what was awarded; this records that a member was attached
-- even when the sale rounded down to zero points.
ALTER TABLE orders ADD COLUMN loyalty_customer_id uuid
    REFERENCES loyalty_customers(id) ON DELETE SET NULL;
CREATE INDEX idx_orders_loyalty_customer ON orders (loyalty_customer_id)
    WHERE loyalty_customer_id IS NOT NULL;

-- A line a reward paid for. Priced at zero on the order, but marked so the
-- receipt can say REWARD rather than showing a mystery free item, and so
-- reporting can separate goods given away from goods discounted.
ALTER TABLE order_items ADD COLUMN is_reward boolean NOT NULL DEFAULT false;

-- Permission resource for the admin and teller surfaces. Seeded per role on boot.
ALTER TYPE permission_resource ADD VALUE IF NOT EXISTS 'loyalty';
